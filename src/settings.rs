//! Claude Code settings from this machine, as the remote claude gets them.
//!
//! The project's `.claude/settings.json` and `.claude/settings.local.json` reach it through the
//! mounted workspace, served through a view that adapts them, and whatever it writes back is turned
//! back first. The user's `~/.claude/settings.json` reaches it as the session's Claude Code folder's
//! settings file (see [`crate::userconfig`]), through the same view, without what belongs to this
//! machine's login, plugins or paths. In the view:
//!
//! - a hook or status line command becomes a call back to this machine, where the command runs in
//!   the project with the hook's input (paths and tool names made local). Its side effects, like a
//!   formatter rewriting a file, happen here, where the files are.
//! - a rule or matcher that names a replaced tool (`Write`, `Edit|Write`) also names the local
//!   tool (`mcp__cc__Write`), which is the tool the remote claude has.
//!
//! Two hook events concern only this machine, so they fire here and never from the remote claude:
//! CwdChanged, when the local Bash tool changes directory, and FileChanged, for the files its
//! matchers name in the shell's directory. What SessionStart, Setup, CwdChanged and FileChanged
//! hooks write to `CLAUDE_ENV_FILE` reaches the local Bash tool's later commands.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::args::ParsedArgs;
use crate::json::{Object, str_field};
use crate::names::{EDIT_TOOLS, PREFIX, READ_TOOLS, is_local_tool, unprefix};
use crate::mount::workdir::PathMap;
use crate::permissions::rules::{self, Behavior, PathRules, Source};
use crate::ssh::shell_quote;

/// Claude Code's default limit for a command hook.
const DEFAULT_TIMEOUT: f64 = 60.0;

/// The settings files the view adapts, relative to the project root.
pub const SETTINGS_FILES: [&str; 2] = [".claude/settings.json", ".claude/settings.local.json"];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Hook,
    /// A command whose output Claude Code shows as it is: the status lines, the `@` file suggestions.
    Display,
}

/// The settings keys that run a command for Claude Code's interface, all run on this machine.
const DISPLAY_KEYS: [&str; 3] = ["statusLine", "subagentStatusLine", "fileSuggestion"];

#[derive(Clone, PartialEq)]
struct Callback {
    kind: Kind,
    /// For a hook, its event and its group's matcher.
    event: String,
    matcher: Option<String>,
    command: String,
    /// Exec form: the arguments, passed to `command` with no shell.
    args: Option<Vec<String>>,
    timeout: Option<f64>,
    /// For a plugin's hook, the plugin's folders here.
    plugin: Option<PluginEnv>,
}

/// A plugin's folders on this machine and its options, which its hooks get as `CLAUDE_PLUGIN_ROOT`,
/// `CLAUDE_PLUGIN_DATA` and `CLAUDE_PLUGIN_OPTION_<KEY>` (and `${user_config.KEY}` in exec form).
#[derive(Clone, Debug, PartialEq)]
pub struct PluginEnv {
    pub root: PathBuf,
    pub data: PathBuf,
    pub options: Vec<(String, String)>,
}

/// Project settings that choose plugins: the plugins come from this machine (see [`crate::plugins`]),
/// so the remote claude never sees these, and would otherwise install the remote machine's.
pub const PLUGIN_KEYS: [&str; 2] = ["enabledPlugins", "extraKnownMarketplaces"];

/// Distinct env files for the hook runners of one process without a run folder (tests).
static NEXT_ENV: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Which settings claude loads for the caller, from its `--setting-sources` (all when absent).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sources {
    pub user: bool,
    pub project: bool,
    pub local: bool,
}

impl Sources {
    pub fn of(p: &ParsedArgs) -> Sources {
        match p.setting_sources.as_deref() {
            None => Sources { user: true, project: true, local: true },
            Some(v) => {
                let list: Vec<&str> = v.split(',').map(str::trim).collect();
                Sources { user: list.contains(&"user"), project: list.contains(&"project"), local: list.contains(&"local") }
            }
        }
    }
}

/// This machine's Claude Code folder: `CLAUDE_CONFIG_DIR`, or `~/.claude`.
pub fn user_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => crate::config::home().join(".claude"),
    }
}

fn read_settings(path: &Path) -> Option<Object> {
    match serde_json::from_slice::<Value>(&std::fs::read(path).ok()?) {
        Ok(Value::Object(o)) => Some(o),
        _ => None,
    }
}

/// The settings files claude loads, lowest precedence first, each with the directory its `/path`
/// rules anchor at: user settings at the user's Claude Code folder, project and local at the project.
fn loaded(root: &Path, s: Sources) -> Vec<(PathBuf, Object)> {
    let mut files = vec![];
    if s.user {
        files.push((user_dir(), user_dir().join("settings.json")));
    }
    for (file, used) in SETTINGS_FILES.iter().zip([s.project, s.local]) {
        if used {
            files.push((root.to_path_buf(), root.join(file)));
        }
    }
    files.into_iter().filter_map(|(anchor, file)| read_settings(&file).map(|s| (anchor, s))).collect()
}

/// Claude Code's `respectGitignore` (true unless a settings file turns it off): whether its `@` file
/// picker leaves out what git ignores. The workspace mount follows it.
pub fn respects_gitignore(root: &Path) -> bool {
    loaded(root, Sources { user: true, project: true, local: true })
        .iter()
        .rev()
        .find_map(|(_, settings)| settings.get("respectGitignore").and_then(Value::as_bool))
        .unwrap_or(true)
}

