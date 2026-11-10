//! This machine's user configuration as the remote claude's Claude Code folder holds it (see
//! [`crate::helper::config`]): `~/.claude/settings.json`, adapted as the project's settings are,
//! and `~/.claude/keybindings.json` as it is. What a session saves there comes back here.
//!
//! The settings go without what belongs to this machine (its login, plugins and paths; see
//! [`crate::settings::user_part`]), and a save is applied as a change: what the session changed,
//! relative to what it was given, goes into the file as it is now. So this machine's own keys stay,
//! and an edit made here during the session is kept too, unless the session changed the same key.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::json::Object;
use crate::settings::{Callbacks, user_dir, user_part};

/// What the remote side gets for the Claude Code folder, as remote paths: the files sent at start
/// (when this machine has them), and the socket saves come back on; and the saves themselves, to
/// wait for at the end of the session.
pub struct Delivery {
    pub settings: Option<String>,
    pub keybindings: Option<String>,
    pub sink: String,
    pub saves: Arc<UserConfig>,
}

/// The files the remote claude finds in its Claude Code folder.
pub struct UserConfig {
    callbacks: Arc<Callbacks>,
    dir: PathBuf,
    /// The settings as the session last had them (what it was given, then what it saved), in this
    /// machine's terms; changes are taken relative to it.
    base: Mutex<Option<Object>>,
    /// The highest save applied per file: saves arrive one per connection, possibly out of order.
    applied: Mutex<std::collections::HashMap<String, u64>>,
    /// Saves on their way to disk, so the session does not end before they land.
    pending: tokio::sync::watch::Sender<usize>,
}

/// One save on its way; the count goes down when it is dropped.
pub struct Saving(pub(crate) Arc<UserConfig>);

impl Drop for Saving {
    fn drop(&mut self) {
        self.0.pending.send_modify(|n| *n -= 1);
    }
}

fn parse(bytes: &[u8]) -> Option<Object> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(o)) => Some(o),
        _ => None,
    }
}

/// Writes a file by replacing it, keeping its mode (0600 when new).
fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
    let tmp = dir.join(format!(".{}.claudette.{}", path.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)
}

/// `current` with the change from `base` to `written` applied. Keys are changed whole, except the
/// permission rule lists, whose added and removed rules are applied one by one, and the variables
/// in `env` and the other `permissions` entries, changed one by one.
pub fn apply_change(mut current: Object, base: &Object, written: &Object) -> Object {
    let keys: BTreeSet<&String> = base.keys().chain(written.keys()).collect();
    for key in keys {
        let (before, after) = (base.get(key), written.get(key));
        if before == after {
            continue;
        }
        let nested = matches!(key.as_str(), "permissions" | "env");
        match (nested, after) {
            (true, Some(Value::Object(after))) => {
                let empty = Object::new();
                let before = before.and_then(Value::as_object).unwrap_or(&empty);
                let mine = current.entry(key.clone()).or_insert_with(|| json!({}));
                if !mine.is_object() {
                    *mine = json!({});
                }
                let mine = mine.as_object_mut().expect("an object");
                for inner in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
                    let (b, a) = (before.get(inner), after.get(inner));
                    if b == a {
                        continue;
                    }
                    let is_rules = key == "permissions" && matches!(inner.as_str(), "allow" | "deny" | "ask");
                    match (is_rules, a) {
                        (true, Some(Value::Array(a))) => {
                            let b: Vec<&Value> = b.and_then(Value::as_array).map(|v| v.iter().collect()).unwrap_or_default();
                            let list = mine.entry(inner.clone()).or_insert_with(|| json!([]));
                            if !list.is_array() {
                                *list = json!([]);
                            }
                            let list = list.as_array_mut().expect("an array");
                            list.retain(|r| !b.contains(&r) || a.contains(r));
                            for rule in a {
                                if !b.contains(&rule) && !list.contains(rule) {
                                    list.push(rule.clone());
                                }
                            }
                        }
                        (_, Some(value)) => {
                            mine.insert(inner.clone(), value.clone());
                        }
                        (_, None) => {
                            mine.remove(inner);
                        }
                    }
                }
            }
            (_, Some(value)) => {
                current.insert(key.clone(), value.clone());
            }
            (_, None) => {
                current.remove(key);
            }
        }
    }
    current
}

impl UserConfig {
    pub fn new(callbacks: Arc<Callbacks>) -> UserConfig {
        UserConfig::in_dir(callbacks, user_dir())
    }

    fn in_dir(callbacks: Arc<Callbacks>, dir: PathBuf) -> UserConfig {
        UserConfig { callbacks, dir, base: Mutex::new(None), applied: Mutex::default(), pending: tokio::sync::watch::channel(0).0 }
    }

    /// Marks a save as on its way until the returned guard is dropped.
    pub fn saving(self: &Arc<Self>) -> Saving {
        self.pending.send_modify(|n| *n += 1);
        Saving(self.clone())
    }

    /// Waits until no save is on its way.
    pub async fn settled(&self) {
        let _ = self.pending.subscribe().wait_for(|n| *n == 0).await;
    }

