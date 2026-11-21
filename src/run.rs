use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::args::ParsedArgs;
use crate::config::Config;
use crate::mount::workdir;
use crate::plugins;
use crate::settings;
use crate::sockets::{Forwards, SocketHub};
use crate::ssh::{claude_command, forwarded_env};
use crate::tools::Workspace;
use crate::userconfig::{Delivery, UserConfig};

pub fn is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 }
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

pub async fn passthrough(config: &Config, argv: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let hub = SocketHub::new()?;
    let mut forwards = Forwards::new();
    let desktop = hub.serve_desktop(&mut forwards, Arc::new(crate::browser::Browser::new(config.clone())))?;
    let script = workdir::enter_with_desktop(cwd, &forwards, &desktop);
    let result = run_remote(config, forwards, claude_command(config, &script, argv)).await;
    hub.close();
    result
}

async fn run_remote(config: &Config, mut forwards: Forwards, command: String) -> anyhow::Result<i32> {
    let env = forwarded_env(std::env::vars(), &config.forward_env, config.forward_claude_env);
    let tty = is_tty();
    let spec = crate::tunnel::Spec {
        run: forwards.run.clone(),
        command,
        sockets: crate::tunnel::socket_pairs(&forwards),
        files: std::mem::take(&mut forwards.files),
        pty: if tty { Some(crate::tunnel::terminal_size().unwrap_or((24, 80))) } else { None },
        env: env.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect(),
    };
    let (_deliveries, inbox) = tokio::sync::mpsc::unbounded_channel();
    let (tunnel, output) = crate::tunnel::Tunnel::start(config, spec, inbox).await?;
    let code = tunnel.attach_stdio(output, tty).await;
    tunnel.finish().await;
    code
}

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
    let result = run_remote(config, forwards, claude_command(config, &script, &args)).await;
    hub.close();
    drop(stage);
    result
}

pub struct Delivered {
    pub sftp: String,
    pub user: Option<String>,
    pub sessions: String,
    pub plugins: Option<String>,
    pub config: Delivery,
    pub desktop: String,
    pub settings: Option<serde_json::Map<String, serde_json::Value>>,
}

pub fn deliver(hub: &SocketHub, forwards: &mut Forwards, args: &mut ParsedArgs, ws: &Arc<Workspace>, config: &Config) -> anyhow::Result<Delivered> {
    let sources = settings::Sources::of(args);
    let cwd = ws.root.as_path();
    let (sftp, callbacks) = hub.serve_workspace(forwards, ws, &config.remote_nc)?;
    let user = if sources.user { hub.serve_user_content(forwards)? } else { None };
    let sessions = hub.serve_sessions(forwards, cwd, &args.rest)?;
    let enabled = plugins::enabled(cwd, args);
    plugins::add_servers(args, &enabled, cwd);
    let options = plugins::remote_options(&enabled);
    let mut settings = args.settings.clone();
    if let Some(s) = settings.as_mut() {
        settings::PLUGIN_KEYS.iter().chain(&["pluginConfigs"]).for_each(|k| drop(s.remove(*k)));
    }
    if !options.is_empty() {
        settings.get_or_insert_default().insert("pluginConfigs".into(), serde_json::Value::Object(options));
    }
    let plugins = hub.serve_plugins(forwards, enabled, callbacks.clone())?;
    let desktop = hub.serve_desktop(forwards, Arc::new(crate::browser::Browser::new(config.clone())))?;
    let config = hub.serve_user_config(forwards, Arc::new(UserConfig::new(callbacks)))?;
    Ok(Delivered { sftp, user, sessions, plugins, config, desktop, settings })
}