fn rule_list<'a>(settings: &'a Object, key: &str) -> impl Iterator<Item = &'a str> {
    settings.get("permissions").and_then(|p| p.get(key)).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str)
}

fn bash_content(rule: &str) -> Option<String> {
    rule.strip_prefix("Bash(").and_then(|r| r.strip_suffix(')')).map(str::to_owned)
}

/// Expands a directory as settings name it: `~/…` at the home folder, relative at the project.
fn settings_dir(dir: &str, root: &Path) -> PathBuf {
    match dir.strip_prefix("~/") {
        Some(rest) => crate::config::home().join(rest),
        None => crate::args::normalize(&root.join(dir)),
    }
}

/// What the settings claude loads mean for the tools on this machine, which claude cannot apply to
/// them itself: `Bash(…)` rules, `Read(…)` and `Edit(…)` rules, `env` (for the commands, hooks and
/// servers that run here) and `additionalDirectories`. The caller's `--settings` ranks last, and its
/// `--allowedTools` and `--disallowedTools` count as rules too. Returns the path rules.
pub fn apply_here(args: &mut ParsedArgs, root: &Path) -> PathRules {
    let mut layers = loaded(root, Sources::of(args));
    if let Some(caller) = &args.settings {
        layers.push((root.to_path_buf(), caller.clone()));
    }
    let mut sources = vec![];
    let mut env: Vec<(String, String)> = vec![];
    for (anchor, settings) in &layers {
        args.bash_allow.extend(rule_list(settings, "allow").filter_map(bash_content));
        args.bash_deny.extend(rule_list(settings, "deny").filter_map(bash_content));
        let rules: Vec<(Behavior, String)> = [("allow", Behavior::Allow), ("ask", Behavior::Ask), ("deny", Behavior::Deny)]
            .into_iter()
            .flat_map(|(key, behavior)| rule_list(settings, key).map(move |r| (behavior, r.to_owned())))
            .collect();
        sources.push((anchor.clone(), rules));
        for dir in rule_list(settings, "additionalDirectories") {
            let dir = settings_dir(dir, root);
            if !args.add_dirs.contains(&dir) {
                args.add_dirs.push(dir);
            }
        }
        if let Some(Value::Object(vars)) = settings.get("env") {
            env.extend(vars.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))));
        }
    }
    let cli: Vec<(Behavior, String)> =
        args.allowed.iter().map(|r| (Behavior::Allow, r.clone())).chain(args.disallowed.iter().map(|r| (Behavior::Deny, r.clone()))).collect();
    sources.push((root.to_path_buf(), cli));
    // Login and provider variables stay out: on this machine they belong to no claude.
    let _ = LOCAL_ENV.set(env.into_iter().filter(|(k, _)| !is_login_env(k)).collect());
    for (behavior, rule) in sources.iter().flat_map(|(_, rules)| rules) {
        match (behavior, rules::parse(rule)) {
            (Behavior::Ask, Some((kind, _))) => {
                let covered: &[&'static str] = if kind == rules::Kind::Read { &READ_TOOLS } else { &EDIT_TOOLS };
                for tool in covered {
                    if !args.ask_tools.contains(tool) {
                        args.ask_tools.push(tool);
                    }
                }
            }
            (Behavior::Allow, None) if is_local_tool(rule) && !args.tool_allows.contains(rule) => args.tool_allows.push(rule.clone()),
            _ => {}
        }
    }
    let sources: Vec<Source> = sources.iter().map(|(anchor, rules)| Source { anchor, rules: rules.clone() }).collect();
    PathRules::new(&sources, root, &crate::config::home())
}

static LOCAL_ENV: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// The settings' `env`, for the processes that start on this machine in claude's name: the Bash
/// tool's commands, hooks and MCP servers. Claude Code sets it for every process it starts.
pub fn local_env() -> &'static [(String, String)] {
    LOCAL_ENV.get().map_or(&[], Vec::as_slice)
}

/// The remote claude's settings with an ask rule for each local tool an ask path rule covers, so
/// that a call to one reaches the permission check even when an allow rule covers the tool.
pub fn with_asks(settings: Option<Object>, tools: &[&str]) -> Option<Object> {
    if tools.is_empty() {
        return settings;
    }
    let mut settings = settings.unwrap_or_default();
    let permissions = settings.entry("permissions").or_insert_with(|| json!({}));
    if !permissions.is_object() {
        *permissions = json!({});
    }
    let ask = permissions.as_object_mut().expect("an object").entry("ask").or_insert_with(|| json!([]));
    if !ask.is_array() {
        *ask = json!([]);
    }
    ask.as_array_mut().expect("an array").extend(tools.iter().map(|t| json!(format!("{PREFIX}{t}"))));
    Some(settings)
}

/// Claude Code's switch for agent teams. Teammates get SendMessage whatever the tool list says, and on
/// the remote machine it would reach every session there, so teams stay off.
pub const AGENT_TEAMS: &str = "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS";

