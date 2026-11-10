//! This machine's Claude Code plugins, for the remote claude.
//!
//! The session's plugins are the ones Claude Code would load here: installed in this machine's
//! plugin store (`~/.claude/plugins/installed_plugins.json`, for the user or for this project) or
//! named in `enabledPlugins`, and on by `enabledPlugins` (user, project and local settings as
//! `--setting-sources` loads them, the caller's `--settings`, then managed settings, which win), or
//! by the manifest's `defaultEnabled` (on unless it says `false`) when no setting names them. Each is
//! served read-only from its folder here (see [`crate::mount::sftp::Plugins`]) and loaded with
//! `--plugin-dir`, so its skills, agents, commands and output styles come from this machine. The
//! remote machine's own plugins stay unused.
//!
//! A plugin's hooks and MCP servers run here, where your files and credentials are: command hooks
//! become calls back to this machine, as the settings' hooks do (with `CLAUDE_PLUGIN_ROOT`,
//! `CLAUDE_PLUGIN_DATA` and `CLAUDE_PLUGIN_OPTION_<KEY>` as Claude Code sets them), and its MCP
//! servers join the session's servers that run here, named `plugin_<plugin>_<server>` so their
//! tools keep the names Claude Code gives them (`mcp__plugin_<plugin>_<server>__<tool>`).
//!
//! A plugin's options (`userConfig`) are the ones Claude Code saved here: `pluginConfigs` in the
//! user settings, the caller's `--settings` and managed settings, then the sensitive ones from
//! Claude Code's credential store (`pluginSecrets`), then each option's default. They fill
//! `${user_config.KEY}` in the plugin's MCP servers and exec-form hook arguments here; the
//! remote claude gets only the non-sensitive ones, for the skills and agents it reads.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use crate::args::ParsedArgs;
use crate::json::Object;
use crate::log::warn;
use crate::settings::{Callbacks, PluginEnv, SETTINGS_FILES, Sources, user_dir};

/// A plugin's manifest, relative to its folder.
pub const MANIFEST: &str = ".claude-plugin/plugin.json";
/// A plugin's default MCP server file.
pub const MCP_FILE: &str = ".mcp.json";
/// A plugin's default hooks file.
const HOOKS_FILE: &str = "hooks/hooks.json";

#[derive(Debug, Clone)]
pub struct Plugin {
    /// `name@marketplace`, as `enabledPlugins` names it.
    pub id: String,
    /// The manifest's `name`, which Claude Code names the plugin's MCP servers after.
    pub name: String,
    /// Its installed folder here, canonical.
    pub root: PathBuf,
    /// `${CLAUDE_PLUGIN_DATA}` here: `~/.claude/plugins/data/<id>`.
    pub data: PathBuf,
    /// Its options, every one with a value: (key, value as Claude Code passes it, sensitive).
    pub options: Vec<(String, String, bool)>,
}

/// `id` with every character other than a letter, digit, `_` or `-` replaced by `-`, as Claude Code
/// names a plugin's data folder.
fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect()
}

fn read_object(path: &Path) -> Object {
    match std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        Some(Value::Object(o)) => o,
        _ => Object::new(),
    }
}

