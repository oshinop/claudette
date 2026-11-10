//! claudette's own commands, beside claude's: `claudette +doctor`, `+update`, `+version`, `+help`. The
//! prefix (`+` unless the `prefix` setting says otherwise) keeps them apart from claude's commands and
//! prompts. On a terminal they end with a notice when a newer release is out; nothing else shows it.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::config::Config;
use crate::json::Object;
use crate::mcp::config::{self as mcp, Place, Scope};
use crate::ssh::{claude_command, shell_quote};
use crate::VERSION;
use crate::mount::workdir;
use crate::run::exit_code;
use crate::update::version_tuple;

/// The Claude Code version the test suite runs against.
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

/// Where to look for usage, in errors.
pub fn see_help(prefix: &str) -> String {
    format!("see `claudette {prefix}help`")
}

/// The help, with claudette's own commands under the prefix in use, aligned with the rest.
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

/// `command`: what follows the prefix (`doctor` in `+doctor`); `args`, the arguments after it.
pub async fn run(command: &str, args: &[String], prefix: &str, cwd: &Path) -> anyhow::Result<i32> {
    if command == "update" {
        anyhow::ensure!(args.len() <= 1, "`{prefix}update` takes at most a version; {}", see_help(prefix));
        return crate::update::update(args.first().map(String::as_str)).await;
    }
    // Looked for while the command runs; only on a terminal, so scripts never wait on it.
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
    // The workspace mount needs sshfs and the relay needs nc; the sandbox uses bwrap or sandbox-exec.
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
    // Sessions run under the helper; it is copied on first use, as here.
    match crate::tunnel::check_helper(config).await {
        Ok(version) => println!("remote helper: {version}"),
        Err(e) => {
            println!("remote helper: FAILED ({e:#})");
            code = 1;
        }
    }
    Ok(code)
}

/// `claude mcp …`, as Claude Code's, on this machine's configuration.
pub fn mcp_command(args: &[String], cwd: &Path) -> anyhow::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") | None => list(cwd),
        Some("get") => get(args.get(1).context("usage: claude mcp get <name>")?, cwd),
        Some("add") => add(&args[1..], cwd, false),
        Some("add-json") => add(&args[1..], cwd, true),
        Some("remove") => remove(&args[1..], cwd),
        Some(other) => bail!("`claude mcp {other}` is not available with claudette: use list, get, add, add-json or remove"),
    }
}

fn place_label(place: Place) -> String {
    match place {
        Place::Here => "this machine".into(),
        Place::Remote => Config::load().map(|c| c.host).unwrap_or_else(|_| "the remote machine".into()),
    }
}

fn summary(config: &Object) -> String {
    match config.get("url").and_then(Value::as_str) {
        Some(url) => format!("{} {url}", config.get("type").and_then(Value::as_str).unwrap_or("http")),
        None => {
            let command = config.get("command").and_then(Value::as_str).unwrap_or("");
            let args: Vec<&str> = config.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            std::iter::once(command).chain(args).collect::<Vec<_>>().join(" ")
        }
    }
}

fn list(cwd: &Path) -> anyhow::Result<i32> {
    let servers = mcp::servers(cwd, true);
    if servers.is_empty() {
        println!("No MCP servers. Add one with `claude mcp add`.");
    }
    for server in servers {
        let approval = if server.approved { "" } else { "  NOT APPROVED: add it to \"enabledMcpjsonServers\" in .claude/settings.local.json" };
        println!("{}: {}  (runs on {}; {}){approval}", server.name, summary(&server.config), place_label(server.place), server.origin.label());
    }
    Ok(0)
}

fn get(name: &str, cwd: &Path) -> anyhow::Result<i32> {
    let server = mcp::servers(cwd, true).into_iter().find(|s| s.name == name).with_context(|| format!("no MCP server named {name}"))?;
    println!("{name}:\n  defined in: {}\n  runs on: {}", server.origin.label(), place_label(server.place));
    if !server.approved {
        println!("  NOT APPROVED: add it to \"enabledMcpjsonServers\" in .claude/settings.local.json");
    }
    println!("{}", serde_json::to_string_pretty(&server.config)?);
    Ok(0)
}

