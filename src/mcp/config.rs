//! Which MCP servers a session gets, and where each runs. All of it comes from this machine.
//!
//! The remote claude runs with `--strict-mcp-config`: nothing the remote machine has configured
//! starts. Every server is defined here, and each says where it runs:
//!
//! | Defined in | Runs |
//! |---|---|
//! | `~/.config/claudette/mcp.json` (`claude mcp add --remote`), user scope or local scope (this project) | where its `runOn` says: `remote`, or here |
//! | the project's `.mcp.json`, once approved | here |
//! | this machine's `~/.claude.json`, user and local scope | here |
//! | the caller's `--mcp-config` and `setMcpServers` | here |
//!
//! On a name clash the caller's servers win, then claudette's own (local scope over user scope),
//! then `~/.claude.json`'s local scope, then `.mcp.json`, then `~/.claude.json`'s user scope. A
//! server that runs here reaches claude through the relay or a forwarded socket; a remote one is
//! started by the remote claude, inside its sandbox.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, bail};
use regex::Regex;
use serde_json::{Value, json};

use crate::args::ParsedArgs;
use crate::config::{home, claudette_dir};
use crate::json::Object;
use crate::log::warn;
use crate::settings::{SETTINGS_FILES, Sources};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Local,
    Project,
}

impl Scope {
    pub fn parse(s: &str) -> anyhow::Result<Scope> {
        match s {
            "user" => Ok(Scope::User),
            "local" => Ok(Scope::Local),
            "project" => Ok(Scope::Project),
            other => bail!("unknown scope {other}: use user, local or project"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Local => "local",
            Scope::Project => "project",
        }
    }
}

/// Where a server is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `~/.config/claudette/mcp.json`
    Claudette(Scope),
    /// The project's `.mcp.json`.
    McpJson,
    /// This machine's `~/.claude.json`.
    Claude(Scope),
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Claudette(Scope::User) => "claudette, user",
            Origin::Claudette(_) => "claudette, local (this project)",
            Origin::McpJson => "project (.mcp.json)",
            Origin::Claude(Scope::User) => "~/.claude.json, user",
            Origin::Claude(_) => "~/.claude.json, local (this project)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Here,
    Remote,
}

#[derive(Debug)]
pub struct Server {
    pub name: String,
    /// The definition as claude takes it: `runOn` removed, variables expanded.
    pub config: Object,
    pub origin: Origin,
    pub place: Place,
    /// A `.mcp.json` server runs only once approved, as Claude Code asks before it starts one.
    pub approved: bool,
}

/// claudette's own MCP configuration, in `~/.claude.json`'s shape: `mcpServers` for user scope,
/// `projects.<path>.mcpServers` for local scope.
pub fn claudette_file() -> PathBuf {
    claudette_dir().join("mcp.json")
}

fn claude_file() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => home().join(".claude.json"),
    }
}

fn read_object(path: &Path) -> Object {
    match std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        Some(Value::Object(o)) => o,
        _ => Object::new(),
    }
}

/// The key a project has in `projects`: its real path.
fn project_key(cwd: &Path) -> String {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()).to_string_lossy().into_owned()
}

static VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").expect("valid pattern"));

/// `${VAR}` and `${VAR:-default}` in every string, from this machine's environment, as Claude Code expands them.
pub fn expand(value: &mut Value) {
    match value {
        Value::String(s) if s.contains("${") => {
            *s = VAR
                .replace_all(s, |c: &regex::Captures| std::env::var(&c[1]).ok().or_else(|| c.get(2).map(|d| d.as_str().to_owned())).unwrap_or_default())
                .into_owned();
        }
        Value::Array(items) => items.iter_mut().for_each(expand),
        Value::Object(map) => map.values_mut().for_each(expand),
        _ => {}
    }
}