/// A manifest path (`./config/hooks.json`) as a path inside the plugin, or None if it leaves it.
fn inside(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// An MCP entry that names a bundle or a URL rather than a JSON file or an inline map.
fn is_bundle(path: &str) -> bool {
    !path.ends_with(".json")
}

/// An option's value as Claude Code passes it to a process: `String(value)` in JavaScript.
fn option_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(option_text).collect::<Vec<_>>().join(","),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

/// `${user_config.KEY}` in `text`, from the plugin's options; an unknown key stays as written.
pub fn fill_options(text: &str, options: &[(String, String)]) -> String {
    if !text.contains("${user_config.") {
        return text.to_owned();
    }
    options.iter().fold(text.to_owned(), |text, (key, value)| text.replace(&format!("${{user_config.{key}}}"), value))
}

impl Plugin {
    /// Its folder's name in the mount.
    pub fn dir_name(&self) -> String {
        sanitize(&self.id)
    }

    pub fn env(&self) -> PluginEnv {
        PluginEnv { root: self.root.clone(), data: self.data.clone(), options: self.options.iter().map(|(k, v, _)| (k.clone(), v.clone())).collect() }
    }

    fn manifest(&self) -> Object {
        read_object(&self.root.join(MANIFEST))
    }

    /// The files that hold its hooks, relative to its folder: `hooks/hooks.json` and the ones its
    /// manifest names.
    pub fn hook_files(&self) -> Vec<PathBuf> {
        let mut files = vec![PathBuf::from(HOOKS_FILE)];
        let named = match self.manifest().get("hooks") {
            Some(Value::String(path)) => vec![path.clone()],
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
            _ => vec![],
        };
        files.extend(named.iter().filter_map(|p| inside(p)));
        files
    }

    /// Its MCP servers by name: `.mcp.json`, then what the manifest declares (JSON files and inline
    /// maps, in order; a later name replaces an earlier one). Bundles and URLs stay with the plugin.
    fn servers(&self) -> Object {
        let file = |path: &Path| {
            let mut object = read_object(&self.root.join(path));
            match object.remove("mcpServers") {
                Some(Value::Object(servers)) => servers,
                _ => object,
            }
        };
        let mut servers = file(Path::new(MCP_FILE));
        let mut declared = |entry: &Value| match entry {
            Value::String(path) if !is_bundle(path) => servers.extend(inside(path).map(|p| file(&p)).unwrap_or_default()),
            Value::Object(inline) => servers.extend(inline.clone()),
            _ => {}
        };
        match self.manifest().get("mcpServers") {
            Some(Value::Array(items)) => items.iter().for_each(&mut declared),
            Some(entry) => declared(entry),
            None => {}
        }
        servers.retain(|_, v| v.is_object());
        servers
    }

    /// The manifest as the remote claude reads it: inline hooks call back, and the MCP servers it
    /// declares are gone (they run here).
    pub fn manifest_view(&self, callbacks: &Callbacks) -> Vec<u8> {
        let Ok(bytes) = std::fs::read(self.root.join(MANIFEST)) else { return Vec::new() };
        let Ok(Value::Object(mut manifest)) = serde_json::from_slice::<Value>(&bytes) else { return bytes };
        let env = self.env();
        match manifest.get_mut("hooks") {
            Some(Value::Object(inline)) => *inline = callbacks.plugin_hooks(inline.clone(), &env),
            Some(Value::Array(items)) => {
                for item in items.iter_mut() {
                    if let Value::Object(inline) = item {
                        *inline = callbacks.plugin_hooks(inline.clone(), &env);
                    }
                }
            }
            _ => {}
        }
        let kept = match manifest.remove("mcpServers") {
            Some(Value::String(path)) if is_bundle(&path) => Some(Value::String(path)),
            Some(Value::Array(items)) => {
                let bundles: Vec<Value> = items.into_iter().filter(|v| v.as_str().is_some_and(is_bundle)).collect();
                (!bundles.is_empty()).then_some(Value::Array(bundles))
            }
            _ => None,
        };
        if let Some(kept) = kept {
            manifest.insert("mcpServers".into(), kept);
        }
        let mut out = serde_json::to_vec_pretty(&manifest).unwrap_or(bytes);
        out.push(b'\n');
        out
    }
}

/// Claude Code's managed settings files: `managed-settings.json`, then `managed-settings.d/*.json`
/// in name order.
fn managed_settings() -> Vec<Object> {
    let dir = if cfg!(target_os = "macos") { Path::new("/Library/Application Support/ClaudeCode") } else { Path::new("/etc/claude-code") };
    let mut files = vec![dir.join("managed-settings.json")];
    let mut dropins: Vec<PathBuf> = std::fs::read_dir(dir.join("managed-settings.d"))
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
        .unwrap_or_default();
    dropins.sort();
    files.extend(dropins);
    files.iter().map(|f| read_object(f)).filter(|o| !o.is_empty()).collect()
}

/// The settings that decide plugins, lowest precedence first, each with whether it may hold
/// options (Claude Code reads `pluginConfigs` from the user's, `--settings` and managed ones).
fn settings_layers(cwd: &Path, p: &ParsedArgs) -> Vec<(Object, bool)> {
    let sources = Sources::of(p);
    let mut layers = vec![];
    if sources.user {
        layers.push((read_object(&user_dir().join("settings.json")), true));
    }
    for (file, on) in SETTINGS_FILES.iter().zip([sources.project, sources.local]) {
        if on {
            layers.push((read_object(&cwd.join(file)), false));
        }
    }
    layers.extend(p.settings.clone().map(|s| (s, true)));
    layers.extend(managed_settings().into_iter().map(|s| (s, true)));
    layers
}

/// The installations of a plugin that apply here: the user's, and this project's.
fn installations<'a>(registry: &'a Object, id: &str, project: &Path) -> Vec<&'a Object> {
    let entries: Vec<&Object> = match registry.get("plugins").and_then(|p| p.get(id)) {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).collect(),
        Some(Value::Object(entry)) => vec![entry],
        _ => return vec![],
    };
    entries
        .into_iter()
        .filter(|e| match e.get("projectPath").and_then(Value::as_str) {
            Some(path) => Path::new(path) == project,
            None => true,
        })
        .collect()
}