/// `add` and `add-json`, with the options `claude mcp add` takes, plus `--remote`.
fn add(args: &[String], cwd: &Path, as_json: bool) -> anyhow::Result<i32> {
    let mut scope = Scope::Local;
    let mut transport = "stdio".to_owned();
    let mut place = Place::Here;
    let mut env = Object::new();
    let mut headers = Object::new();
    let mut positional: Vec<String> = vec![];
    let mut i = 0;
    let value = |i: usize, flag: &str| args.get(i + 1).cloned().with_context(|| format!("{flag} needs a value"));
    while i < args.len() {
        let arg = &args[i];
        // Once the name and command are known, everything after belongs to the server's command.
        if positional.len() >= 2 || arg == "--" {
            positional.extend(args[i + usize::from(arg == "--")..].iter().cloned());
            break;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if arg.starts_with("--") => (f, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut take = || -> anyhow::Result<String> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => {
                    i += 1;
                    value(i - 1, flag)
                }
            }
        };
        match flag {
            "-s" | "--scope" => scope = Scope::parse(&take()?)?,
            "-t" | "--transport" => transport = take()?,
            "-e" | "--env" => {
                let pair = take()?;
                let (k, v) = pair.split_once('=').with_context(|| format!("--env {pair}: use KEY=VALUE"))?;
                env.insert(k.to_owned(), json!(v));
            }
            "-H" | "--header" => {
                let header = take()?;
                let (k, v) = header.split_once(':').with_context(|| format!("--header {header}: use 'Name: value'"))?;
                headers.insert(k.trim().to_owned(), json!(v.trim()));
            }
            "--remote" => place = Place::Remote,
            _ if arg.starts_with('-') && positional.is_empty() => bail!("unknown option {arg}: `mcp add` takes claude's -s, -t, -e and -H, and --remote"),
            _ => positional.push(arg.clone()),
        }
        i += 1;
    }
    let (Some(name), Some(target)) = (positional.first().cloned(), positional.get(1).cloned()) else {
        bail!(if as_json { "usage: claude mcp add-json [-s scope] [--remote] <name> '<json>'" } else { "usage: claude mcp add [options] <name> <command-or-url> [args…]" });
    };
    let config: Object = if as_json {
        match serde_json::from_str::<Value>(&target).context("the server's JSON")? {
            Value::Object(o) => o,
            _ => bail!("the server's JSON must be an object"),
        }
    } else {
        match transport.as_str() {
            "stdio" => {
                let mut c = json!({ "command": target, "args": positional[2..] }).as_object().cloned().unwrap_or_default();
                if !env.is_empty() {
                    c.insert("env".into(), Value::Object(env));
                }
                c
            }
            "http" | "sse" => {
                let mut c = json!({ "type": transport, "url": target }).as_object().cloned().unwrap_or_default();
                if !headers.is_empty() {
                    c.insert("headers".into(), Value::Object(headers));
                }
                c
            }
            other => bail!("unknown transport {other}: use stdio, http or sse"),
        }
    };
    let file = mcp::add(cwd, scope, &name, config, place)?;
    println!("Added {name} ({} scope, runs on {}) to {}", scope.name(), place_label(place), file.display());
    if scope == Scope::Project {
        println!("Approved for this machine in .claude/settings.local.json");
    }
    Ok(0)
}

fn remove(args: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let mut scope = None;
    let mut name = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-s" | "--scope" => {
                i += 1;
                scope = Some(Scope::parse(args.get(i).context("--scope needs a value")?)?);
            }
            a if a.starts_with("--scope=") => scope = Some(Scope::parse(&a["--scope=".len()..])?),
            a => name = Some(a.to_owned()),
        }
        i += 1;
    }
    let name = name.context("usage: claude mcp remove [-s scope] <name>")?;
    let (scope, file) = mcp::remove(cwd, scope, &name)?;
    println!("Removed {name} ({} scope) from {}", scope.name(), file.display());
    Ok(0)
}
