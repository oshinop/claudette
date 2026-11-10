//! The helper: claudette's own binary, run on the remote machine to hold a session.
//!
//! claudette copies itself there (see [`crate::tunnel`]) and starts it per session; nothing is
//! installed and nothing keeps running once the session ends. For each run, a holder process owns
//! claude, the Unix sockets claude reaches this machine through, and claude's inbox. It is detached
//! from ssh, so when the connection drops (the local machine sleeps, the network changes) the session waits
//! instead of ending: claude's reads through the mounts and its tool calls simply wait, and once
//! claudette reconnects both sides send again what the other had not received.
//!
//! - `__helper connect <run> [--spawn]`: what ssh runs. Bridges ssh's stdio to the run's holder,
//!   starting it first with `--spawn`.
//! - `__helper hold <run>`: the holder.
//! - `__helper inbox <run>`: started by claude as a (tool-less) MCP server, so a process of claude's
//!   own: the holder hands it messages for claude's inbox, and claude takes what its own child posts
//!   as from itself, whatever its permission mode.
//! - `__helper mcp <socket> <manifest-file>`: the direct-mode local-tools MCP shim.
//! - `__helper notify <socket>`: forwards a hook payload without waiting for its response.
//! - `__helper hook <rules> <socket>`: claudette's PreToolUse hook; decides Bash calls itself.
//! - `__helper version`: prints the version, for `claudette +doctor`.
//! - `__helper config <state> <home> <laptop> <sink>`: the remote claude's Claude Code folder, for
//!   sshfs to mount (see [`config`]).
//! - `__helper sftp <socket> <warm file> [--writeback]`: the remote end of a mount of this machine,
//!   which answers what it knows without a round trip (see [`near`]).

pub mod config;
pub mod holder;
pub mod near;

use std::path::PathBuf;

use serde_json::{Map, Value, json};

/// The run's holder control socket on the remote machine.
pub fn control_path(run: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/claudette-{run}.holder"))
}

/// Where the run's inbox relay (`__helper inbox`) picks up messages from the holder.
pub fn relay_path(run: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/claudette-{run}.relay"))
}

/// The environment variable the holder gives the session: the helper binary, for claude to start
/// `__helper inbox` with, and for the sandbox to let it be read.
pub const HELPER_ENV: &str = "CLAUDETTE_HELPER";

/// The MCP server name of the inbox relay.
pub const INBOX_SERVER: &str = "claudette-inbox";

/// The inbox relay as an MCP server for the remote claude: the helper the holder gave the session.
pub fn inbox_server(run: &str) -> Map<String, Value> {
    let mut server = Map::new();
    server.insert("type".into(), json!("stdio"));
    server.insert("command".into(), json!("sh"));
    server.insert("args".into(), json!(["-c", format!("exec \"${HELPER_ENV}\" __helper inbox {run}")]));
    server
}

/// `claudette __helper …`: runs on the remote machine; returns the exit code.
pub fn main(args: &[String]) -> i32 {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
    match (args.first().map(String::as_str), args.get(1)) {
        (Some("version"), _) => {
            println!("{}", crate::VERSION);
            0
        }
        (Some("connect"), Some(run)) if valid_run(run) => runtime.block_on(holder::connect(run, args.iter().any(|a| a == "--spawn"))),
        (Some("hold"), Some(run)) if valid_run(run) => runtime.block_on(holder::hold(run)),
        (Some("inbox"), Some(run)) if valid_run(run) => runtime.block_on(holder::inbox(run)),
        (Some("mcp"), Some(socket)) if args.len() == 3 => runtime.block_on(holder::mcp(socket, &args[2])),
        (Some("notify"), Some(socket)) if args.len() == 2 => runtime.block_on(holder::notify(socket)),
        (Some("hook"), Some(rules)) if args.len() == 3 => runtime.block_on(holder::hook(rules, &args[2])),
        (Some("config"), _) => runtime.block_on(config::serve(&args[1..])),
        (Some("sftp"), _) => runtime.block_on(near::serve(&args[1..])),
        _ => {
            eprintln!("claudette: unknown helper command");
            2
        }
    }
}

/// Run ids are claudette's own 16 hex digits; nothing else may name a path.
fn valid_run(run: &str) -> bool {
    run.len() == 16 && run.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_are_checked() {
        assert!(valid_run("0123456789abcdef"));
        assert!(!valid_run("../../etc/passwd"));
    }
}