/// Where a plugin is installed: this project's installation when there is one, else the user's.
fn install_path(registry: &Object, id: &str, project: &Path) -> Option<PathBuf> {
    let found = installations(registry, id, project);
    let chosen = found.iter().find(|e| e.get("projectPath").is_some()).or_else(|| found.first())?;
    chosen.get("installPath").and_then(Value::as_str).map(PathBuf::from)
}

/// Claude Code's credential store on this machine, as JSON: the Keychain entry on macOS (when the
/// Claude Code folder is the default one), else `.credentials.json` in it.
fn credentials() -> Object {
    if cfg!(target_os = "macos") && std::env::var_os("CLAUDE_CONFIG_DIR").is_none_or(|v| v.is_empty()) {
        let user = std::env::var("USER").unwrap_or_default();
        let out = std::process::Command::new("security").args(["find-generic-password", "-s", "Claude Code-credentials", "-a", &user, "-w"]).output();
        if let Ok(out) = out
            && out.status.success()
            && let Ok(Value::Object(store)) = serde_json::from_slice::<Value>(&out.stdout)
        {
            return store;
        }
    }
    read_object(&user_dir().join(".credentials.json"))
}

/// A plugin's options from what Claude Code saved: (key, value, sensitive) for each declared
/// option that has a value or a default. The credential store is read once, and only for a plugin
/// with a sensitive option.
fn options(id: &str, manifest: &Object, layers: &[(Object, bool)], secrets: &mut Option<Object>) -> Vec<(String, String, bool)> {
    let Some(Value::Object(declared)) = manifest.get("userConfig") else { return vec![] };
    let mut saved = Object::new();
    for (layer, _) in layers.iter().filter(|(_, holds)| *holds) {
        if let Some(Value::Object(options)) = layer.get("pluginConfigs").and_then(|c| c.get(id)).and_then(|c| c.get("options")) {
            saved.extend(options.clone());
        }
    }
    let sensitive = |key: &str| declared.get(key).and_then(|d| d.get("sensitive")).and_then(Value::as_bool) == Some(true);
    if declared.keys().any(|k| sensitive(k)) {
        let store = secrets.get_or_insert_with(credentials);
        if let Some(Value::Object(values)) = store.get("pluginSecrets").and_then(|s| s.get(id)) {
            saved.extend(values.clone());
        }
    }
    declared
        .iter()
        .filter_map(|(key, spec)| {
            let value = saved.get(key).or_else(|| spec.get("default"))?;
            Some((key.clone(), option_text(value), sensitive(key)))
        })
        .collect()
}

