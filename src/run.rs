//! What the paths that run claude remotely share: a child on our terminal, Claude Code's own
//! commands passed through as is, and what a session takes from this machine besides its tools.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};

use crate::args::ParsedArgs;
use crate::config::Config;
use crate::mount::workdir;
use crate::plugins;
use crate::settings;
use crate::sockets::{Forwards, SocketHub};
use crate::ssh::{SAVE_PRIVATE_FILE, claude_command, env_file, forwarded_env, shell_quote, ssh_argv};
use crate::tools::Workspace;
use crate::userconfig::{Delivery, UserConfig};

pub fn is_tty() -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 }
}

/// An exit status as a shell reports it: the code, or 128 plus the signal.
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Runs a command on our terminal; a signal that stops us stops it too.
pub async fn run_child(argv: &[String], stdin: Option<std::fs::File>) -> anyhow::Result<i32> {
    let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(stdin.map_or_else(Stdio::inherit, Stdio::from)).spawn()?;
    let pid = child.id();
    let (mut int, mut term, mut hup) = (signal(SignalKind::interrupt())?, signal(SignalKind::terminate())?, signal(SignalKind::hangup())?);
    let forward = |sig: nix::sys::signal::Signal| {
        if let Some(pid) = pid {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig);
        }
    };
    loop {
        tokio::select! {
            status = child.wait() => return Ok(exit_code(status?)),
            _ = int.recv() => forward(nix::sys::signal::Signal::SIGINT),
            _ = term.recv() => forward(nix::sys::signal::Signal::SIGTERM),
            _ = hup.recv() => forward(nix::sys::signal::Signal::SIGHUP),
        }
    }
}

/// Claude Code's own commands (`auth`, `doctor`, `--version`, …) run remotely as is. Their
/// environment goes ahead in a file only the remote user can read, which the command reads and
/// removes before claude starts, so it is never on the remote command line.
pub async fn passthrough(config: &Config, argv: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let env = forwarded_env(std::env::vars(), &config.forward_env, config.forward_claude_env);
    let mut script = workdir::enter(cwd);
    if !env.is_empty() {
        let file = shell_quote(&save_private_file(config, env_file(&env).into_bytes()).await?);
        script.setup = format!(". {file} && rm -f {file} || {{ rm -f {file}; exit 1; }}\n{}", script.setup);
    }
    let command = claude_command(config, &script, argv);
    run_child(&ssh_argv(config, &command, is_tty()), None).await
}

/// Saves `content` on the remote machine as a file only the remote user can read; returns its path.
async fn save_private_file(config: &Config, content: Vec<u8>) -> anyhow::Result<String> {
    use tokio::io::AsyncWriteExt;
    let argv = ssh_argv(config, SAVE_PRIVATE_FILE, false);
    let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(&content).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || !path.starts_with('/') {
        anyhow::bail!("could not pass the environment to {}: {}{}", config.host, String::from_utf8_lossy(&out.stderr).trim(), crate::ssh::failure_hint(config, out.status));
    }
    Ok(path)
}

/// The local folders the plugin store records as marketplaces (`"source": "directory"` or `"file"`),
/// which `claude plugin marketplace update` reads again.
fn recorded_marketplace_paths() -> Vec<PathBuf> {
    let user = settings::user_dir();
    let read = |p: PathBuf| std::fs::read(p).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    let known = read(user.join("plugins/known_marketplaces.json"));
    let declared = read(user.join("settings.json")).and_then(|s| s.get("extraKnownMarketplaces").cloned());
    let mut paths: Vec<PathBuf> = [known, declared]
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_object().cloned())
        .flat_map(|m| m.into_values())
        .filter_map(|m| m.get("source").and_then(|s| s.get("path")).and_then(|p| p.as_str()).map(PathBuf::from))
        .filter(|p| p.is_absolute() && p.exists() && !p.starts_with(&user))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// `claude plugin …`: Claude Code's own plugin commands, run by the remote machine's Claude Code on
