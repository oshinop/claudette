//! The SDK path: an Agent SDK host on our stdio (`--input-format stream-json --output-format
//! stream-json`), its lines relayed to and from the remote claude by [`crate::relay`], which serves
//! the local tools over the SDK's control channel.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use futures_util::FutureExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::args::{RemoteExtras, parse_args, remote_args};
use crate::config::Config;
use crate::log::{trace, warn};
use crate::mount::workdir;
use crate::relay::{self, Relay};
use crate::run::deliver;
use crate::ssh::{claude_command, forwarded_env};
use crate::tools::{Workspace, kill_all_commands};
use crate::{helper, mcp, peers, prompt, settings, sockets, tunnel};

pub async fn session(config: &Config, argv: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let mut args = parse_args(argv, cwd)?;
    let path_rules = settings::apply_here(&mut args, cwd);
    mcp::config::add_servers(&mut args, cwd);
    let ws = Arc::new(Workspace::new(cwd.to_path_buf(), args.add_dirs.clone(), path_rules));
    let env_prompt = tokio::spawn(prompt::environment_prompt(cwd.to_path_buf(), args.add_dirs.clone())).map(Result::unwrap_or_default).boxed().shared();

    // The local CLAUDE.md files arrive in the environment prompt; the remote machine's stay out.
    let mut env = forwarded_env(std::env::vars(), &config.forward_env, config.forward_claude_env);
    env.insert("CLAUDE_CODE_DISABLE_CLAUDE_MDS".into(), "1".into());
    env.insert(settings::AGENT_TEAMS.into(), "0".into());
    env.insert(settings::AGENT_VIEW.into(), "1".into());
    let hub = sockets::SocketHub::new()?;
    let mut forwards = sockets::Forwards::new();
    let (to_inbox, inbox) = tokio::sync::mpsc::unbounded_channel();
    let agent = peers::Agent::new(&forwards.run, args.name.clone(), cwd, peers::queued(to_inbox));
    peers::register(agent.clone());
    hub.serve_agent(agent.clone())?;
    let delivered = deliver(&hub, &mut forwards, &mut args, &ws, config)?;
    // Servers that run here reach claude as SDK servers through the relay; remote ones it starts
    // itself, and so the inbox relay, which has to be a process of claude's own.
    let mut remote_servers = args.remote_servers.clone();
    remote_servers.insert(helper::INBOX_SERVER.into(), helper::inbox_server(&forwards.run));
    let mcp_config = Some(forwards.file("mcp.json", serde_json::json!({ "mcpServers": remote_servers }).to_string().into_bytes()));
    let session_settings = settings::session_settings(settings::with_asks(delivered.settings, &args.ask_tools));
    let settings_file = session_settings.as_ref().map(|s| forwards.file("session-settings.json", serde_json::Value::Object(s.clone()).to_string().into_bytes()));
    let append_system_prompt_file = (!args.append_system_prompt.is_empty())
        .then(|| forwards.file("append-prompt.md", args.append_system_prompt.join("\n\n").into_bytes()));
    let extras = RemoteExtras {
        // claude writes its transcripts into this machine's sessions folder through the mount; a host
        // with its own session store still gets the mirror frames it asked for.
        mirror: args.host_mirror,
        mcp_config,
        settings_file,
        append_system_prompt_file,
        plugin_dir: delivered.plugins.is_some().then(|| workdir::plugins_dir(&forwards.run)),
    };
    let script = workdir::mount(cwd, &forwards, &delivered.sftp, delivered.user.as_deref(), delivered.plugins.as_deref(), &delivered.sessions, &delivered.config);
    let spec = tunnel::Spec {
        run: forwards.run.clone(),
        command: claude_command(config, &script, &remote_args(&args, &extras)),
        sockets: tunnel::socket_pairs(&forwards),
        files: std::mem::take(&mut forwards.files),
        pty: None,
        // claude's environment travels with the run, never on the remote command line.
        env: env.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect(),
    };
    // The frame log (this machine's, opt-in) records what the command line no longer carries.
    trace("spawn", &serde_json::json!({ "command": spec.command, "sockets": spec.sockets.len(), "env": spec.env, "settings": session_settings }).to_string());
    let (tunnel, mut output) = tunnel::Tunnel::start(config, spec, inbox).await?;
    let tunnel = Arc::new(tunnel);

    let (to_claude, mut claude_lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    let to_host: relay::HostOut = Arc::new(|line: String| {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(line.as_bytes()).and_then(|()| out.write_all(b"\n")).and_then(|()| out.flush());
    });
    let relay = Relay::new(ws, &args, env_prompt, to_claude, to_host);

    // Everything queued before the host's input ended goes to claude first; then its stdin closes.
    let host_done = CancellationToken::new();
    let writer = tokio::spawn({
        let (host_done, tunnel) = (host_done.clone(), tunnel.clone());
        async move {
            loop {
                let line = tokio::select! {
                    biased;
                    line = claude_lines.recv() => line,
                    _ = host_done.cancelled() => None,
                };
                let Some(line) = line else { break };
                tunnel.write(format!("{line}\n").into_bytes()).await;
            }
            tunnel.close_stdin().await;
        }
    });

    // Host lines go in order: initialize waits for the environment prompt, and nothing may pass it.
    let reader = tokio::spawn({
        let (relay, host_done) = (relay.clone(), host_done.clone());
        async move {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.is_empty() => {}
                    Ok(Some(line)) => relay.from_host(line).await,
                    Ok(None) => break,
                    Err(e) => {
                        warn(&format!("reading from the host failed: {e}"));
                        break;
                    }
                }
            }
            host_done.cancel();
        }
    });

    // claude's output, split into lines for the relay; its stderr goes to ours.
    let mut pending: Vec<u8> = Vec::new();
    let take = |chan: u32, bytes: Vec<u8>, pending: &mut Vec<u8>| {
        if chan != tunnel::STDIO_CHANNEL {
            let _ = std::io::stderr().write_all(&bytes);
            return;
        }
        pending.extend_from_slice(&bytes);
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            if !line.is_empty() {
                relay.from_claude(&line);
            }
        }
    };
    let (mut int, mut term, mut hup) = (signal(SignalKind::interrupt())?, signal(SignalKind::terminate())?, signal(SignalKind::hangup())?);
    let code = loop {
        tokio::select! {
            item = output.recv() => match item {
                Some((chan, Some(bytes))) => take(chan, bytes, &mut pending),
                Some((_, None)) => {}
                None => break tunnel.wait().await,
            },
            code = tunnel.wait() => break code,
            _ = int.recv() => tunnel.signal(libc::SIGTERM).await,
            _ = term.recv() => tunnel.signal(libc::SIGTERM).await,
            _ = hup.recv() => tunnel.signal(libc::SIGTERM).await,
        }
    };
    while let Ok((chan, Some(bytes))) = output.try_recv() {
        take(chan, bytes, &mut pending);
    }
    reader.abort();
    writer.abort();
    relay.close().await;
    agent.end().await;
    tunnel.finish().await;
    // A save the session made just before it ended is still on its way here.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), delivered.config.saves.settled()).await;
    hub.close();
    kill_all_commands();
    code
}
