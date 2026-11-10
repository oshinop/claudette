//! claude used directly: interactively, or with `-p`. There is no SDK host on a pipe to talk to, so
//! claude reaches the local tools through a remote stdio MCP shim; tool calls then use a Unix socket
//! the run's holder carries back here over the link (see [`crate::sockets`]).
//! A PreToolUse hook, over a socket of its own, makes the permission decisions claude makes for its
//! own tools (acceptEdits, Bash rules), which it does not make for MCP tools.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::args::{RemoteExtras, parse_args, remote_args};
use crate::config::Config;
use crate::json::{Object, str_field};
use crate::log::{trace, warn};
use crate::mcp::Endpoint;
use crate::mcp::bridge::Bridge;
use crate::mcp::local::LocalTools;
use crate::names::{EDIT_TOOLS, PREFIX, SERVER, is_local_tool};
use crate::mount::workdir;
use crate::peers::Status;
use crate::permissions::policy::{Decision, Policy, hook_output};
use crate::prompt::environment_prompt;
use crate::run::{deliver, is_tty};
use crate::sockets::{Forwards, SocketHub};
use crate::ssh::{claude_command, forwarded_env, shell_quote};
use crate::tools::{Workspace, kill_all_commands};

/// Adds claudette's hooks to the caller's settings, keeping the caller's own: a PreToolUse hook
/// for the tools claudette decides for (Bash, the edit tools, and the read tools an ask rule
/// covers), and the hooks that tell it whether the session is working, idle or waiting for input.
fn with_hook(settings: Option<Object>, pretool_command: &str, notify_command: &str, ask_tools: &[&str]) -> Object {
    let mut tools: Vec<&str> = std::iter::once("Bash").chain(EDIT_TOOLS).collect();
    tools.extend(ask_tools.iter().filter(|t| !tools.contains(t)).collect::<Vec<_>>());
    let hooked: Vec<String> = tools.iter().map(|t| format!("{PREFIX}{t}")).collect();
    let mut base = settings.unwrap_or_default();
    let hooks = base.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().expect("an object");
    for (event, matcher, command) in [
        ("PreToolUse", Some(hooked.join("|")), pretool_command),
        ("UserPromptSubmit", None, notify_command),
        ("Stop", None, notify_command),
        ("Notification", Some("permission_prompt".to_owned()), notify_command),
    ] {
        let list = hooks.entry(event).or_insert_with(|| json!([]));
        if !list.is_array() {
            *list = json!([]);
        }
        let mut entry = json!({ "hooks": [{ "type": "command", "command": command, "timeout": 30 }] });
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        list.as_array_mut().expect("an array").push(entry);
    }
    base
}