fn servers_in(value: Option<&Value>, origin: Origin) -> Vec<Server> {
    let Some(map) = value.and_then(Value::as_object) else { return vec![] };
    map.iter()
        .filter_map(|(name, config)| {
            let mut config = config.as_object()?.clone();
            let run_on = config.remove("runOn");
            // Only claudette's own file places a server; elsewhere `runOn` is not Claude Code's and stays out.
            let remote = matches!(origin, Origin::Claudette(_)) && run_on.as_ref().and_then(Value::as_str) == Some("remote");
            let mut value = Value::Object(config);
            expand(&mut value);
            let Value::Object(config) = value else { return None };
            Some(Server { name: name.clone(), config, origin, place: if remote { Place::Remote } else { Place::Here }, approved: true })
        })
        .collect()
}

fn names(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// Whether the project's `.mcp.json` servers are approved, as Claude Code records it: in the
/// project's settings, or in its entry in `~/.claude.json`. Returns (all, enabled, disabled).
fn approvals(cwd: &Path, claude_entry: &Object) -> (bool, Vec<String>, Vec<String>) {
    let mut all = claude_entry.get("enableAllProjectMcpServers").and_then(Value::as_bool).unwrap_or(false);
    let mut enabled = names(claude_entry.get("enabledMcpjsonServers"));
    let mut disabled = names(claude_entry.get("disabledMcpjsonServers"));
    for file in SETTINGS_FILES {
        let settings = read_object(&cwd.join(file));
        all |= settings.get("enableAllProjectMcpServers").and_then(Value::as_bool).unwrap_or(false);
        enabled.extend(names(settings.get("enabledMcpjsonServers")));
        disabled.extend(names(settings.get("disabledMcpjsonServers")));
    }
    (all, enabled, disabled)
}

/// Every server defined for a project directory: the winning definition for each name.
/// `mcp_json` follows `--setting-sources` (whether the project's settings load).
pub fn servers(cwd: &Path, mcp_json: bool) -> Vec<Server> {
    let key = project_key(cwd);
    let claude = read_object(&claude_file());
    let own = read_object(&claudette_file());
    let entry = |root: &Object| root.get("projects").and_then(|p| p.get(&key)).and_then(Value::as_object).cloned().unwrap_or_default();
    let (claude_entry, claudette_entry) = (entry(&claude), entry(&own));

    // Lowest precedence first; a later definition replaces an earlier one.
    let mut layers = vec![servers_in(claude.get("mcpServers"), Origin::Claude(Scope::User))];
    if mcp_json {
        let (all, enabled, disabled) = approvals(cwd, &claude_entry);
        let mut dirs: Vec<&Path> = cwd.ancestors().collect();
        dirs.reverse();
        for dir in dirs {
            let mut found = servers_in(read_object(&dir.join(".mcp.json")).get("mcpServers"), Origin::McpJson);
            for server in &mut found {
                server.approved = !disabled.contains(&server.name) && (all || enabled.contains(&server.name));
            }
            layers.push(found);
        }
    }
    layers.push(servers_in(claude_entry.get("mcpServers"), Origin::Claude(Scope::Local)));
    layers.push(servers_in(own.get("mcpServers"), Origin::Claudette(Scope::User)));
    layers.push(servers_in(claudette_entry.get("mcpServers"), Origin::Claudette(Scope::Local)));

    let mut winners: BTreeMap<String, Server> = BTreeMap::new();
    for server in layers.into_iter().flatten() {
        winners.insert(server.name.clone(), server);
    }
    winners.into_values().collect()
}

/// Adds this machine's servers to the session, beside the caller's own (which win). With the
/// caller's `--strict-mcp-config`, only the caller's servers run, as with claude.
pub fn add_servers(p: &mut ParsedArgs, cwd: &Path) {
    if p.strict_mcp {
        return;
    }
    let project = Sources::of(p).project;
    for server in servers(cwd, project) {
        if p.config_servers.contains_key(&server.name) {
            continue;
        }
        if !server.approved {
            warn(&format!(
                "not starting the .mcp.json server \"{}\": approve it with \"enabledMcpjsonServers\": [\"{}\"] in .claude/settings.local.json",
                server.name, server.name
            ));
            continue;
        }
        match server.place {
            Place::Here => p.config_servers.insert(server.name, server.config),
            Place::Remote => p.remote_servers.insert(server.name, server.config),
        };
    }
}

/// Reads a configuration file to change it: missing is empty, but one that does not parse stops the
/// change instead of being replaced.
fn read_for_change(path: &Path) -> anyhow::Result<Object> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Object::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(o)) => Ok(o),
        _ => bail!("{} is not a JSON object; fix it first", path.display()),
    }
}

