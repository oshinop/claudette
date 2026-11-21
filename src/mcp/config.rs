use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, bail};
use regex::Regex;
use serde_json::{Value, json};

use crate::args::ParsedArgs;
use crate::config::{home, claudette_dir};
use crate::json::Object;
use crate::mcp::{LocalServer, Source};
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
            other => bail!("Invalid scope: {other}. Must be one of: local, user, project"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Local => "local",
            Scope::Project => "project",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Scope::User => "User config (available in all your projects)",
            Scope::Local => "Local config (private to you in this project)",
            Scope::Project => "Project config (shared via .mcp.json)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Claudette(Scope),
    McpJson,
    Claude(Scope),
}

impl Origin {
    pub fn scope(self) -> Scope {
        match self {
            Origin::Claudette(scope) | Origin::Claude(scope) => scope,
            Origin::McpJson => Scope::Project,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Here,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Approved,
    Pending,
    Rejected,
}

#[derive(Debug)]
pub struct Server {
    pub name: String,
    pub config: Object,
    pub origin: Origin,
    pub place: Place,
    pub approval: Approval,
    pub source: Source,
}

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

pub fn project_key(cwd: &Path) -> String {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()).to_string_lossy().into_owned()
}

static VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").expect("valid pattern"));

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

fn servers_in(value: Option<&Value>, origin: Origin, source: Source) -> Vec<Server> {
    let Some(map) = value.and_then(Value::as_object) else { return vec![] };
    map.iter()
        .filter_map(|(name, config)| {
            let mut config = config.as_object()?.clone();
            let run_on = config.remove("runOn");
            let remote = matches!(origin, Origin::Claudette(_)) && run_on.as_ref().and_then(Value::as_str) == Some("remote");
            let mut value = Value::Object(config);
            expand(&mut value);
            let Value::Object(config) = value else { return None };
            Some(Server { name: name.clone(), config, origin, place: if remote { Place::Remote } else { Place::Here }, approval: Approval::Approved, source: source.clone() })
        })
        .collect()
}

fn names(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

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

pub fn servers(cwd: &Path, mcp_json: bool) -> Vec<Server> {
    let key = project_key(cwd);
    let claude = read_object(&claude_file());
    let own = read_object(&claudette_file());
    let entry = |root: &Object| root.get("projects").and_then(|p| p.get(&key)).and_then(Value::as_object).cloned().unwrap_or_default();
    let (claude_entry, claudette_entry) = (entry(&claude), entry(&own));

    let claude_user = servers_in(claude.get("mcpServers"), Origin::Claude(Scope::User), Source::User);
    let mut project = vec![];
    if mcp_json {
        let (all, enabled, disabled) = approvals(cwd, &claude_entry);
        let mut dirs: Vec<&Path> = cwd.ancestors().collect();
        dirs.reverse();
        for dir in dirs {
            let mut found = servers_in(read_object(&dir.join(".mcp.json")).get("mcpServers"), Origin::McpJson, Source::Project(dir.to_path_buf()));
            for server in &mut found {
                server.approval = if disabled.contains(&server.name) {
                    Approval::Rejected
                } else if all || enabled.contains(&server.name) {
                    Approval::Approved
                } else {
                    Approval::Pending
                };
            }
            project.extend(found);
        }
    }
    let claude_local = servers_in(claude_entry.get("mcpServers"), Origin::Claude(Scope::Local), Source::Local);
    let own_user = servers_in(own.get("mcpServers"), Origin::Claudette(Scope::User), Source::User);
    let own_local = servers_in(claudette_entry.get("mcpServers"), Origin::Claudette(Scope::Local), Source::Local);

    let order: Vec<String> = claude_user.iter().chain(&own_user).chain(&project).chain(&claude_local).chain(&own_local).map(|s| s.name.clone()).collect();
    let mut winners: HashMap<String, Server> = HashMap::new();
    for server in claude_user.into_iter().chain(project).chain(claude_local).chain(own_user).chain(own_local) {
        winners.insert(server.name.clone(), server);
    }
    order.into_iter().filter_map(|name| winners.remove(&name)).collect()
}

pub fn add_servers(p: &mut ParsedArgs, cwd: &Path) {
    if p.strict_mcp {
        return;
    }
    let project = Sources::of(p).project;
    for server in servers(cwd, project) {
        if p.config_servers.contains_key(&server.name) {
            continue;
        }
        match server.approval {
            Approval::Approved => {}
            Approval::Rejected => continue,
            Approval::Pending => {
                warn(&format!(
                    "not starting the .mcp.json server \"{}\": approve it with \"enabledMcpjsonServers\": [\"{}\"] in .claude/settings.local.json",
                    server.name, server.name
                ));
                continue;
            }
        }
        match server.place {
            Place::Here => {
                p.config_servers.insert(server.name, LocalServer { config: server.config, source: server.source });
            }
            Place::Remote => {
                p.remote_servers.insert(server.name, server.config);
            }
        }
    }
}

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

fn scope_file(cwd: &Path, scope: Scope, place: Place) -> PathBuf {
    match (scope, place) {
        (Scope::Project, _) => cwd.join(".mcp.json"),
        (_, Place::Here) => claude_file(),
        (_, Place::Remote) => claudette_file(),
    }
}

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

pub fn files_with(cwd: &Path, scope: Scope, name: &str) -> Vec<PathBuf> {
    let files = if scope == Scope::Project { vec![scope_file(cwd, scope, Place::Here)] } else { vec![claude_file(), claudette_file()] };
    files.into_iter().filter(|f| scope_servers(&mut read_object(f), scope, cwd).contains_key(name)).collect()
}

pub enum Removal {
    Removed(Scope, PathBuf),
    Missing,
    Ambiguous(Vec<(Scope, PathBuf)>),
}

pub fn remove(cwd: &Path, scope: Option<Scope>, name: &str) -> anyhow::Result<Removal> {
    let (scope, files) = match scope {
        Some(scope) => (scope, files_with(cwd, scope, name)),
        None => {
            let mut found: Vec<(Scope, Vec<PathBuf>)> = [Scope::Local, Scope::User, Scope::Project].into_iter().map(|s| (s, files_with(cwd, s, name))).filter(|(_, f)| !f.is_empty()).collect();
            match found.len() {
                0 => return Ok(Removal::Missing),
                1 => found.remove(0),
                _ => return Ok(Removal::Ambiguous(found.into_iter().map(|(s, f)| (s, f[0].clone())).collect())),
            }
        }
    };
    let Some(first) = files.first().cloned() else { return Ok(Removal::Missing) };
    for file in files {
        let mut root = read_for_change(&file)?;
        let removed = scope_servers(&mut root, scope, cwd).remove(name);
        if let Some(mut removed) = removed
            && !(file == claudette_file() && removed.get("runOn").and_then(Value::as_str) == Some("remote"))
        {
            expand(&mut removed);
            if let Some(store) = removed.as_object().and_then(|config| super::oauth::Store::for_scope(name, config, cwd, scope)) {
                store.clear()?;
            }
        }
        write_pretty(&file, &root)?;
    }
    Ok(Removal::Removed(scope, first))
}

const CHOICES: [&str; 3] = ["enabledMcpjsonServers", "disabledMcpjsonServers", "enableAllProjectMcpServers"];

pub fn reset_choices(cwd: &Path) -> anyhow::Result<()> {
    let file = claude_file();
    let mut root = read_for_change(&file)?;
    if let Some(Value::Object(entry)) = root.get_mut("projects").and_then(|p| p.get_mut(project_key(cwd)))
        && CHOICES.map(|key| entry.remove(key).is_some()).contains(&true)
    {
        write_pretty(&file, &root)?;
    }
    let local = cwd.join(".claude").join("settings.local.json");
    let mut settings = read_for_change(&local)?;
    if CHOICES.map(|key| settings.remove(key).is_some()).contains(&true) {
        write_pretty(&local, &settings)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_expand_with_defaults() {
        unsafe { std::env::set_var("CT_MCP_TOKEN", "t0k") };
        let mut v = json!({ "headers": { "Authorization": "Bearer ${CT_MCP_TOKEN}" }, "args": ["${CT_MCP_MISSING:-fallback}", "${CT_MCP_MISSING}"] });
        expand(&mut v);
        assert_eq!(v, json!({ "headers": { "Authorization": "Bearer t0k" }, "args": ["fallback", ""] }));
    }

    #[test]
    fn only_claudettes_own_file_places_a_server_remotely() {
        let own = servers_in(Some(&json!({ "a": { "command": "x", "runOn": "remote" }, "b": { "command": "y" } })), Origin::Claudette(Scope::User), Source::User);
        assert_eq!(own.iter().map(|s| (s.name.as_str(), s.place)).collect::<Vec<_>>(), [("a", Place::Remote), ("b", Place::Here)]);
        assert!(!own[0].config.contains_key("runOn"));
        let claude = servers_in(Some(&json!({ "c": { "command": "z", "runOn": "remote" } })), Origin::Claude(Scope::User), Source::User);
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
        assert_eq!((shared.config["command"].as_str(), shared.approval), (Some("inner"), Approval::Approved));
        assert_eq!(found.iter().find(|s| s.name == "other").unwrap().approval, Approval::Pending);
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
        assert!(servers(&dir, true).iter().any(|s| s.name == "p" && s.approval == Approval::Approved));
        assert!(add(&dir, Scope::Project, "q", Object::new(), Place::Remote).is_err());
        assert!(matches!(remove(&dir, Some(Scope::Project), "p").unwrap(), Removal::Removed(Scope::Project, _)));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