/// this machine's plugin store and settings, mounted (see [`crate::mount::store`]). A local folder
/// the command names (`./my-marketplace`) is mounted too.
pub async fn plugin_command(config: &Config, argv: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let hub = SocketHub::new()?;
    let mut forwards = Forwards::new();
    let remote = workdir::store_dir(&forwards.run);
    let mut paths = recorded_marketplace_paths();
    let mut args = argv.to_vec();
    for arg in args.iter_mut().skip(1) {
        let looks_like_path = ["/", "./", "../", "~/"].iter().any(|p| arg.starts_with(p)) || arg == "." || arg == "..";
        if !looks_like_path {
            continue;
        }
        let expanded = match arg.strip_prefix("~/") {
            Some(rest) => crate::config::home().join(rest),
            None => cwd.join(&*arg),
        };
        let Ok(local) = expanded.canonicalize() else { continue };
        let n = paths.iter().position(|p| *p == local).unwrap_or_else(|| {
            paths.push(local);
            paths.len() - 1
        });
        *arg = format!("{remote}/paths/{n}");
    }
    let stage = Arc::new(crate::mount::store::Stage::new(cwd, &paths, &remote)?);
    let socket = hub.serve_store(&mut forwards, stage.clone())?;
    let script = workdir::store(&forwards, &socket, &config.remote_nc);
    let env = forwarded_env(std::env::vars(), &config.forward_env, config.forward_claude_env);
    let tty = is_tty();
    let spec = crate::tunnel::Spec {
        run: forwards.run.clone(),
        command: claude_command(config, &script, &args),
        sockets: crate::tunnel::socket_pairs(&forwards),
        files: std::mem::take(&mut forwards.files),
        pty: if tty { Some(crate::tunnel::terminal_size().unwrap_or((24, 80))) } else { None },
        // claude's environment travels with the run, never on the remote command line.
        env: env.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect(),
    };
    let (_deliveries, inbox) = tokio::sync::mpsc::unbounded_channel();
    let result = async {
        let (tunnel, output) = crate::tunnel::Tunnel::start(config, spec, inbox).await?;
        let code = tunnel.attach_stdio(output, tty).await;
        tunnel.finish().await;
        code
    }
    .await;
    hub.close();
    drop(stage);
    result
}

/// What a session takes from this machine besides its tools: the project, mounted; the sessions
/// folder, mounted as the remote claude's; for the remote claude's Claude Code folder, the user's
/// settings and keybindings, adapted, with the user's skills, agents, commands and output styles
/// mounted read-only; and the enabled plugins, mounted read-only (see [`crate::plugins`]).
pub struct Delivered {
    /// The workspace's SFTP socket on the remote machine.
    pub sftp: String,
    /// The user content's SFTP socket on the remote machine, if there is any to send.
    pub user: Option<String>,
    /// The sessions folder's SFTP socket on the remote machine.
    pub sessions: String,
    /// The plugins' SFTP socket on the remote machine, if the session has any.
    pub plugins: Option<String>,
    /// The user's configuration files, and where saves of them come back.
    pub config: Delivery,
    /// The remote claude's `--settings`: the caller's, when it gave any, without its plugin choices.
    pub settings: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Serves what the session takes from this machine, and adds the plugins' MCP servers to `args`.
pub fn deliver(hub: &SocketHub, forwards: &mut Forwards, args: &mut ParsedArgs, ws: &Arc<Workspace>, config: &Config) -> anyhow::Result<Delivered> {
    let sources = settings::Sources::of(args);
    let cwd = ws.root.as_path();
    let (sftp, callbacks) = hub.serve_workspace(forwards, ws, &config.remote_nc)?;
    let user = if sources.user { hub.serve_user_content(forwards)? } else { None };
    let sessions = hub.serve_sessions(forwards, cwd, &args.rest)?;
    let enabled = plugins::enabled(cwd, args);
    plugins::add_servers(args, &enabled, cwd);
    // The remote claude knows the plugins by the ids it gives `--plugin-dir` ones, and gets their
    // non-sensitive options under those; the caller's own plugin choices stay here.
    let options = plugins::remote_options(&enabled);
    let mut settings = args.settings.clone();
    if let Some(s) = settings.as_mut() {
        settings::PLUGIN_KEYS.iter().chain(&["pluginConfigs"]).for_each(|k| drop(s.remove(*k)));
    }
    if !options.is_empty() {
        settings.get_or_insert_default().insert("pluginConfigs".into(), serde_json::Value::Object(options));
    }
    let plugins = hub.serve_plugins(forwards, enabled, callbacks.clone())?;
    let config = hub.serve_user_config(forwards, Arc::new(UserConfig::new(callbacks)))?;
    Ok(Delivered { sftp, user, sessions, plugins, config, settings })
}