fn write_pretty(path: &Path, value: &Object) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// The file a server of a scope and place lives in: where Claude Code keeps it (`~/.claude.json`,
/// the project's `.mcp.json`), or claudette's own file for a server that runs remotely.
fn scope_file(cwd: &Path, scope: Scope, place: Place) -> PathBuf {
    match (scope, place) {
        (Scope::Project, _) => cwd.join(".mcp.json"),
        (_, Place::Here) => claude_file(),
        (_, Place::Remote) => claudette_file(),
    }
}

/// The `mcpServers` object of a scope in its file, created when missing.
fn scope_servers<'a>(root: &'a mut Object, scope: Scope, cwd: &Path) -> &'a mut Object {
    let holder = if scope == Scope::Local {
        let projects = root.entry("projects").or_insert_with(|| json!({}));
        let project = projects.as_object_mut().expect("projects is an object").entry(project_key(cwd)).or_insert_with(|| json!({}));
        project.as_object_mut().expect("a project entry is an object")
    } else {
        root
    };
    holder.entry("mcpServers").or_insert_with(|| json!({})).as_object_mut().expect("mcpServers is an object")
}

/// Adds or replaces a server, as `claude mcp add` does: user and local scope in `~/.claude.json`,
/// project scope in the project's `.mcp.json`, approved for this machine in
/// `.claude/settings.local.json` since you added it. A remote server goes to claudette's file,
/// the one place `runOn` means something; a server of the same name and scope in the other file goes.
pub fn add(cwd: &Path, scope: Scope, name: &str, mut config: Object, place: Place) -> anyhow::Result<PathBuf> {
    if scope == Scope::Project && place == Place::Remote {
        bail!("a project's .mcp.json servers run on this machine; add a remote server with --scope user or local");
    }
    if place == Place::Remote {
        config.insert("runOn".into(), json!("remote"));
    }
    let file = scope_file(cwd, scope, place);
    let mut root = read_for_change(&file)?;
    scope_servers(&mut root, scope, cwd).insert(name.to_owned(), Value::Object(config));
    write_pretty(&file, &root)?;
    if scope != Scope::Project {
        let other = scope_file(cwd, scope, if place == Place::Here { Place::Remote } else { Place::Here });
        let mut root = read_for_change(&other)?;
        if scope_servers(&mut root, scope, cwd).remove(name).is_some() {
            write_pretty(&other, &root)?;
        }
    }
    if scope == Scope::Project {
        let local = cwd.join(".claude").join("settings.local.json");
        let mut settings = read_for_change(&local)?;
        if let Value::Array(list) = settings.entry("enabledMcpjsonServers").or_insert_with(|| json!([]))
            && !list.iter().any(|v| v == name)
        {
            list.push(json!(name));
        }
        write_pretty(&local, &settings)?;
    }
    Ok(file)
}