/// The plugins the session gets: on by the settings or by default, and installed here.
pub fn enabled(cwd: &Path, p: &ParsedArgs) -> Vec<Plugin> {
    let plugins_dir = user_dir().join("plugins");
    let registry = read_object(&plugins_dir.join("installed_plugins.json"));
    let project = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let layers = settings_layers(cwd, p);
    let mut explicit: BTreeMap<String, bool> = BTreeMap::new();
    for (layer, _) in &layers {
        if let Some(Value::Object(plugins)) = layer.get("enabledPlugins") {
            for (id, on) in plugins {
                explicit.insert(id.clone(), !matches!(on, Value::Bool(false) | Value::Null));
            }
        }
    }
    let installed = registry.get("plugins").and_then(Value::as_object).into_iter().flat_map(|m| m.keys()).filter(|id| !installations(&registry, id, &project).is_empty());
    let mut ids: Vec<String> = installed.cloned().chain(explicit.keys().cloned()).collect();
    ids.sort();
    ids.dedup();
    let mut secrets = None;
    ids.into_iter()
        .filter_map(|id| {
            let root = install_path(&registry, &id, &project).and_then(|r| r.canonicalize().ok()).filter(|r| r.is_dir());
            let manifest = root.as_ref().map(|r| read_object(&r.join(MANIFEST))).unwrap_or_default();
            let on = explicit.get(&id).copied().unwrap_or_else(|| manifest.get("defaultEnabled").and_then(Value::as_bool) != Some(false));
            if !on {
                return None;
            }
            let Some(root) = root else {
                warn(&format!("the plugin {id} is enabled but not installed on this machine: install it with `claude plugin install {id}`"));
                return None;
            };
            let name = manifest.get("name").and_then(Value::as_str).map_or_else(|| id.split('@').next().unwrap_or(&id).to_owned(), str::to_owned);
            let data = plugins_dir.join("data").join(sanitize(&id));
            let _ = std::fs::create_dir_all(&data);
            let options = options(&id, &manifest, &layers, &mut secrets);
            Some(Plugin { id, name, root, data, options })
        })
        .collect()
}

/// The plugin's placeholders in every string: `${CLAUDE_PLUGIN_ROOT}`, `${CLAUDE_PLUGIN_DATA}`,
/// `${CLAUDE_PROJECT_DIR}` and `${user_config.KEY}`.
fn place(value: &mut Value, plugin: &PluginEnv, project: &str) {
    match value {
        Value::String(s) if s.contains("${") => {
            let replaced = s.replace("${CLAUDE_PLUGIN_ROOT}", &plugin.root.to_string_lossy()).replace("${CLAUDE_PLUGIN_DATA}", &plugin.data.to_string_lossy()).replace("${CLAUDE_PROJECT_DIR}", project);
            *s = fill_options(&replaced, &plugin.options);
        }
        Value::Array(items) => items.iter_mut().for_each(|v| place(v, plugin, project)),
        Value::Object(map) => map.values_mut().for_each(|v| place(v, plugin, project)),
        _ => {}
    }
}

/// Adds the plugins' MCP servers to the ones that run here, beside the caller's own (which win).
/// With `--strict-mcp-config`, none are added, as Claude Code starts no plugin servers then.
pub fn add_servers(p: &mut ParsedArgs, plugins: &[Plugin], cwd: &Path) {
    if p.strict_mcp {
        return;
    }
    for plugin in plugins {
        let env = plugin.env();
        for (server, mut config) in plugin.servers() {
            let name = format!("plugin_{}_{server}", plugin.name);
            if p.config_servers.contains_key(&name) || p.remote_servers.contains_key(&name) {
                continue;
            }
            place(&mut config, &env, &cwd.to_string_lossy());
            crate::mcp::config::expand(&mut config);
            if let Value::Object(config) = config {
                p.config_servers.insert(name, config);
            }
        }
    }
}

