//! Reading claude's arguments and rewriting them for the remote claude.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::Value;

use crate::json::Object;
use crate::log::warn;
use crate::names::{LOCAL_TOOLS, PREFIX, REMOTE_TOOLS, UNASKED_TOOLS, is_local_tool};

/// What the caller asked of claude, with everything that names this machine taken out.
#[derive(Default, Debug)]
pub struct ParsedArgs {
    /// Arguments that pass to the remote claude unchanged, in order (positional prompt included).
    pub rest: Vec<String>,
    /// Local directories from `--add-dir`. The remote machine has none of them.
    pub add_dirs: Vec<PathBuf>,
    pub permission_mode: Option<String>,
    /// `--name`, still passed to claude: what this machine's other sessions call this one.
    pub name: Option<String>,
    /// The caller asked for transcript mirror frames itself (an SDK host with a sessionStore).
    pub host_mirror: bool,
    /// MCP servers that run here: the caller's `--mcp-config`, then this machine's own configuration.
    pub config_servers: BTreeMap<String, Object>,
    /// MCP servers this machine's configuration places on the remote machine; the remote claude starts them.
    pub remote_servers: BTreeMap<String, Object>,
    /// The caller passed `--strict-mcp-config`: only its own servers, none from configuration.
    pub strict_mcp: bool,
    /// The caller's `--setting-sources`, when given.
    pub setting_sources: Option<String>,
    /// Local tools some `Read(…)` or `Edit(…)` ask rule covers: their calls must reach a permission
    /// check even when a rule or the mode would approve them.
    pub ask_tools: Vec<&'static str>,
    /// Local tools a bare allow rule (`Edit`) approves, from the settings and `--allowedTools`.
    pub tool_allows: Vec<String>,
    /// `--settings`, read from this machine when it names a file.
    pub settings: Option<Object>,
    pub append_system_prompt: Vec<String>,
    /// The raw `--tools` value, when given.
    pub tools: Option<String>,
    pub allowed: Vec<String>,
    pub disallowed: Vec<String>,
    /// The local tools this session may use: the caller's --tools and --disallowedTools applied.
    pub local_tools: Vec<&'static str>,
    /// Bash rule contents from the caller's allow and deny lists (`git diff:*`).
    pub bash_allow: Vec<String>,
    pub bash_deny: Vec<String>,
}

/// `--flag=value` or `--flag value`; returns the flag name and the inline value, if any.
fn split(token: &str) -> (&str, Option<&str>) {
    match token.find('=') {
        Some(eq) if token.starts_with("--") && eq > 0 => (&token[..eq], Some(&token[eq + 1..])),
        _ => (token, None),
    }
}

/// True when the host speaks the SDK's stream-json protocol both ways.
pub fn is_stream_json_session(argv: &[String]) -> bool {
    let value = |flag: &str| {
        for (i, token) in argv.iter().enumerate() {
            let (name, inline) = split(token);
            if name == flag {
                return inline.map(str::to_owned).or_else(|| argv.get(i + 1).cloned());
            }
        }
        None
    };
    value("--input-format").as_deref() == Some("stream-json") && value("--output-format").as_deref() == Some("stream-json")
}

/// Claude Code's own subcommands (`claude --help`), and flags that only print. They run remotely as is.
/// `mcp` and `plugin` are handled here, and the background-session commands (`agents`, `attach`,
/// `stop`, …) are refused, before this list is consulted.
const COMMANDS: [&str; 11] = ["auth", "auto-mode", "doctor", "gateway", "import", "install", "purge", "setup-token", "ultrareview", "update", "upgrade"];
const PRINT_ONLY: [&str; 4] = ["-v", "--version", "-h", "--help"];

pub fn is_claude_command(argv: &[String]) -> bool {
    argv.first().is_some_and(|a| COMMANDS.contains(&a.as_str())) || argv.iter().any(|a| PRINT_ONLY.contains(&a.as_str()))
}

/// Reads a `--mcp-config` / `--settings` value: inline JSON, or a path to a JSON file on this machine.
fn read_json_value(flag: &str, value: &str, cwd: &Path) -> anyhow::Result<Value> {
    if value.trim_start().starts_with('{') {
        return serde_json::from_str(value).with_context(|| format!("{flag}: invalid JSON"));
    }
    let path = cwd.join(value);
    if !path.exists() {
        bail!("{flag} {value}: no such file on this machine");
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).with_context(|| format!("{flag} {value}: invalid JSON"))
}

const VARIADIC: [&str; 7] = ["--add-dir", "--mcp-config", "--allowedTools", "--allowed-tools", "--disallowedTools", "--disallowed-tools", "--tools"];
const SINGLE: [&str; 5] = ["--settings", "--project-config-root", "--permission-mode", "--append-system-prompt", "--setting-sources"];