/// Claude Code's switch for agent view and background sessions. `/bg`, `←` on an empty prompt and
/// "Move to background and exit" would hand the session to the remote machine's own background
/// service, which outlives the run and its sockets: no local tools, no mounts behind it.
pub const AGENT_VIEW: &str = "CLAUDE_CODE_DISABLE_AGENT_VIEW";

/// The token file in the run's private inbox folder on the remote machine (`CLAUDE_CODE_TMPDIR`).
pub const INBOX_TOKEN: &str = "token";

/// What claudette adds to the remote claude's `--settings` in every session, which rank above the
/// user's and the project's settings:
/// - agent teams and agent view off, so their `env` cannot turn them back on;
/// - a `SessionStart` hook that keeps claude's messaging token (exported to hooks) in the run's private
///   inbox folder. A message for this session then opens with it, so claude takes the message as from
///   its own child and delivers it whatever its permission mode, as it does a hook posting to it.
pub fn session_settings(settings: Option<Object>) -> Option<Object> {
    let mut settings = settings.unwrap_or_default();
    let env = settings.entry("env").or_insert_with(|| json!({}));
    if !env.is_object() {
        *env = json!({});
    }
    let env = env.as_object_mut().expect("an object");
    env.insert(AGENT_TEAMS.into(), json!("0"));
    env.insert(AGENT_VIEW.into(), json!("1"));
    settings.insert("disableAgentView".into(), json!(true));
    let hooks = settings.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let start = hooks.as_object_mut().expect("an object").entry("SessionStart").or_insert_with(|| json!([]));
    if !start.is_array() {
        *start = json!([]);
    }
    let save = format!(r#"[ -n "$CLAUDE_CODE_TMPDIR" ] && (umask 077 && printf '%s' "$CLAUDE_CODE_MESSAGING_TOKEN" >"$CLAUDE_CODE_TMPDIR/{INBOX_TOKEN}"); exit 0"#);
    start.as_array_mut().expect("an array").push(json!({ "hooks": [{ "type": "command", "command": save, "timeout": 10 }] }));
    Some(settings)
}

/// User settings that belong to this machine rather than to how the user wants Claude to work:
/// its login (the remote machine has its own), its installed plugins and marketplaces, and paths.
const MACHINE_KEYS: [&str; 11] = [
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "gcpAuthRefresh",
    "otelHeadersHelper",
    "forceLoginMethod",
    "forceLoginOrgUUID",
    "enabledPlugins",
    "extraKnownMarketplaces",
    "strictKnownMarketplaces",
    "pluginConfigs",
];

/// Environment variables that choose or carry this machine's credentials and API provider.
fn is_login_env(name: &str) -> bool {
    ["ANTHROPIC_", "AWS_", "GOOGLE_", "CLOUD_ML_", "VERTEX_", "AZURE_"].iter().any(|p| name.starts_with(p))
        || matches!(name, "CLAUDE_CODE_USE_BEDROCK" | "CLAUDE_CODE_USE_VERTEX" | "CLAUDE_CODE_USE_FOUNDRY" | "CLAUDE_CODE_OAUTH_TOKEN" | "CLAUDE_CODE_API_KEY_HELPER_TTL_MS")
}

/// The user's settings without what belongs to this machine.
pub(crate) fn user_part(mut settings: Object) -> Object {
    for key in MACHINE_KEYS {
        settings.remove(key);
    }
    // Variables as forwarding passes them, but the file's credentials stay with this machine's other
    // login settings (apiKeyHelper and the like, above).
    if let Some(Value::Object(env)) = settings.get_mut("env") {
        env.retain(|name, _| !crate::ssh::machine_bound(name) && !crate::ssh::is_credential(name));
    }
    // Directories on this machine; the remote claude cannot open them, and the local tools have --add-dir.
    if let Some(Value::Object(permissions)) = settings.get_mut("permissions") {
        permissions.remove("additionalDirectories");
    }
    settings
}

/// `Edit|Write` → `Edit|Write|mcp__cc__Edit|mcp__cc__Write`; `*` and others stay.
fn widen_matcher(matcher: &str) -> String {
    let parts: Vec<&str> = matcher.split('|').collect();
    let added: Vec<String> = parts.iter().filter(|p| is_local_tool(p.trim())).map(|p| format!("{PREFIX}{}", p.trim())).filter(|p| !parts.contains(&p.as_str())).collect();
    if added.is_empty() { matcher.to_owned() } else { format!("{matcher}|{}", added.join("|")) }
}

/// The local tool names a widened matcher gained, removed again.
fn narrow_matcher(matcher: &str) -> String {
    let parts: Vec<&str> = matcher.split('|').collect();
    parts.iter().filter(|p| !p.strip_prefix(PREFIX).is_some_and(|bare| is_local_tool(bare) && parts.contains(&bare))).copied().collect::<Vec<_>>().join("|")
}

/// Rule lists: a replaced tool's rule (`Write`) gains the local tool's (`mcp__cc__Write`).
fn widen_rules(rules: &mut Vec<Value>) {
    let mut out = vec![];
    for rule in rules.drain(..) {
        let local = rule.as_str().filter(|r| is_local_tool(r)).map(|r| json!(format!("{PREFIX}{r}")));
        out.push(rule);
        out.extend(local);
    }
    let mut seen = HashSet::new();
    out.retain(|r| seen.insert(r.to_string()));
    *rules = out;
}

/// Rule lists back on this machine: local tool names become the built-in ones a local claude knows,
/// with their parameters (`mcp__cc__Bash(npm test:*)` becomes `Bash(npm test:*)`).
pub(crate) fn narrow_rules(rules: &mut Vec<Value>) {
    for rule in rules.iter_mut() {
        if let Some(bare) = rule.as_str().and_then(|r| r.strip_prefix(PREFIX)).filter(|b| is_local_tool(b.split('(').next().unwrap_or(b))) {
            *rule = json!(bare);
        }
    }
    let mut seen = HashSet::new();
    rules.retain(|r| seen.insert(r.to_string()));
}

fn rule_lists(settings: &mut Object) -> Vec<&mut Vec<Value>> {
    match settings.get_mut("permissions") {
        Some(Value::Object(permissions)) => permissions
            .iter_mut()
            .filter(|(k, _)| matches!(k.as_str(), "allow" | "deny" | "ask"))
            .filter_map(|(_, v)| v.as_array_mut())
            .collect(),
        _ => vec![],
    }
}

/// Every command hook entry: (event, matcher group or hook, whether it is the group) as mutable objects.
fn for_each_hook(settings: &mut Object, mut f: impl FnMut(&str, &mut Object, bool)) {
    let Some(Value::Object(events)) = settings.get_mut("hooks") else { return };
    for (event, groups) in events.iter_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        for group in groups.iter_mut().filter_map(Value::as_object_mut) {
            f(event, group, true);
            for hook in group.get_mut("hooks").and_then(Value::as_array_mut).into_iter().flatten().filter_map(Value::as_object_mut) {
                f(event, hook, false);
            }
        }
    }
}