/// Removes a server from the scope given, or from the one scope that has it.
pub fn remove(cwd: &Path, scope: Option<Scope>, name: &str) -> anyhow::Result<(Scope, PathBuf)> {
    let homes = |scope: Scope| -> Vec<PathBuf> {
        let files = if scope == Scope::Project { vec![scope_file(cwd, scope, Place::Here)] } else { vec![claude_file(), claudette_file()] };
        files.into_iter().filter(|f| scope_servers(&mut read_object(f), scope, cwd).contains_key(name)).collect()
    };
    let (scope, files) = match scope {
        Some(scope) => (scope, homes(scope)),
        None => {
            let found: Vec<(Scope, Vec<PathBuf>)> = [Scope::Local, Scope::User, Scope::Project].into_iter().map(|s| (s, homes(s))).filter(|(_, f)| !f.is_empty()).collect();
            match found.len() {
                1 => found.into_iter().next().expect("one"),
                0 => bail!("no MCP server named {name}"),
                _ => bail!("{name} is defined in more than one scope; choose one with --scope"),
            }
        }
    };
    let Some(first) = files.first().cloned() else { bail!("no MCP server named {name} in {} scope", scope.name()) };
    for file in files {
        let mut root = read_for_change(&file)?;
        scope_servers(&mut root, scope, cwd).remove(name);
        write_pretty(&file, &root)?;
    }
    Ok((scope, first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_expand_with_defaults() {
        // SAFETY: tests in this module are the only readers of this variable.
        unsafe { std::env::set_var("CT_MCP_TOKEN", "t0k") };
        let mut v = json!({ "headers": { "Authorization": "Bearer ${CT_MCP_TOKEN}" }, "args": ["${CT_MCP_MISSING:-fallback}", "${CT_MCP_MISSING}"] });
        expand(&mut v);
        assert_eq!(v, json!({ "headers": { "Authorization": "Bearer t0k" }, "args": ["fallback", ""] }));
    }

    #[test]
    fn only_claudettes_own_file_places_a_server_remotely() {
        let own = servers_in(Some(&json!({ "a": { "command": "x", "runOn": "remote" }, "b": { "command": "y" } })), Origin::Claudette(Scope::User));
        assert_eq!(own.iter().map(|s| (s.name.as_str(), s.place)).collect::<Vec<_>>(), [("a", Place::Remote), ("b", Place::Here)]);
        assert!(!own[0].config.contains_key("runOn"));
        let claude = servers_in(Some(&json!({ "c": { "command": "z", "runOn": "remote" } })), Origin::Claude(Scope::User));
        assert_eq!(claude[0].place, Place::Here);
    }

    #[test]
    fn project_servers_need_approval_and_nearer_definitions_win() {
        let root = std::env::temp_dir().join(format!("ct-mcp-{}", std::process::id()));
        let sub = root.join("app");
        std::fs::create_dir_all(sub.join(".claude")).unwrap();
        std::fs::write(root.join(".mcp.json"), r#"{"mcpServers":{"shared":{"command":"outer"},"other":{"command":"o"}}}"#).unwrap();
        std::fs::write(sub.join(".mcp.json"), r#"{"mcpServers":{"shared":{"command":"inner"}}}"#).unwrap();
        std::fs::write(sub.join(".claude/settings.local.json"), r#"{"enabledMcpjsonServers":["shared"]}"#).unwrap();
        let found = servers(&sub, true);
        let shared = found.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!((shared.config["command"].as_str(), shared.approved), (Some("inner"), true));
        assert!(!found.iter().find(|s| s.name == "other").unwrap().approved);
        assert!(servers(&sub, false).iter().all(|s| s.origin != Origin::McpJson));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_project_server_added_here_lands_in_mcp_json_and_is_approved_for_this_machine() {
        let dir = std::env::temp_dir().join(format!("ct-mcp-add-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        add(&dir, Scope::Project, "p", json!({ "command": "x" }).as_object().unwrap().clone(), Place::Here).unwrap();
        assert_eq!(read_object(&dir.join(".mcp.json"))["mcpServers"]["p"], json!({ "command": "x" }));
        assert_eq!(read_object(&dir.join(".claude/settings.local.json"))["enabledMcpjsonServers"], json!(["p"]));
        assert!(servers(&dir, true).iter().any(|s| s.name == "p" && s.approved));
        assert!(add(&dir, Scope::Project, "q", Object::new(), Place::Remote).is_err());
        assert_eq!(remove(&dir, Some(Scope::Project), "p").unwrap().0, Scope::Project);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
