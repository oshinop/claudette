mod args;
mod commands;
mod config;
mod direct;
mod helper;
mod json;
mod link;
mod log;
mod mcp;
mod mount;
mod names;
mod peers;
mod permissions;
mod plugins;
mod prompt;
mod relay;
mod run;
mod sdk;
mod settings;
mod sockets;
mod ssh;
mod tools;
mod tunnel;
mod update;
mod userconfig;

use anyhow::bail;

use crate::args::{is_claude_command, is_stream_json_session};
use crate::config::Config;
use crate::log::warn;
use crate::tools::kill_all_commands;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Background sessions would run on the remote machine without this machine's tools or the sandbox.
const BACKGROUND: &str = "{what} manages background sessions, which would run on the remote machine without this machine's tools \
or the sandbox. To keep a session going without its terminal, run claudette inside tmux or screen here (a session waits \
while the local machine sleeps); hosts such as T3 Code and Paseo manage sessions of their own. Sessions here list and message each \
other with ListAgents and SendMessage.";

async fn run(argv: Vec<String>) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?;
    let prefix = config::prefix()?;
    match argv.first().map(String::as_str) {
        Some(first) if first.starts_with(prefix.as_str()) => return commands::run(&first[prefix.len()..], &argv[1..], &prefix, &cwd).await,
        Some("mcp") => return commands::mcp_command(&argv[1..], &cwd),
        Some("plugin" | "plugins") => return run::plugin_command(&Config::load()?, &argv, &cwd).await,
        Some(command @ ("agents" | "attach" | "respawn" | "stop" | "kill" | "rm" | "logs")) => bail!("{}", BACKGROUND.replace("{what}", &format!("`claude {command}`"))),
        _ => {}
    }
    if argv.iter().any(|a| a == "--bg" || a == "--background") {
        bail!("{}", BACKGROUND.replace("{what}", "`--bg`"));
    }
    let config = Config::load()?;
    log::open(config.log_file.clone());
    if is_stream_json_session(&argv) {
        return sdk::session(&config, &argv, &cwd).await;
    }
    if is_claude_command(&argv) {
        return run::passthrough(&config, &argv, &cwd).await;
    }
    direct::direct(&config, &argv, &cwd).await
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // On the remote machine: the session helper, which runs its own runtime.
    if args.first().map(String::as_str) == Some("__helper") {
        std::process::exit(helper::main(&args[1..]));
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
    let code = runtime.block_on(run(args)).unwrap_or_else(|e| {
        warn(&e.to_string());
        1
    });
    kill_all_commands();
    std::process::exit(code);
}