    /// `settings.json` for the remote claude: this machine's, without what belongs to this machine,
    /// hooks and rules adapted. None when there is no such file; one that is not a JSON object passes
    /// unchanged, as claude would read it here.
    pub fn settings(&self) -> Option<Vec<u8>> {
        let bytes = std::fs::read(self.dir.join("settings.json")).ok()?;
        let Some(object) = parse(&bytes) else { return Some(bytes) };
        let given = user_part(object);
        *self.base.lock().unwrap() = Some(given.clone());
        let mut out = self.callbacks.view(&Value::Object(given).to_string().into_bytes());
        // Pretty, as Claude Code writes it.
        if let Some(viewed) = parse(&out) {
            out = serde_json::to_vec_pretty(&viewed).unwrap_or(out);
        }
        Some(out)
    }

    /// `keybindings.json`, as it is.
    pub fn keybindings(&self) -> Option<Vec<u8>> {
        std::fs::read(self.dir.join("keybindings.json")).ok()
    }

    /// A save from the remote claude: `name` is `settings.json` or `keybindings.json`, `seq` orders
    /// saves of one session. An older save than one already applied is dropped.
    pub fn save(&self, name: &str, seq: u64, bytes: &[u8]) -> std::io::Result<()> {
        {
            let mut applied = self.applied.lock().unwrap();
            if applied.get(name).is_some_and(|&done| done >= seq) {
                return Ok(());
            }
            applied.insert(name.to_owned(), seq);
        }
        let path = self.dir.join(name);
        match name {
            "keybindings.json" => replace(&path, bytes),
            "settings.json" => {
                let restored = self.callbacks.restore(bytes);
                let mut base = self.base.lock().unwrap();
                let (Some(written), Some(current)) = (parse(&restored), std::fs::read(&path).ok().map_or(Some(Object::new()), |b| parse(&b))) else {
                    // Not JSON objects: the session's file, as it wrote it.
                    return replace(&path, &restored);
                };
                let next = apply_change(current, base.as_ref().unwrap_or(&Object::new()), &written);
                *base = Some(written);
                let mut out = serde_json::to_vec_pretty(&next).unwrap_or_default();
                out.push(b'\n');
                replace(&path, &out)
            }
            _ => Err(std::io::Error::other(format!("not a configuration file: {name}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Object {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn a_save_applies_what_the_session_changed_and_keeps_the_rest() {
        let given = obj(json!({ "theme": "dark", "permissions": { "allow": ["Read"], "defaultMode": "default" }, "env": { "A": "1" } }));
        // Since the session started, this machine got a rule and a variable of its own, and it has its login.
        let current = obj(json!({ "theme": "dark", "apiKeyHelper": "/bin/key", "permissions": { "allow": ["Read", "Bash(make:*)"], "defaultMode": "default" }, "env": { "A": "1", "B": "local" } }));
        // The session switched the theme, allowed Write, removed Read, and dropped A.
        let written = obj(json!({ "theme": "light", "permissions": { "allow": ["Write"], "defaultMode": "default" }, "env": {} }));
        assert_eq!(
            Value::Object(apply_change(current, &given, &written)),
            json!({ "theme": "light", "apiKeyHelper": "/bin/key", "permissions": { "allow": ["Bash(make:*)", "Write"], "defaultMode": "default" }, "env": { "B": "local" } })
        );
    }

    #[test]
    fn the_settings_go_without_this_machines_keys_and_come_back_with_them() {
        let dir = std::env::temp_dir().join(format!("ct-userconfig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"apiKeyHelper":"/bin/key","env":{"ANTHROPIC_API_KEY":"secret","KEEP":"1"},"permissions":{"allow":["Write"]}}"#).unwrap();
        let uc = UserConfig::in_dir(Arc::new(Callbacks::new(&dir, "nc")), dir.clone());
        let given = String::from_utf8(uc.settings().unwrap()).unwrap();
        assert!(!given.contains("apiKeyHelper") && !given.contains("secret") && given.contains("KEEP"), "{given}");
        // claude sees the local tool's rule under its MCP name too, and saves what it sees.
        assert!(given.contains("mcp__cc__Write"), "{given}");
        let saved = given.replace("\"KEEP\": \"1\"", "\"KEEP\": \"2\"");
        uc.save("settings.json", 1, saved.as_bytes()).unwrap();
        let after: Value = serde_json::from_slice(&std::fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(after, json!({ "apiKeyHelper": "/bin/key", "env": { "ANTHROPIC_API_KEY": "secret", "KEEP": "2" }, "permissions": { "allow": ["Write"] } }));
        // A save older than one already applied changes nothing.
        uc.save("settings.json", 1, given.as_bytes()).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(dir.join("settings.json")).unwrap()).unwrap()["env"]["KEEP"], "2");
        uc.save("keybindings.json", 2, b"{\"bindings\":[]}").unwrap();
        assert_eq!(std::fs::read(dir.join("keybindings.json")).unwrap(), b"{\"bindings\":[]}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