fn serialize(settings: &Object) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(settings).unwrap_or_default();
    out.push(b'\n');
    out
}

/// The events whose hooks may write `export` lines to `CLAUDE_ENV_FILE` for later Bash commands.
const ENV_EVENTS: [&str; 4] = ["SessionStart", "Setup", "CwdChanged", "FileChanged"];

/// The events claudette fires itself; the remote claude's own firings of them are ignored.
const LOCAL_EVENTS: [&str; 2] = ["CwdChanged", "FileChanged"];

/// The common hook input fields that stay the same across a session, kept for the local events.
const COMMON_FIELDS: [&str; 3] = ["session_id", "transcript_path", "permission_mode"];

/// How often the files FileChanged watches are looked at.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// The commands of the session's settings (the project's and the user's), run here when the remote
/// claude calls back or when CwdChanged or FileChanged fires here.
pub struct Callbacks {
    root: PathBuf,
    paths: PathMap,
    nc: String,
    /// The callback socket's path on the remote machine; set once it is forwarded.
    socket: OnceLock<String>,
    entries: Mutex<Vec<Callback>>,
    /// `CLAUDE_ENV_FILE` for SessionStart and Setup hooks: kept for the whole session.
    session_env: PathBuf,
    /// `CLAUDE_ENV_FILE` for CwdChanged and FileChanged hooks: emptied at each CwdChanged.
    dir_env: PathBuf,
    /// The shell line that loads both, run before each local Bash command.
    env_source: String,
    /// Files a hook asked FileChanged to watch (`watchPaths`), beside the matchers' own.
    watch_paths: Mutex<Vec<PathBuf>>,
    /// [`COMMON_FIELDS`], as the last callback carried them.
    common: Mutex<Object>,
}

