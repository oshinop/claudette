use std::path::Path;
use std::process::Stdio;

use anyhow::bail;
use tokio::process::Command;

use crate::config::Config;
use crate::ssh::{claude_command, shell_quote};
use crate::VERSION;
use crate::mount::workdir;
use crate::run::exit_code;
use crate::update::version_tuple;

const TESTED_CLAUDE: &str = "2.1.319";

const USAGE: &str = "claude, running on another machine, working on this one.

Usage:
  claudette [claude arguments]    claude itself; its tools run here, in this folder
  claudette mcp ...               claude's MCP commands, on this machine's configuration;
                                  --remote runs a server on the remote machine
  claudette plugin ...            claude's plugin commands, on this machine's plugins
{own}

Settings, in ~/.config/claudette/config.toml or as CLAUDETTE_<KEY> variables, which win:
  host                            ssh destination
  ssh                             ssh command and options (default: ssh, one shared connection)
  remote_claude                   claude command on the remote machine (default: claude)
  remote_nc                       nc command on the remote machine (default: nc)
  forward_claude_env              pass Claude Code's variables to the remote claude (default: true)
  forward_env                     more variables to pass
  detach_timeout                  how long a session waits for this machine (default: 12h)
  log                             file to log every relayed message to (holds prompts and files)
  prefix                          prefix of claudette's own commands (default: +)

More: https://github.com/oshinop/claudette/blob/main/docs/usage.md";

pub fn see_help(prefix: &str) -> String {
    format!("see `claudette {prefix}help`")
}

fn usage(prefix: &str) -> String {
    let own = [
        ("doctor", "check ssh and the remote machine"),
        ("update [version]", "install the latest release, or the one named"),
        ("version", "print claudette's version"),
        ("help", "show this help"),
    ]
        .map(|(name, what)| format!("  {:<32}{what}", format!("claudette {prefix}{name}")))
        .join("\n");
    USAGE.replace("{own}", &own)
}

pub async fn run(command: &str, args: &[String], prefix: &str, cwd: &Path) -> anyhow::Result<i32> {
    if command == "update" {
        anyhow::ensure!(args.len() <= 1, "`{prefix}update` takes at most a version; {}", see_help(prefix));
        return crate::update::update(args.first().map(String::as_str)).await;
    }
    let notice = std::io::IsTerminal::is_terminal(&std::io::stderr()).then(|| tokio::spawn(crate::update::notice(prefix.to_owned())));
    let code = match command {
        "" | "help" => {
            println!("claudette {VERSION}: {}", usage(prefix));
            0
        }
        "version" => {
            println!("{VERSION}");
            0
        }
        "doctor" => {
            let config = Config::load()?;
            crate::log::open(config.log_file.clone());
            doctor(&config, cwd).await?
        }
        other => bail!("unknown command `{prefix}{other}`; {}", see_help(prefix)),
    };
    if let Some(notice) = notice
        && let Ok(Ok(Some(line))) = tokio::time::timeout(crate::update::CHECK_TIMEOUT, notice).await
    {
        eprintln!("\n{line}");
    }
    Ok(code)
}

async fn doctor(config: &Config, cwd: &Path) -> anyhow::Result<i32> {
    println!("claudette {VERSION}");
    println!("host: {}", config.host);
    println!("ssh:  {}", config.ssh.join(" "));
    let command = claude_command(config, &workdir::enter(cwd), &["--version".to_owned()]);
    let out = Command::new(&config.ssh[0]).args(&config.ssh[1..]).args(["-o", "BatchMode=yes", "-T", &config.host, &command]).stdin(Stdio::null()).output().await?;
    if !out.status.success() {
        println!("remote claude: FAILED (exit {})\n{}", exit_code(out.status), String::from_utf8_lossy(&out.stderr).trim());
        println!("Check that `ssh -o BatchMode=yes <host> claude --version` works without a prompt.");
        return Ok(1);
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    println!("remote claude: {text}");
    let version = regex::Regex::new(r"\d+\.\d+\.\d+").expect("valid pattern").find(&text).map(|m| m.as_str().to_owned());
    if let Some(version) = version
        && version_tuple(&version) < version_tuple(TESTED_CLAUDE)
    {
        println!("warning: claudette is tested with Claude Code {TESTED_CLAUDE}; the remote has {version}");
    }
    let probe = format!(
        "for c in sshfs {} bwrap sandbox-exec; do if command -v \"$c\" >/dev/null 2>&1; then echo \"$c yes\"; else echo \"$c no\"; fi; done",
        shell_quote(&config.remote_nc)
    );
    let out = Command::new(&config.ssh[0]).args(&config.ssh[1..]).args(["-o", "BatchMode=yes", "-T", &config.host, &probe]).stdin(Stdio::null()).output().await?;
    let found: std::collections::HashMap<String, bool> =
        String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.rsplit_once(' ')).map(|(c, v)| (c.to_owned(), v == "yes")).collect();
    let has = |c: &str| found.get(c).copied().unwrap_or(false);
    let mut code = 0;
    for tool in ["sshfs", config.remote_nc.as_str()] {
        if has(tool) {
            println!("remote {tool}: found");
        } else {
            println!("remote {tool}: MISSING (the project is mounted with sshfs over `{} -U`)", config.remote_nc);
            code = 1;
        }
    }
    if has("bwrap") {
        println!("remote sandbox: bubblewrap");
    } else if has("sandbox-exec") {
        println!("remote sandbox: sandbox-exec");
    } else {
        println!("remote sandbox: MISSING (install bubblewrap; claude only runs confined to the workspace)");
        code = 1;
    }
    match crate::tunnel::check_helper(config).await {
        Ok(version) => println!("remote helper: {version}"),
        Err(e) => {
            println!("remote helper: FAILED ({e:#})");
            code = 1;
        }
    }
    Ok(code)
}
