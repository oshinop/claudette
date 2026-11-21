pub mod builds;
pub mod config;
pub mod holder;
pub mod near;

use std::path::PathBuf;

use serde_json::{Map, Value, json};

pub fn control_path(run: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/claudette-{run}.holder"))
}

pub fn relay_path(run: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/claudette-{run}.relay"))
}

pub const HELPER_ENV: &str = "CLAUDETTE_HELPER";

pub const INBOX_SERVER: &str = "claudette-inbox";

pub fn inbox_server(run: &str) -> Map<String, Value> {
    let mut server = Map::new();
    server.insert("type".into(), json!("stdio"));
    server.insert("command".into(), json!("sh"));
    server.insert("args".into(), json!(["-c", format!("exec \"${HELPER_ENV}\" __helper inbox {run}")]));
    server
}

pub fn main(args: &[String]) -> i32 {
    forget_ssh_session();
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
        (Some("probe"), None) => runtime.block_on(probe()),
        (Some("prune"), None) => builds::prune(),
        (Some("open"), Some(socket)) if args.len() == 3 && !args[2].contains('\n') => runtime.block_on(desktop(socket, &format!("open {}", args[2]), false)),
        (Some("copy"), Some(socket)) if args.len() == 2 => runtime.block_on(desktop(socket, "copy", true)),
        (Some("paste"), Some(socket)) if args.len() == 2 => runtime.block_on(desktop(socket, "paste", false)),
        (Some("sftp"), _) => runtime.block_on(near::serve(&args[1..])),
        _ => {
            eprintln!("claudette: unknown helper command");
            2
        }
    }
}

fn forget_ssh_session() {
    for (name, _) in std::env::vars_os().filter(|(name, _)| name.to_str().is_some_and(|n| n.starts_with("SSH_"))) {
        unsafe { std::env::remove_var(name) };
    }
}

async fn probe() -> i32 {
    use tokio::io::AsyncReadExt;
    let mut input = String::new();
    if tokio::io::stdin().read_to_string(&mut input).await.is_err() {
        return 1;
    }
    let Ok(Value::Object(servers)) = serde_json::from_str::<Value>(&input) else { return 1 };
    let launch = crate::mcp::bridge::Launch::new(&crate::config::home(), &[], crate::mcp::Source::User);
    let checks = servers.iter().filter_map(|(name, config)| Some((name, config.as_object()?))).map(|(name, config)| {
        let launch = &launch;
        async move { (name.clone(), crate::mcp::bridge::probe(name, config, launch).await.to_json()) }
    });
    let results: Map<String, Value> = futures_util::future::join_all(checks).await.into_iter().collect();
    println!("{}", Value::Object(results));
    0
}

async fn desktop(socket: &str, request: &str, send_stdin: bool) -> i32 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(mut conn) = tokio::net::UnixStream::connect(socket).await else {
        eprintln!("claudette: the session's link to the local machine is gone");
        return 1;
    };
    let mut reply = Vec::new();
    let sent = async {
        conn.write_all(format!("{request}\n").as_bytes()).await?;
        if send_stdin {
            tokio::io::copy(&mut tokio::io::stdin(), &mut conn).await?;
        }
        conn.shutdown().await?;
        conn.read_to_end(&mut reply).await
    };
    if sent.await.is_err() {
        eprintln!("claudette: the local machine did not answer");
        return 1;
    }
    let newline = reply.iter().position(|&b| b == b'\n').unwrap_or(reply.len());
    let (status, body) = (String::from_utf8_lossy(&reply[..newline]), reply.get(newline + 1..).unwrap_or_default());
    match status.strip_prefix("error: ") {
        None if status == "ok" => {
            use std::io::Write;
            let mut out = std::io::stdout();
            i32::from(out.write_all(body).and_then(|()| out.flush()).is_err())
        }
        Some(error) => {
            eprintln!("claudette: {error}");
            1
        }
        None => 1,
    }
}

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