impl Callbacks {
    pub fn new(root: &Path, nc: &str) -> Callbacks {
        let dir = match crate::sockets::run_dir() {
            Some(run) => run.clone(),
            None => {
                let dir = std::env::temp_dir().join(format!("claudette-hooks-{}-{}", std::process::id(), NEXT_ENV.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
                let _ = std::fs::create_dir_all(&dir);
                dir
            }
        };
        let (session_env, dir_env) = (dir.join("hook-env"), dir.join("hook-env-dir"));
        let env_source = [&session_env, &dir_env]
            .map(|f| {
                let f = shell_quote(&f.to_string_lossy());
                format!("[ -f {f} ] && . {f}\n")
            })
            .concat();
        Callbacks {
            root: root.to_path_buf(),
            paths: PathMap::new(root),
            nc: nc.to_owned(),
            socket: OnceLock::new(),
            entries: Mutex::new(vec![]),
            session_env,
            dir_env,
            env_source,
            watch_paths: Mutex::default(),
            common: Mutex::default(),
        }
    }

    /// The shell lines that load what hooks wrote to `CLAUDE_ENV_FILE`; the local Bash tool runs
    /// them before each command.
    pub fn env_source(&self) -> &str {
        &self.env_source
    }

    pub fn set_socket(&self, remote: String) {
        let _ = self.socket.set(remote);
    }

    fn register(&self, callback: Callback) -> String {
        let mut entries = self.entries.lock().unwrap();
        let index = match entries.iter().position(|e| *e == callback) {
            Some(i) => i,
            None => {
                entries.push(callback);
                entries.len() - 1
            }
        };
        self.callback_command(index)
    }

    fn callback_command(&self, index: usize) -> String {
        let socket = self.socket.get().map(String::as_str).unwrap_or("");
        format!("{{ printf '%s\\n' {index}; cat; }} | {} -U {}", shell_quote(&self.nc), shell_quote(socket))
    }

    fn original(&self, command: &str) -> Option<Callback> {
        let entries = self.entries.lock().unwrap();
        (0..entries.len()).find(|i| self.callback_command(*i) == command).map(|i| entries[i].clone())
    }

    /// Turns a command entry (a hook, a status line) into a call back to this machine. A hook in exec
    /// form (`command` plus `args`) becomes the callback's shell command, its arguments kept here.
    fn call_back(&self, entry: &mut Object, kind: Kind, event: &str, matcher: Option<String>, plugin: Option<&PluginEnv>) {
        let Some(command) = str_field(entry, "command").map(str::to_owned) else { return };
        let args = entry.get("args").and_then(Value::as_array).map(|a| a.iter().map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).collect());
        let timeout = entry.get("timeout").and_then(Value::as_f64);
        let callback = self.register(Callback { kind, event: event.to_owned(), matcher, command, args, timeout, plugin: plugin.cloned() });
        entry.remove("args");
        entry.insert("command".into(), json!(callback));
    }

    /// The entry as it was before [`Callbacks::call_back`].
    fn put_back(&self, entry: &mut Object) {
        let Some(original) = str_field(entry, "command").and_then(|c| self.original(c)) else { return };
        entry.insert("command".into(), json!(original.command));
        if let Some(args) = original.args {
            entry.insert("args".into(), json!(args));
        }
    }

    /// Command hooks call back; matchers that name a replaced tool name the local one too.
    fn view_hooks(&self, settings: &mut Object, plugin: Option<&PluginEnv>) {
        let mut matcher = None;
        for_each_hook(settings, |event, entry, is_group| {
            if is_group {
                matcher = str_field(entry, "matcher").map(str::to_owned);
                if let Some(m) = matcher.as_deref().map(widen_matcher) {
                    entry.insert("matcher".into(), json!(m));
                }
            } else if str_field(entry, "type") == Some("command") {
                self.call_back(entry, Kind::Hook, event, matcher.clone(), plugin);
            }
        });
    }

    /// The settings file as the remote claude sees it. A file that is not a JSON object passes unchanged.
    pub fn view(&self, bytes: &[u8]) -> Vec<u8> {
        let Ok(Value::Object(mut settings)) = serde_json::from_slice::<Value>(bytes) else { return bytes.to_vec() };
        rule_lists(&mut settings).into_iter().for_each(widen_rules);
        self.view_hooks(&mut settings, None);
        for key in DISPLAY_KEYS {
            if let Some(Value::Object(display)) = settings.get_mut(key) {
                self.call_back(display, Kind::Display, "", None, None);
            }
        }
        for key in PLUGIN_KEYS {
            settings.remove(key);
        }
        serialize(&settings)
    }

    /// A plugin's hooks file (`{"hooks": {…}}`) as the remote claude sees it.
    pub fn plugin_view(&self, bytes: &[u8], plugin: &PluginEnv) -> Vec<u8> {
        let Ok(Value::Object(mut file)) = serde_json::from_slice::<Value>(bytes) else { return bytes.to_vec() };
        self.view_hooks(&mut file, Some(plugin));
        serialize(&file)
    }

    /// Hooks a plugin's manifest declares inline (the event map itself), as the remote claude sees them.
    pub fn plugin_hooks(&self, events: Object, plugin: &PluginEnv) -> Object {
        let mut file = Object::new();
        file.insert("hooks".into(), Value::Object(events));
        self.view_hooks(&mut file, Some(plugin));
        match file.remove("hooks") {
            Some(Value::Object(events)) => events,
            _ => Object::new(),
        }
    }

    /// What the remote claude wrote, turned back into this machine's settings.
    pub fn restore(&self, bytes: &[u8]) -> Vec<u8> {
        let Ok(Value::Object(mut settings)) = serde_json::from_slice::<Value>(bytes) else { return bytes.to_vec() };
        rule_lists(&mut settings).into_iter().for_each(narrow_rules);
        for_each_hook(&mut settings, |_, entry, is_group| {
            if is_group {
                if let Some(m) = str_field(entry, "matcher").map(narrow_matcher) {
                    entry.insert("matcher".into(), json!(m));
                }
            } else {
                self.put_back(entry);
            }
        });
        for key in DISPLAY_KEYS {
            if let Some(Value::Object(display)) = settings.get_mut(key) {
                self.put_back(display);
            }
        }
        serialize(&settings)
    }

    /// What the remote claude wrote to a project settings file, turned back, with the plugin
    /// choices the view left out kept from the file as it is (`current`).
    pub fn restore_file(&self, bytes: &[u8], current: &Path) -> Vec<u8> {
        let restored = self.restore(bytes);
        let Ok(Value::Object(mut settings)) = serde_json::from_slice::<Value>(&restored) else { return restored };
        let Ok(Value::Object(mut before)) = std::fs::read(current).map_err(|_| ()).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|_| ())) else {
            return restored;
        };
        for key in PLUGIN_KEYS {
            if let Some(value) = before.remove(key) {
                settings.insert(key.into(), value);
            }
        }
        serialize(&settings)
    }

    /// Runs callback `index` with the remote claude's input; returns what goes back to it.
    pub async fn run(&self, index: usize, input: &[u8]) -> Vec<u8> {
        let Some(callback) = self.entries.lock().unwrap().get(index).cloned() else {
            return Vec::new();
        };
        if LOCAL_EVENTS.contains(&callback.event.as_str()) {
            return Vec::new();
        }
        let text = String::from_utf8_lossy(input);
        let local = unprefix(&self.paths.map(&text)).into_owned();
        if let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(&local) {
            let mut common = self.common.lock().unwrap();
            for key in COMMON_FIELDS {
                if let Some(value) = fields.get(key) {
                    common.insert(key.into(), value.clone());
                }
            }
        }
        let (code, stdout, stderr) = self.execute(&callback, &local).await;
        if callback.kind == Kind::Display || code == 0 {
            return stdout;
        }
        // The hook ran here, so its exit status becomes the JSON answer that means the same.
        let answer = if code == 2 {
            match callback.event.as_str() {
                "PreToolUse" => json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": stderr } }),
                "PostToolUse" | "Stop" | "SubagentStop" | "UserPromptSubmit" => json!({ "decision": "block", "reason": stderr }),
                _ => json!({ "systemMessage": stderr }),
            }
        } else {
            json!({ "systemMessage": format!("hook `{}` failed (exit {code}): {stderr}", callback.command) })
        };
        answer.to_string().into_bytes()
    }

    /// Runs one command with `input` on its stdin: exit code, stdout, stderr.
    async fn execute(&self, callback: &Callback, input: &str) -> (i32, Vec<u8>, String) {
        let Callback { event, command, args, timeout, plugin, .. } = callback;
        let limit = Duration::from_secs_f64(timeout.unwrap_or(DEFAULT_TIMEOUT).max(1.0));
        // Exec form, as Claude Code runs it: no shell, the placeholders put in as plain text.
        let root = self.root.to_string_lossy();
        let place = |s: &str| {
            let s = s.replace("${CLAUDE_PROJECT_DIR}", &root);
            match plugin {
                Some(p) => crate::plugins::fill_options(&s.replace("${CLAUDE_PLUGIN_ROOT}", &p.root.to_string_lossy()).replace("${CLAUDE_PLUGIN_DATA}", &p.data.to_string_lossy()), &p.options),
                None => s,
            }
        };
        let env_file = match event.as_str() {
            "SessionStart" | "Setup" => Some(&self.session_env),
            "CwdChanged" | "FileChanged" => Some(&self.dir_env),
            _ => None,
        };
        let outcome = async {
            let mut process = match args {
                Some(args) => {
                    let mut c = Command::new(place(command));
                    c.args(args.iter().map(|a| place(a)));
                    c
                }
                None => {
                    let mut c = Command::new("sh");
                    c.arg("-c").arg(command);
                    c
                }
            };
            process.envs(local_env().iter().map(|(k, v)| (k, v)));
            if let Some(file) = env_file {
                process.env("CLAUDE_ENV_FILE", file);
            }
            if let Some(plugin) = plugin {
                process.env("CLAUDE_PLUGIN_ROOT", &plugin.root).env("CLAUDE_PLUGIN_DATA", &plugin.data);
                for (key, value) in &plugin.options {
                    let name: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c.to_ascii_uppercase() } else { '_' }).collect();
                    process.env(format!("CLAUDE_PLUGIN_OPTION_{name}"), value);
                }
            }
            let mut child = process
                .current_dir(&self.root)
                .env("CLAUDE_PROJECT_DIR", &self.root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(input.as_bytes()).await;
            }
            tokio::time::timeout(limit, child.wait_with_output()).await.map_err(|_| std::io::Error::other(format!("timed out after {}s", limit.as_secs())))?
        };
        let (code, stdout, stderr) = match outcome.await {
            Ok(out) => (out.status.code().unwrap_or(1), out.stdout, String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            Err(e) => (1, Vec::new(), e.to_string()),
        };
        if code == 0 && ENV_EVENTS.contains(&event.as_str()) {
            self.take_watch_paths(&stdout);
        }
        (code, stdout, stderr)
    }

    /// A hook's `watchPaths`, which replace the files FileChanged watches beside its matchers'.
    fn take_watch_paths(&self, stdout: &[u8]) {
        let Ok(Value::Object(out)) = serde_json::from_slice::<Value>(stdout) else { return };
        let paths = out.get("watchPaths").or_else(|| out.get("hookSpecificOutput").and_then(|h| h.get("watchPaths")));
        if let Some(Value::Array(paths)) = paths {
            *self.watch_paths.lock().unwrap() = paths.iter().filter_map(Value::as_str).map(PathBuf::from).collect();
        }
    }

    /// Runs the hooks of an event claudette fires itself, all at once, as Claude Code runs them.
    async fn fire(&self, event: &str, subject: Option<&str>, cwd: &Path, fields: Object) {
        let hooks: Vec<Callback> = {
            let entries = self.entries.lock().unwrap();
            entries.iter().filter(|e| e.kind == Kind::Hook && e.event == event && matcher_matches(e.matcher.as_deref(), subject)).cloned().collect()
        };
        if hooks.is_empty() {
            return;
        }
        let mut input = self.common.lock().unwrap().clone();
        input.insert("cwd".into(), json!(cwd.to_string_lossy()));
        input.insert("hook_event_name".into(), json!(event));
        input.extend(fields);
        let input = Value::Object(input).to_string();
        futures_util::future::join_all(hooks.iter().map(|hook| self.execute(hook, &input))).await;
    }

    /// The local Bash tool moved from `old` to `new`: what CwdChanged and FileChanged hooks set for
    /// the last folder goes, and CwdChanged fires.
    pub async fn cwd_changed(&self, old: &Path, new: &Path) {
        let _ = std::fs::remove_file(&self.dir_env);
        let mut fields = Object::new();
        fields.insert("old_cwd".into(), json!(old.to_string_lossy()));
        fields.insert("new_cwd".into(), json!(new.to_string_lossy()));
        self.fire("CwdChanged", None, new, fields).await;
    }

    /// The files FileChanged watches: each name its matchers list, in the shell's folder, and
    /// whatever hooks asked for with `watchPaths`.
    fn watched(&self, cwd: &Path) -> BTreeSet<PathBuf> {
        let mut files: BTreeSet<PathBuf> = self.watch_paths.lock().unwrap().iter().cloned().collect();
        for entry in self.entries.lock().unwrap().iter().filter(|e| e.kind == Kind::Hook && e.event == "FileChanged") {
            let names = entry.matcher.as_deref().unwrap_or("").split('|').map(str::trim).filter(|n| !n.is_empty());
            files.extend(names.map(|n| cwd.join(n)));
        }
        files
    }

    /// Fires FileChanged for the watched files as they are created, modified or deleted, until the
    /// session ends. A file newly watched (a new folder, a new hook) is taken as it is.
    pub async fn watch(&self, ws: &crate::tools::Workspace) {
        let mut seen: HashMap<PathBuf, Option<SystemTime>> = HashMap::new();
        let mut tick = tokio::time::interval(WATCH_INTERVAL);
        loop {
            tick.tick().await;
            let cwd = ws.cwd();
            let files = self.watched(&cwd);
            seen.retain(|file, _| files.contains(file));
            for file in files {
                let now = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
                let Some(before) = seen.insert(file.clone(), now) else { continue };
                let change = match (before, now) {
                    (None, Some(_)) => "add",
                    (Some(_), None) => "unlink",
                    (Some(a), Some(b)) if a != b => "change",
                    _ => continue,
                };
                let mut fields = Object::new();
                fields.insert("file_path".into(), json!(file.to_string_lossy()));
                fields.insert("event".into(), json!(change));
                let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.fire("FileChanged", Some(&name), &cwd, fields).await;
            }
        }
    }
}