pub async fn direct(config: &Config, argv: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let mut p = parse_args(argv, cwd)?;
    let path_rules = crate::settings::apply_here(&mut p, cwd);
    crate::mcp::config::add_servers(&mut p, cwd);
    let ws = Arc::new(Workspace::new(cwd.to_path_buf(), p.add_dirs.clone(), path_rules));
    let env_prompt = tokio::spawn(environment_prompt(cwd.to_path_buf(), p.add_dirs.clone()));

    let hub = SocketHub::new()?;
    let mut forwards = Forwards::new();
    let (to_inbox, inbox) = tokio::sync::mpsc::unbounded_channel();
    let agent = crate::peers::Agent::new(&forwards.run, p.name.clone(), cwd, crate::peers::queued(to_inbox));
    crate::peers::register(agent.clone());
    hub.serve_agent(agent.clone())?;
    let mut servers = Object::new();
    let mut bridges: Vec<Arc<Bridge>> = vec![];

    let result = async {
        let delivered = deliver(&hub, &mut forwards, &mut p, &ws, config)?;
        if !p.local_tools.is_empty() {
            let local = Arc::new(LocalTools::new(ws.clone(), &p.local_tools));
            let (path, _) = hub.serve_mcp(SERVER, Endpoint::Local(local.clone()))?;
            let socket = forwards.add(&path);
            let manifest = forwards.file("cc-manifest.json", local.manifest().to_string().into_bytes());
            let command = format!("exec \"$CLAUDETTE_HELPER\" __helper mcp {} {}", shell_quote(&socket), shell_quote(&manifest));
            servers.insert(SERVER.into(), json!({ "type": "stdio", "command": "sh", "args": ["-c", command] }));
        }
        for (name, server_config) in &p.config_servers {
            if name == SERVER {
                warn(&format!("ignoring the MCP server \"{name}\": the name is taken by claudette"));
                continue;
            }
            let bridge = Arc::new(Bridge::new(name, server_config.clone(), cwd.to_path_buf()));
            bridges.push(bridge.clone());
            let (path, notify) = hub.serve_mcp(name, Endpoint::Bridge(bridge.clone()))?;
            bridge.set_notify(notify);
            servers.insert(name.clone(), json!({ "type": "stdio", "command": config.remote_nc, "args": ["-U", forwards.add(&path)] }));
        }
        // Remote servers: the remote claude starts them from their own definitions.
        for (name, server_config) in &p.remote_servers {
            if name == SERVER {
                warn(&format!("ignoring the MCP server \"{name}\": the name is taken by claudette"));
                continue;
            }
            servers.insert(name.clone(), Value::Object(server_config.clone()));
        }
        servers.insert(crate::helper::INBOX_SERVER.into(), Value::Object(crate::helper::inbox_server(&forwards.run)));

        let policy = Arc::new(Policy::new(p.permission_mode.as_deref().unwrap_or("default"), p.bash_allow.clone(), p.bash_deny.clone(), p.tool_allows.clone()));
        let hook_ws = ws.clone();
        // Events nobody waits on can arrive out of order: an older one's status is let be.
        let latest = std::sync::atomic::AtomicU64::new(0);
        let hook = hub.serve_hook(Arc::new(move |input: &Object| {
            let sent = input.get(crate::helper::holder::SENT_AT).and_then(Value::as_u64);
            if sent.is_none_or(|sent| latest.fetch_max(sent, std::sync::atomic::Ordering::SeqCst) <= sent) {
                match str_field(input, "hook_event_name") {
                    Some("UserPromptSubmit") => crate::peers::set_status(Status::Working),
                    Some("Stop") => crate::peers::set_status(Status::Idle),
                    Some("Notification") => crate::peers::set_status(Status::Waiting),
                    _ => crate::peers::set_status(Status::Working),
                }
            }
            if str_field(input, "hook_event_name") != Some("PreToolUse") {
                return json!({});
            }
            let mode = str_field(input, "permission_mode");
            // Decided on the remote machine (a Bash call): only recorded here.
            if let Some(decision) = input.get(crate::helper::holder::DECIDED) {
                let tool = str_field(input, "tool_name").and_then(|n| n.strip_prefix(PREFIX)).unwrap_or("");
                trace("hook", &json!({ "tool": tool, "mode": mode, "decision": decision }).to_string());
                return json!({});
            }
            let name = str_field(input, "tool_name").and_then(|n| n.strip_prefix(PREFIX)).unwrap_or("");
            if !is_local_tool(name) {
                return json!({});
            }
            let empty = Object::new();
            let tool_input = input.get("tool_input").and_then(Value::as_object).unwrap_or(&empty);
            let decision = policy.decide(&hook_ws, name, tool_input, mode);
            trace("hook", &json!({ "tool": name, "mode": mode, "decision": decision.as_ref().map(Decision::to_json) }).to_string());
            hook_output(decision.as_ref())
        }))?;
        let hook_socket = forwards.add(&hook);
        let rules = forwards.file("bash-rules.json", json!({ "allow": p.bash_allow, "deny": p.bash_deny }).to_string().into_bytes());
        let pretool = format!("exec \"$CLAUDETTE_HELPER\" __helper hook {} {}", shell_quote(&rules), shell_quote(&hook_socket));
        let notify = format!("exec \"$CLAUDETTE_HELPER\" __helper notify {}", shell_quote(&hook_socket));
        let hooked = with_hook(delivered.settings, &pretool, &notify, &p.ask_tools);
        let settings = crate::settings::session_settings(Some(hooked));
        let settings_file = settings.as_ref().map(|s| forwards.file("session-settings.json", Value::Object(s.clone()).to_string().into_bytes()));

        let mut append = p.append_system_prompt.clone();
        append.push(env_prompt.await.unwrap_or_default());
        let append_system_prompt_file = Some(forwards.file("append-prompt.md", append.join("\n\n").into_bytes()));
        let mcp_config = Some(forwards.file("mcp.json", json!({ "mcpServers": servers }).to_string().into_bytes()));
        let plugin_dir = delivered.plugins.is_some().then(|| workdir::plugins_dir(&forwards.run));
        let args = remote_args(&p, &RemoteExtras { mirror: false, mcp_config, settings_file, append_system_prompt_file, plugin_dir });
        // The local CLAUDE.md files arrive in the environment prompt; the remote machine's stay out.
        let mut env = forwarded_env(std::env::vars(), &config.forward_env, config.forward_claude_env);
        env.insert("CLAUDE_CODE_DISABLE_CLAUDE_MDS".into(), "1".into());
        env.insert(crate::settings::AGENT_TEAMS.into(), "0".into());
        env.insert(crate::settings::AGENT_VIEW.into(), "1".into());
        let script = workdir::mount(cwd, &forwards, &delivered.sftp, delivered.user.as_deref(), delivered.plugins.as_deref(), &delivered.sessions, &delivered.config);
        let tty = is_tty();
        // claude's environment travels with the run, never on the remote command line.
        let mut run_env: serde_json::Map<String, Value> = env.into_iter().map(|(k, v)| (k, json!(v))).collect();
        // A terminal's own settings go with it, as ssh -t would send them.
        if tty {
            for key in ["TERM", "COLORTERM", "LANG", "LC_ALL", "LC_CTYPE"] {
                if let Ok(value) = std::env::var(key) {
                    run_env.insert(key.into(), json!(value));
                }
            }
        }
        let spec = crate::tunnel::Spec {
            run: forwards.run.clone(),
            command: claude_command(config, &script, &args),
            sockets: crate::tunnel::socket_pairs(&forwards),
            files: std::mem::take(&mut forwards.files),
            pty: if tty { Some(crate::tunnel::terminal_size().unwrap_or((24, 80))) } else { None },
            env: run_env,
        };
        // The frame log (this machine's, opt-in) records what the command line no longer carries.
        trace("spawn", &json!({ "command": spec.command, "sockets": spec.sockets.len(), "env": spec.env, "settings": settings }).to_string());
        let (tunnel, output) = crate::tunnel::Tunnel::start(config, spec, inbox).await?;
        // A host stopping us (T3 Code timing out text generation) stops the remote claude too.
        let code = tunnel.attach_stdio(output, tty).await;
        tunnel.finish().await;
        // A save the session made just before it ended is still on its way here.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), delivered.config.saves.settled()).await;
        code
    }
    .await;

    for bridge in bridges {
        bridge.close().await;
    }
    agent.end().await;
    hub.close();
    kill_all_commands();
    result
}