/// The non-sensitive options as `--settings` gives them to the remote claude (`pluginConfigs`),
/// under the ids it gives `--plugin-dir` plugins (`<name>@inline`), for the skills and agents it
/// reads.
pub fn remote_options(plugins: &[Plugin]) -> Object {
    plugins
        .iter()
        .filter_map(|plugin| {
            let options: Object = plugin.options.iter().filter(|(_, _, sensitive)| !sensitive).map(|(k, v, _)| (k.clone(), json!(v))).collect();
            (!options.is_empty()).then(|| (format!("{}@inline", plugin.name), json!({ "options": options })))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(files: &[(&str, Value)]) -> Plugin {
        let root = std::env::temp_dir().join(format!("ct-plugin-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content.to_string()).unwrap();
        }
        let root = root.canonicalize().unwrap();
        Plugin { id: "lint@market".into(), name: "lint".into(), data: root.join("data"), root, options: vec![("token".into(), "s3cret".into(), true), ("level".into(), "2".into(), false)] }
    }

    #[test]
    fn servers_run_here_named_as_claude_code_names_them_and_stay_out_of_the_manifest() {
        let manifest = json!({
            "name": "lint",
            "hooks": ["./extra/hooks.json", { "Stop": [{ "hooks": [{ "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/stop.sh" }] }] }],
            "mcpServers": ["./servers.json", "./bundle.mcpb", { "inline": { "command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/s.js", "${CLAUDE_PLUGIN_DATA}", "--token=${user_config.token}", "${user_config.missing}"] } }]
        });
        let p = plugin(&[
            (MANIFEST, manifest),
            (MCP_FILE, json!({ "mcpServers": { "base": { "command": "a" }, "file": { "command": "replaced" } } })),
            ("servers.json", json!({ "file": { "command": "b", "env": { "P": "${CLAUDE_PROJECT_DIR}" } } })),
        ]);
        assert_eq!(p.hook_files(), [PathBuf::from("hooks/hooks.json"), PathBuf::from("extra/hooks.json")]);

        let mut args = ParsedArgs::default();
        add_servers(&mut args, std::slice::from_ref(&p), Path::new("/work"));
        let root = p.root.to_string_lossy();
        assert_eq!(args.config_servers.keys().collect::<Vec<_>>(), ["plugin_lint_base", "plugin_lint_file", "plugin_lint_inline"]);
        assert_eq!(Value::Object(args.config_servers["plugin_lint_file"].clone()), json!({ "command": "b", "env": { "P": "/work" } }));
        assert_eq!(args.config_servers["plugin_lint_inline"]["args"], json!([format!("{root}/s.js"), p.data.to_string_lossy(), "--token=s3cret", "${user_config.missing}"]));

        let callbacks = Callbacks::new(&p.root, "nc");
        let view: Value = serde_json::from_slice(&p.manifest_view(&callbacks)).unwrap();
        assert_eq!(view["mcpServers"], json!(["./bundle.mcpb"]));
        assert_eq!(view["hooks"][0], "./extra/hooks.json");
        assert!(view["hooks"][1]["Stop"][0]["hooks"][0]["command"].as_str().unwrap().contains(" -U "));

        // The remote claude gets the non-sensitive options only, under the id it gives the plugin.
        assert_eq!(Value::Object(remote_options(std::slice::from_ref(&p))), json!({ "lint@inline": { "options": { "level": "2" } } }));
        std::fs::remove_dir_all(&p.root).unwrap();
    }

    #[test]
    fn options_come_from_saved_settings_then_secrets_then_defaults() {
        let manifest = json!({ "userConfig": {
            "level": { "type": "number", "default": 1 },
            "tags": { "type": "string", "multiple": true },
            "token": { "type": "string", "sensitive": true },
            "unset": { "type": "string" }
        } });
        let user = json!({ "pluginConfigs": { "lint@market": { "options": { "level": 3, "tags": ["a", "b"] } } } });
        let project = json!({ "pluginConfigs": { "lint@market": { "options": { "level": 9 } } } });
        let managed = json!({ "pluginConfigs": { "lint@market": { "options": { "level": 4 } } } });
        let layers = [(user, true), (project, false), (managed, true)].map(|(v, holds)| (v.as_object().unwrap().clone(), holds));
        let mut secrets = Some(json!({ "pluginSecrets": { "lint@market": { "token": "t0k" } } }).as_object().unwrap().clone());
        let got = options("lint@market", manifest.as_object().unwrap(), &layers, &mut secrets);
        let got: Vec<(&str, &str, bool)> = got.iter().map(|(k, v, s)| (k.as_str(), v.as_str(), *s)).collect();
        assert_eq!(got, [("level", "4", false), ("tags", "a,b", false), ("token", "t0k", true)]);
    }

    #[test]
    fn the_projects_installation_wins_then_the_users_and_other_projects_do_not_count() {
        let registry = json!({ "version": 2, "plugins": {
            "a@m": [
                { "scope": "project", "projectPath": "/elsewhere", "installPath": "/p/other" },
                { "scope": "user", "installPath": "/p/user" },
                { "scope": "local", "projectPath": "/", "installPath": "/p/here" }
            ],
            "b@m": [{ "scope": "project", "projectPath": "/elsewhere", "installPath": "/p/b" }]
        } });
        let registry = registry.as_object().unwrap();
        assert_eq!(install_path(registry, "a@m", Path::new("/")), Some(PathBuf::from("/p/here")));
        assert_eq!(install_path(registry, "a@m", Path::new("/nowhere")), Some(PathBuf::from("/p/user")));
        assert_eq!(install_path(registry, "b@m", Path::new("/")), None);
    }
}