fn rule_list(values: &[String]) -> Vec<String> {
    values.iter().flat_map(|v| v.split(',')).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

fn bash_rule(rule: &str) -> Option<String> {
    rule.strip_prefix("Bash(").and_then(|r| r.strip_suffix(')')).map(str::to_owned)
}

pub fn parse_args(argv: &[String], cwd: &Path) -> anyhow::Result<ParsedArgs> {
    let mut p = ParsedArgs::default();
    let mut i = 0;
    while i < argv.len() {
        let token = &argv[i];
        let (name, inline) = split(token);
        if name == "--session-mirror" || name == "--strict-mcp-config" {
            if name == "--session-mirror" {
                p.host_mirror = true;
            } else {
                p.strict_mcp = true;
            }
            i += 1;
            continue;
        }
        if name == "--name" {
            p.name = inline.map(str::to_owned).or_else(|| argv.get(i + 1).cloned());
        }
        if !VARIADIC.contains(&name) && !SINGLE.contains(&name) {
            p.rest.push(token.clone());
            i += 1;
            continue;
        }
        let mut values = Vec::new();
        if let Some(v) = inline {
            values.push(v.to_owned());
        } else if VARIADIC.contains(&name) {
            while i + 1 < argv.len() && !argv[i + 1].starts_with('-') {
                i += 1;
                values.push(argv[i].clone());
            }
        } else if i + 1 < argv.len() {
            i += 1;
            values.push(argv[i].clone());
        }
        match name {
            "--add-dir" => p.add_dirs.extend(values.iter().map(|d| normalize(&cwd.join(d)))),
            "--allowedTools" | "--allowed-tools" => p.allowed.extend(rule_list(&values)),
            "--disallowedTools" | "--disallowed-tools" => p.disallowed.extend(rule_list(&values)),
            "--tools" => p.tools = Some(values.join(",")),
            "--mcp-config" => {
                for value in &values {
                    let parsed = read_json_value(name, value, cwd)?;
                    if let Some(servers) = parsed.get("mcpServers").and_then(Value::as_object) {
                        for (server, config) in servers {
                            if let Some(config) = config.as_object() {
                                p.config_servers.insert(server.clone(), config.clone());
                            }
                        }
                    }
                }
            }
            "--settings" => {
                if let Some(value) = values.first()
                    && let Value::Object(settings) = read_json_value(name, value, cwd)?
                {
                    p.settings = Some(settings);
                }
            }
            "--append-system-prompt" => p.append_system_prompt.extend(values),
            "--project-config-root" => warn(&format!(
                "dropping --project-config-root {}: it names a directory on this machine",
                values.first().map(String::as_str).unwrap_or("")
            )),
            "--permission-mode" => p.permission_mode = values.into_iter().next(),
            "--setting-sources" => p.setting_sources = Some(values.into_iter().next().unwrap_or_default()),
            _ => {}
        }
        i += 1;
    }

    // The caller's tool restrictions, as they would apply to the built-ins the local tools replace.
    let listed: Option<Vec<String>> = match p.tools.as_deref() {
        None | Some("default") => None,
        Some(list) => Some(rule_list(&[list.to_owned()])),
    };
    let bare_denied: Vec<&String> = p.disallowed.iter().filter(|r| !r.contains('(')).collect();
    p.local_tools = LOCAL_TOOLS
        .iter()
        .copied()
        .filter(|t| listed.as_ref().is_none_or(|l| l.iter().any(|x| x == t)) && !bare_denied.iter().any(|d| d.as_str() == *t))
        .collect();
    p.bash_allow = p.allowed.iter().filter_map(|r| bash_rule(r)).collect();
    p.bash_deny = p.disallowed.iter().filter_map(|r| bash_rule(r)).collect();
    Ok(p)
}

/// Lexically normalizes a path (`a/../b` → `b`) without touching the file system.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Extra arguments a mode adds for the remote claude.
#[derive(Default)]
pub struct RemoteExtras {
    /// Ask claude for transcript mirror frames (SDK mode).
    pub mirror: bool,
    /// The MCP config file on the remote machine. With or without it, claude uses no other servers.
    pub mcp_config: Option<String>,
    /// The session's settings and the appended system prompt, as files on the remote machine: on
    /// the command line, anyone there could read them (`ps`).
    pub settings_file: Option<String>,
    pub append_system_prompt_file: Option<String>,
    /// The folder the session's plugins are mounted in on the remote machine, each loaded from it.
    pub plugin_dir: Option<String>,
}

fn dedup(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// The remote claude's built-in tools: the allowlist, narrowed to the caller's `--tools` when given.
fn remote_tools(p: &ParsedArgs) -> String {
    match p.tools.as_deref() {
        None | Some("default") => REMOTE_TOOLS.join(","),
        Some(list) => rule_list(&[list.to_owned()]).into_iter().filter(|t| REMOTE_TOOLS.contains(&t.as_str())).collect::<Vec<_>>().join(","),
    }
}

/// The settings claude loads, as the caller asked: the user's come from this machine through the
/// session's own Claude Code folder, the project's through the workspace.
fn remote_setting_sources(p: &ParsedArgs) -> String {
    match p.setting_sources.as_deref() {
        None => "user,project,local".into(),
        Some(list) => list.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(","),
    }
}

/// The arguments for the remote claude: an allowlist of built-in tools, only the MCP servers
/// claudette passes, the local read tools and the caller's bare local-tool approvals pre-approved
/// under their MCP names, local paths taken out. Every added flag uses `--flag=value`, so none can
/// swallow a positional prompt.
pub fn remote_args(p: &ParsedArgs, extras: &RemoteExtras) -> Vec<String> {
    let mut args = p.rest.clone();
    if let Some(mode) = &p.permission_mode {
        args.push(format!("--permission-mode={mode}"));
    }
    args.push(format!("--tools={}", remote_tools(p)));
    args.push(format!("--setting-sources={}", remote_setting_sources(p)));
    args.push("--strict-mcp-config".into());
    if !p.disallowed.is_empty() {
        args.push(format!("--disallowedTools={}", dedup(p.disallowed.iter().cloned()).join(",")));
    }
    let offered = |t: &str| p.local_tools.contains(&t);
    let approved_local = p.allowed.iter().filter(|r| is_local_tool(r) && offered(r)).map(|t| format!("{PREFIX}{t}"));
    let reads = UNASKED_TOOLS.iter().filter(|t| offered(t)).map(|t| format!("{PREFIX}{t}"));
    let allowed = dedup(p.allowed.iter().cloned().chain(reads).chain(approved_local));
    if !allowed.is_empty() {
        args.push(format!("--allowedTools={}", allowed.join(",")));
    }
    if let Some(file) = &extras.settings_file {
        args.push(format!("--settings={file}"));
    }
    if let Some(file) = &extras.append_system_prompt_file {
        args.push(format!("--append-system-prompt-file={file}"));
    }
    if let Some(file) = &extras.mcp_config {
        args.push(format!("--mcp-config={file}"));
    }
    if let Some(dir) = &extras.plugin_dir {
        args.push(format!("--plugin-dir={dir}"));
    }
    if extras.mirror {
        args.push("--session-mirror".into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    const SDK_BASE: [&str; 5] = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json"];

    #[test]
    fn modes() {
        assert!(is_stream_json_session(&s(&SDK_BASE)));
        assert!(is_stream_json_session(&s(&["--output-format=stream-json", "--input-format=stream-json"])));
        assert!(!is_stream_json_session(&s(&["-p", "--output-format", "stream-json"])));
        assert!(is_claude_command(&s(&["auth", "status"])));
        assert!(is_claude_command(&s(&["-p", "--help"])));
        assert!(!is_claude_command(&s(&[])));
        assert!(!is_claude_command(&s(&["fix the auth bug"])));
        assert!(!is_claude_command(&s(&["-p", "auth"])));
    }

    #[test]
    fn an_sdk_hosts_arguments_come_out_ready_for_the_remote_claude() {
        let servers = r#"{"mcpServers":{"local":{"type":"stdio","command":"node","args":["s.js"]}}}"#;
        let mut argv = s(&SDK_BASE);
        argv.extend(s(&[
            "--model", "claude-opus-5-5", "--permission-mode=acceptEdits", "--add-dir=/work/a", "--add-dir=/work/b", "--mcp-config", servers,
            "--disallowedTools=WebFetch", "--allowedTools=Bash(git:*)", "--resume=abc-123", "--session-mirror", "--include-partial-messages",
        ]));
        let p = parse_args(&argv, Path::new("/work/project")).unwrap();
        let out = remote_args(&p, &RemoteExtras { mirror: true, ..Default::default() });
        let mut head = s(&SDK_BASE);
        head.extend(s(&["--model", "claude-opus-5-5", "--resume=abc-123", "--include-partial-messages"]));
        assert_eq!(out[..9], head[..]);
        assert!(!out.iter().any(|a| a.starts_with("--add-dir") || a.starts_with("--mcp-config")));
        assert_eq!(out.iter().filter(|a| *a == "--session-mirror").count(), 1);
        assert!(out.contains(&"--disallowedTools=WebFetch".into()));
        assert!(out.contains(&"--tools=Task,Agent,Skill,WebFetch,WebSearch,ToolSearch,TodoWrite,TaskCreate,TaskGet,TaskList,TaskUpdate,TaskOutput,TaskStop,ExitPlanMode,EnterPlanMode,AskUserQuestion,Workflow,ReportFindings,ScheduleWakeup,CronCreate,CronDelete,CronList,ListMcpResourcesTool,ReadMcpResourceTool,WaitForMcpServers".into()));
        assert!(out.contains(&"--setting-sources=user,project,local".into()));
        assert!(out.contains(&"--strict-mcp-config".into()));
        assert!(out.contains(&"--allowedTools=Bash(git:*),mcp__cc__Read,mcp__cc__Glob,mcp__cc__Grep,mcp__cc__ListAgents,mcp__cc__SendMessage".into()));
        assert_eq!(p.add_dirs, [PathBuf::from("/work/a"), PathBuf::from("/work/b")]);
        assert_eq!(p.bash_allow, ["git:*"]);
        assert!(p.host_mirror);
        assert!(p.config_servers.contains_key("local"));
    }

    #[test]
    fn a_positional_prompt_stays_and_added_flags_cannot_swallow_it() {
        let p = parse_args(&s(&["--model", "opus", "fix the bug", "--add-dir", "../shared", "/abs", "--verbose"]), Path::new("/work/project")).unwrap();
        assert_eq!(p.add_dirs, [PathBuf::from("/work/shared"), PathBuf::from("/abs")]);
        let out = remote_args(&p, &RemoteExtras { append_system_prompt_file: Some("/tmp/p.md".into()), mcp_config: Some("/tmp/x.json".into()), ..Default::default() });
        assert_eq!(out[..4], s(&["--model", "opus", "fix the bug", "--verbose"])[..]);
        // A flag that takes no value cannot swallow anything either.
        assert!(out[4..].iter().all(|a| a.starts_with("--") && (a.contains('=') || a == "--strict-mcp-config")));
    }

    #[test]
    fn the_callers_tool_restrictions_apply_to_the_local_tools() {
        let investigate = parse_args(
            &s(&[
                "-p", "--tools", "Read,Grep,Glob,Bash", "--allowedTools", "Read,Grep,Glob,Bash(git diff:*),Bash(git log:*)", "--disallowedTools",
                "Bash(git * -o *)", "--permission-mode", "dontAsk",
            ]),
            Path::new("/w"),
        )
        .unwrap();
        assert_eq!(investigate.local_tools, ["Read", "Glob", "Grep", "Bash"]);
        assert_eq!(investigate.bash_allow, ["git diff:*", "git log:*"]);
        assert_eq!(investigate.bash_deny, ["git * -o *"]);
        assert!(parse_args(&s(&["-p", "--tools", "", "title"]), Path::new("/w")).unwrap().local_tools.is_empty());
        // The remote claude gets the caller's built-ins that are allowed there, and never the replaced ones.
        assert!(remote_args(&investigate, &RemoteExtras::default()).contains(&"--tools=".into()));
        let web = parse_args(&s(&["--tools", "Read,WebFetch,Monitor"]), Path::new("/w")).unwrap();
        assert!(remote_args(&web, &RemoteExtras::default()).contains(&"--tools=WebFetch".into()));
        assert_eq!(parse_args(&s(&["--tools=default"]), Path::new("/w")).unwrap().local_tools.len(), 9);
        let approved = parse_args(&s(&["--allowedTools", "Edit,Bash", "--disallowedTools", "Bash"]), Path::new("/w")).unwrap();
        assert_eq!(
            remote_args(&approved, &RemoteExtras::default()).iter().find(|a| a.starts_with("--allowedTools")).unwrap(),
            "--allowedTools=Edit,Bash,mcp__cc__Read,mcp__cc__Glob,mcp__cc__Grep,mcp__cc__ListAgents,mcp__cc__SendMessage,mcp__cc__Edit"
        );
    }

    #[test]
    fn the_remote_users_settings_never_load() {
        let sources = |argv: &[&str]| {
            let p = parse_args(&s(argv), Path::new("/w")).unwrap();
            remote_args(&p, &RemoteExtras::default()).into_iter().find(|a| a.starts_with("--setting-sources=")).unwrap()
        };
        assert_eq!(sources(&[]), "--setting-sources=user,project,local");
        assert_eq!(sources(&["--setting-sources", "user,project"]), "--setting-sources=user,project");
        assert_eq!(sources(&["--setting-sources", ""]), "--setting-sources=");
        let strict = parse_args(&s(&["--strict-mcp-config", "-p", "x"]), Path::new("/w")).unwrap();
        assert!(strict.strict_mcp);
        assert_eq!(remote_args(&strict, &RemoteExtras::default()).iter().filter(|a| *a == "--strict-mcp-config").count(), 1);
    }

    #[test]
    fn a_config_file_that_is_not_on_this_machine_fails_loudly() {
        let err = parse_args(&s(&["--mcp-config", "/no/such/file.json"]), Path::new("/")).unwrap_err();
        assert!(err.to_string().contains("no such file on this machine"));
    }
}