/// Whether a hook group's matcher selects `subject` (none: the event has no matchers), by Claude
/// Code's rules: empty or `*` matches all, letters, digits, `_` and `|` list exact names, anything
/// else is a regular expression.
fn matcher_matches(matcher: Option<&str>, subject: Option<&str>) -> bool {
    let (Some(matcher), Some(subject)) = (matcher, subject) else { return true };
    if matcher.is_empty() || matcher == "*" {
        true
    } else if matcher.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|') {
        matcher.split('|').any(|m| m == subject)
    } else {
        regex::Regex::new(matcher).is_ok_and(|re| re.is_match(subject))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_adapts_hooks_and_rules_and_writes_come_back_as_they_were() {
        let cb = Callbacks::new(Path::new("/p"), "nc");
        cb.set_socket("/tmp/s.sock".into());
        let original = json!({
            "permissions": { "allow": ["Write", "Bash(npm test:*)", "WebFetch"], "deny": ["Read"] },
            "hooks": { "PostToolUse": [{ "matcher": "Edit|Write", "hooks": [
                { "type": "command", "command": "prettier --write", "timeout": 5 },
                { "type": "command", "command": "node", "args": ["${CLAUDE_PROJECT_DIR}/fmt.js", "--fix"] },
                { "type": "http", "url": "https://example.com/hook" }
            ] }] },
            "statusLine": { "type": "command", "command": "echo hi" },
            "subagentStatusLine": { "type": "command", "command": "jq -c ." },
            "fileSuggestion": { "type": "command", "command": "git ls-files" }
        });
        let view: Value = serde_json::from_slice(&cb.view(original.to_string().as_bytes())).unwrap();
        assert_eq!(view["permissions"]["allow"], json!(["Write", "mcp__cc__Write", "Bash(npm test:*)", "WebFetch"]));
        assert_eq!(view["hooks"]["PostToolUse"][0]["matcher"], "Edit|Write|mcp__cc__Edit|mcp__cc__Write");
        let hooks = &view["hooks"]["PostToolUse"][0]["hooks"];
        assert_eq!(hooks[0]["command"], "{ printf '%s\\n' 0; cat; } | 'nc' -U '/tmp/s.sock'");
        // An exec-form hook becomes a shell-form callback: its arguments stay here.
        assert_eq!(hooks[1]["command"], "{ printf '%s\\n' 1; cat; } | 'nc' -U '/tmp/s.sock'");
        assert!(hooks[1].get("args").is_none());
        // An HTTP hook runs where claude runs.
        assert_eq!(hooks[2], original["hooks"]["PostToolUse"][0]["hooks"][2]);
        for key in DISPLAY_KEYS {
            assert!(view[key]["command"].as_str().unwrap().contains("-U '/tmp/s.sock'"), "{key}");
        }

        // The remote claude adds a rule under the local tool's name and writes the file back.
        let mut written = view.clone();
        written["permissions"]["allow"].as_array_mut().unwrap().push(json!("mcp__cc__Edit"));
        let restored: Value = serde_json::from_slice(&cb.restore(written.to_string().as_bytes())).unwrap();
        let mut expected = original.clone();
        expected["permissions"]["allow"].as_array_mut().unwrap().push(json!("Edit"));
        assert_eq!(restored, expected);
    }

    #[test]
    fn user_settings_lose_this_machines_login_plugins_and_paths() {
        let user = json!({
            "model": "opus",
            "apiKeyHelper": "/bin/key",
            "enabledPlugins": { "x@y": true },
            "env": { "ANTHROPIC_API_KEY": "sk", "CLAUDE_CODE_OAUTH_TOKEN": "t", "ANTHROPIC_BASE_URL": "https://gateway", "ANTHROPIC_MODEL": "claude-opus-5-5", "CLAUDE_CONFIG_DIR": "/Users/me/.c", "FOO": "1" },
            "permissions": { "allow": ["Write"], "defaultMode": "acceptEdits", "additionalDirectories": ["/Users/me/x"] },
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "user" }] }] }
        });
        assert_eq!(
            Value::Object(user_part(user.as_object().unwrap().clone())),
            json!({
                "model": "opus",
                "env": { "ANTHROPIC_BASE_URL": "https://gateway", "ANTHROPIC_MODEL": "claude-opus-5-5", "FOO": "1" },
                "permissions": { "allow": ["Write"], "defaultMode": "acceptEdits" },
                "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "user" }] }] }
            })
        );
    }

    #[tokio::test]
    async fn a_blocking_hook_becomes_a_denial_with_its_reason() {
        let dir = std::env::temp_dir();
        let cb = Callbacks::new(&dir, "nc");
        cb.view(json!({ "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "cat >/dev/null; echo nope >&2; exit 2" }] }] } }).to_string().as_bytes());
        let out = cb.run(0, br#"{"hook_event_name":"PreToolUse","tool_name":"mcp__cc__Bash"}"#).await;
        let answer: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(answer["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(answer["hookSpecificOutput"]["permissionDecisionReason"], "nope");
    }

    #[tokio::test]
    async fn an_exec_form_hook_runs_here_with_its_arguments_as_written() {
        let dir = std::env::temp_dir().join(format!("ct-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let cb = Callbacks::new(&dir, "nc");
        // No shell: `$HOME` and the space stay as written; the placeholder is this machine's folder.
        cb.view(json!({ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "printf", "args": ["%s|%s", "${CLAUDE_PROJECT_DIR}/x", "$HOME a"] }] }] } }).to_string().as_bytes());
        let out = cb.run(0, br#"{"hook_event_name":"Stop"}"#).await;
        assert_eq!(String::from_utf8(out).unwrap(), format!("{}/x|$HOME a", dir.display()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cwd_and_file_hooks_fire_here_and_their_env_reaches_bash() {
        use crate::tools::test_support::{call, text, workspace};
        use std::sync::Arc;
        let (dir, ws) = workspace(&[("sub/.envrc", "a")]);
        let ws = Arc::new(ws);
        let cb = Arc::new(Callbacks::new(&dir, "nc"));
        let hook = |command: &str| json!([{ "hooks": [{ "type": "command", "command": command }] }]);
        let mut file_changed = hook(r#"cat > "$CLAUDE_PROJECT_DIR/changed.json""#);
        file_changed[0]["matcher"] = json!(".envrc");
        cb.view(
            json!({ "hooks": {
                "SessionStart": hook(r#"echo 'export S=1' >> "$CLAUDE_ENV_FILE""#),
                "CwdChanged": hook(r#"sed -n 's/.*"new_cwd":"\([^"]*\)".*/export D="\1"/p' >> "$CLAUDE_ENV_FILE""#),
                "FileChanged": file_changed,
            } })
            .to_string()
            .as_bytes(),
        );
        // The remote claude fires SessionStart; its own CwdChanged and FileChanged go nowhere.
        for index in 0..3 {
            cb.run(index, br#"{"hook_event_name":"SessionStart","session_id":"s1"}"#).await;
        }
        ws.set_hooks(cb.clone());
        let watcher = tokio::spawn({
            let (cb, ws) = (cb.clone(), ws.clone());
            async move { cb.watch(&ws).await }
        });

        call(&ws, "Bash", json!({ "command": "cd sub" })).await;
        let sub = dir.join("sub");
        assert_eq!(text(&call(&ws, "Bash", json!({ "command": "echo \"$S $D\"" })).await), format!("1 {}", sub.display()));

        // `.envrc` is watched in the shell's folder; the hook gets the change, with the session's fields.
        tokio::time::sleep(WATCH_INTERVAL * 2).await;
        std::fs::write(sub.join(".envrc"), "b").unwrap();
        let changed = dir.join("changed.json");
        for _ in 0..50 {
            if changed.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let input: Value = serde_json::from_slice(&std::fs::read(&changed).unwrap()).unwrap();
        assert_eq!(input["hook_event_name"], "FileChanged");
        assert_eq!(input["file_path"], sub.join(".envrc").to_string_lossy().as_ref());
        assert_eq!(input["event"], "change");
        assert_eq!(input["session_id"], "s1");
        watcher.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
