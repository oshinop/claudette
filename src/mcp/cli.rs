use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde_json::{Value, json};

use super::bridge::{Health, Launch, configured_headers, probe};
use super::config::{self as mcp, Approval, Place, Removal, Scope, Server};
use super::oauth;
use crate::config::Config;
use crate::json::Object;

const PENDING: &str = "⏸ Pending approval (add it to \"enabledMcpjsonServers\" in .claude/settings.local.json)";
const REJECTED: &str = "✘ Rejected (see disabledMcpjsonServers in settings)";
const SHOWN_NAMES: usize = 8;

pub async fn run(argv: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let args = &argv[1..];
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") || args[0] == "help" {
        return crate::run::passthrough(&Config::load()?, argv, cwd).await;
    }
    let rest = &args[1..];
    match args[0].as_str() {
        "list" => list(cwd).await,
        "get" => get(rest, cwd).await,
        "add" => add(rest, cwd, false),
        "add-json" => add(rest, cwd, true),
        "remove" => remove(rest, cwd),
        "login" => login(rest, cwd).await,
        "logout" => logout(rest, cwd),
        "reset-project-choices" => {
            mcp::reset_choices(cwd)?;
            println!("Project-scoped (.mcp.json) server approvals and rejections stored for this project have been reset.");
            println!("Servers in .mcp.json start again once you approve them in .claude/settings.local.json.");
            Ok(0)
        }
        "serve" | "add-from-claude-desktop" => fail(format!("`claude mcp {}` is not available with claudette", args[0])),
        other => fail(format!("error: unknown command '{other}'")),
    }
}

fn fail(message: impl std::fmt::Display) -> anyhow::Result<i32> {
    eprintln!("{message}");
    Ok(1)
}

async fn login(args: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let (mut name, mut no_browser) = (None, false);
    for arg in args {
        match arg.as_str() {
            "--no-browser" => no_browser = true,
            flag if flag.starts_with('-') => return fail(format!("error: unknown option '{flag}'")),
            value if name.is_none() => name = Some(value),
            _ => return fail("error: too many arguments for 'login'"),
        }
    }
    let Some(name) = name else { return fail("error: missing required argument 'name'") };
    println!("Starting authentication for \"{name}\"…");
    let result = async {
        let servers = mcp::servers(cwd, true);
        let server = servers.iter().find(|server| server.name == name).context("no MCP server with that name")?;
        ensure!(server.place == Place::Here, "this server runs remotely; login is only available for servers running on this machine");
        ensure!(server.approval == Approval::Approved, "approve this MCP server before logging in");
        let store = oauth::Store::new(name, &server.config, cwd, &server.source).context("OAuth login requires an HTTP or SSE MCP server with a URL")?;
        let url = server.config.get("url").and_then(Value::as_str).context("server has no URL")?;
        let launch = Launch::new(cwd, &[], server.source.clone());
        let headers = configured_headers(name, &server.config, url, &launch).await.map_err(anyhow::Error::msg)?;
        store.login(&server.config, &headers, no_browser).await
    }.await;
    if let Err(error) = result {
        return fail(format!("Couldn't complete authentication for \"{name}\": {error:#}"));
    }
    println!("Authenticated with \"{name}\". Its tools are now available in Claude Code.");
    Ok(0)
}

fn logout(args: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let [name] = args else { return fail("error: expected one argument 'name'") };
    let servers = mcp::servers(cwd, true);
    let Some(server) = servers.iter().find(|server| &server.name == name) else { return fail(format!("No MCP server named \"{name}\".")) };
    if server.place == Place::Here
        && let Some(store) = oauth::Store::new(name, &server.config, cwd, &server.source)
    {
        store.clear()?;
    }
    println!("Signed out of \"{name}\". Run `claude mcp login {name}` to authenticate again.");
    Ok(0)
}

fn read_client_secret() -> anyhow::Result<String> {
    let secret = match std::env::var("MCP_CLIENT_SECRET") {
        Ok(secret) => secret,
        Err(std::env::VarError::NotPresent) if std::io::stdin().is_terminal() => rpassword::prompt_password("OAuth client secret: ")?,
        Err(std::env::VarError::NotPresent) => anyhow::bail!("set MCP_CLIENT_SECRET when --client-secret is used without a terminal"),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("MCP_CLIENT_SECRET must contain valid UTF-8"),
    };
    ensure!(!secret.is_empty(), "OAuth client secret cannot be empty");
    Ok(secret)
}

fn kind(config: &Object) -> &str {
    match config.get("type").and_then(Value::as_str) {
        None => "stdio",
        Some("streamable-http") => "http",
        Some(kind) => kind,
    }
}

fn strings(value: Option<&Value>) -> Vec<&str> {
    value.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

fn summary(config: &Object) -> String {
    let url = config.get("url").and_then(Value::as_str).unwrap_or("");
    match kind(config) {
        "http" => format!("{url} (HTTP)"),
        "sse" => format!("{url} (SSE)"),
        "ws" => format!("{url} (WS)"),
        _ => format!("{} {}", config.get("command").and_then(Value::as_str).unwrap_or(""), strings(config.get("args")).join(" ")),
    }
}

enum Status {
    Pending,
    Rejected,
    Checked(Health),
}

impl Status {
    fn text(&self) -> (&str, Option<&str>) {
        match self {
            Status::Pending => (PENDING, None),
            Status::Rejected => (REJECTED, None),
            Status::Checked(Health::Connected) => ("✔ Connected", None),
            Status::Checked(Health::NeedsAuth) => ("! Needs authentication", None),
            Status::Checked(Health::Failed(issue)) => ("✘ Failed to connect", Some(issue)),
        }
    }
}

async fn statuses(servers: &[&Server], cwd: &Path) -> Vec<Status> {
    let remote: Object = servers.iter().filter(|s| s.place == Place::Remote && s.approval == Approval::Approved).map(|s| (s.name.clone(), Value::Object(s.config.clone()))).collect();
    let remote_health = async {
        if remote.is_empty() {
            return Ok(Object::new());
        }
        let config = Config::load()?;
        let out = crate::tunnel::helper_output(&config, "probe", Value::Object(remote.clone()).to_string().as_bytes()).await?;
        match serde_json::from_str::<Value>(&out) {
            Ok(Value::Object(results)) => Ok(results),
            _ => anyhow::bail!("the remote machine's helper gave no health report"),
        }
    };
    let local = futures_util::future::join_all(servers.iter().map(|s| async move {
        match s.approval {
            Approval::Pending => Some(Status::Pending),
            Approval::Rejected => Some(Status::Rejected),
            Approval::Approved if s.place == Place::Here => Some(Status::Checked(probe(&s.name, &s.config, &Launch::new(cwd, &[], s.source.clone())).await)),
            Approval::Approved => None,
        }
    }));
    let (local, remote_health): (Vec<Option<Status>>, anyhow::Result<Object>) = futures_util::future::join(local, remote_health).await;
    servers
        .iter()
        .zip(local)
        .map(|(server, status)| {
            status.unwrap_or_else(|| match &remote_health {
                Ok(results) => Status::Checked(results.get(&server.name).map_or_else(|| Health::Failed("no answer from the remote machine".into()), Health::from_json)),
                Err(e) => Status::Checked(Health::Failed(format!("{e:#}"))),
            })
        })
        .collect()
}

async fn list(cwd: &Path) -> anyhow::Result<i32> {
    let servers = mcp::servers(cwd, true);
    let shown: Vec<&Server> = servers.iter().filter(|s| s.approval != Approval::Rejected).collect();
    if shown.is_empty() {
        println!("No MCP servers configured. Use `claude mcp add` to add a server.");
        return Ok(0);
    }
    println!("Checking MCP server health…\n");
    for (server, status) in shown.iter().zip(statuses(&shown, cwd).await) {
        let (text, issue) = status.text();
        let issue = issue.map(|i| format!(" — {i}")).unwrap_or_default();
        println!("{}: {} - {text}{issue}", server.name, summary(&server.config));
    }
    Ok(0)
}

fn names(servers: &[Server], approved_only: bool) -> String {
    let mut names: Vec<&str> = servers.iter().filter(|s| !approved_only || s.approval == Approval::Approved).map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    if names.len() <= SHOWN_NAMES {
        return names.join(", ");
    }
    format!("{} (and {} more — run `claude mcp list` to see all)", names[..SHOWN_NAMES].join(", "), names.len() - SHOWN_NAMES)
}

async fn get(args: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let [name] = args else { return fail("error: missing required argument 'name'") };
    let servers = mcp::servers(cwd, true);
    let Some(server) = servers.iter().find(|s| &s.name == name) else {
        let waiting = if servers.iter().any(|s| s.approval == Approval::Pending) {
            " (.mcp.json servers are awaiting approval: add them to \"enabledMcpjsonServers\" in .claude/settings.local.json.)"
        } else {
            ""
        };
        return fail(format!("No MCP server named \"{name}\". Configured servers: {}{waiting}", names(&servers, true)));
    };
    let status = statuses(&[server], cwd).await.remove(0);
    let (text, issue) = status.text();
    let scope = server.origin.scope();
    let config = &server.config;
    println!("{name}:");
    println!("  Scope: {}", scope.label());
    if server.place == Place::Remote {
        println!("  Runs on: {}", Config::load()?.host);
    }
    println!("  Status: {text}");
    if let Some(issue) = issue {
        println!("  Issue: {issue}");
    }
    println!("  Type: {}", kind(config));
    match kind(config) {
        "stdio" => {
            println!("  Command: {}", config.get("command").and_then(Value::as_str).unwrap_or(""));
            println!("{}", format!("  Args: {}", strings(config.get("args")).join(" ")).trim_end());
            println!("  Environment:");
            for (key, value) in config.get("env").and_then(Value::as_object).into_iter().flatten() {
                println!("    {key}={}", value.as_str().unwrap_or_default());
            }
        }
        _ => {
            println!("  URL: {}", config.get("url").and_then(Value::as_str).unwrap_or(""));
            if let Some(headers) = config.get("headers").and_then(Value::as_object).filter(|h| !h.is_empty()) {
                println!("  Headers:");
                for (key, value) in headers {
                    println!("    {key}: {}", value.as_str().unwrap_or_default());
                }
            }
            if let Some(oauth) = config.get("oauth").and_then(Value::as_object) {
                let mut parts = vec![];
                if oauth.contains_key("clientId") {
                    parts.push("client_id configured".to_owned());
                }
                if let Some(port) = oauth.get("callbackPort") {
                    parts.push(format!("callback_port {port}"));
                }
                println!("  OAuth: {}", parts.join(", "));
            }
        }
    }
    println!();
    println!("To remove this server, run: claude mcp remove {name} -s {}", scope.name());
    Ok(0)
}

struct AddArgs {
    scope: Scope,
    transport: String,
    env: Vec<String>,
    headers: Vec<String>,
    client_id: Option<String>,
    client_secret: bool,
    callback_port: Option<String>,
    remote: bool,
    positional: Vec<String>,
}

fn parse_add(args: &[String], as_json: bool) -> Result<AddArgs, String> {
    let mut parsed = AddArgs { scope: Scope::Local, transport: "stdio".into(), env: vec![], headers: vec![], client_id: None, client_secret: false, callback_port: None, remote: false, positional: vec![] };
    let mut i = 0;
    let mut operands_only = false;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if operands_only || !arg.starts_with('-') || arg == "-" {
            parsed.positional.push(arg.clone());
            continue;
        }
        if arg == "--" {
            operands_only = true;
            continue;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if arg.starts_with("--") => (f, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = |spec: &str| -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            match args.get(i) {
                Some(v) if !v.starts_with('-') => {
                    i += 1;
                    Ok(v.clone())
                }
                _ => Err(format!("error: option '{spec}' argument missing")),
            }
        };
        match flag {
            "-s" | "--scope" => parsed.scope = Scope::parse(&value("-s, --scope <scope>")?).map_err(|e| e.to_string())?,
            "-t" | "--transport" if !as_json => parsed.transport = value("-t, --transport <transport>")?,
            "--client-id" if !as_json => parsed.client_id = Some(value("--client-id <clientId>")?),
            "--callback-port" if !as_json => parsed.callback_port = Some(value("--callback-port <port>")?),
            "--client-secret" if inline.is_none() => parsed.client_secret = true,
            "--client-secret" => return Err("--client-secret takes no value; use MCP_CLIENT_SECRET or the terminal prompt".into()),
            "--remote" => parsed.remote = true,
            "-e" | "--env" | "-H" | "--header" if !as_json => {
                let target = if flag.starts_with("-e") || flag == "--env" { &mut parsed.env } else { &mut parsed.headers };
                target.extend(inline.clone());
                while let Some(next) = args.get(i).filter(|a| !a.starts_with('-')) {
                    target.push(next.clone());
                    i += 1;
                }
                if inline.is_none() && target.is_empty() {
                    return Err(format!("error: option '{}' argument missing", if flag.starts_with("-e") || flag == "--env" { "-e, --env <env...>" } else { "-H, --header <header...>" }));
                }
            }
            _ => return Err(format!("error: unknown option '{arg}'")),
        }
    }
    Ok(parsed)
}

fn file_modified(file: &Path, scope: Scope, cwd: &Path) -> String {
    match scope {
        Scope::Local => format!("File modified: {} [project: {}]", file.display(), mcp::project_key(cwd)),
        _ => format!("File modified: {}", file.display()),
    }
}

fn add(args: &[String], cwd: &Path, as_json: bool) -> anyhow::Result<i32> {
    let parsed = match parse_add(args, as_json) {
        Ok(parsed) => parsed,
        Err(message) => return fail(message),
    };
    let Some(name) = parsed.positional.first().cloned() else { return fail("error: missing required argument 'name'") };
    let Some(target) = parsed.positional.get(1).cloned() else {
        return fail(if as_json { "error: missing required argument 'json'" } else { "error: missing required argument 'commandOrUrl'" });
    };
    let config = if as_json {
        let text = if target == "-" {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text
        } else {
            target
        };
        match json_config(&text) {
            Ok(config) => config,
            Err(message) => return fail(message),
        }
    } else {
        match flag_config(&parsed, &target) {
            Ok(config) => config,
            Err(message) => return fail(message),
        }
    };
    let scope = parsed.scope;
    if !mcp::files_with(cwd, scope, &name).is_empty() {
        return fail(format!("MCP server {name} already exists in {} config", scope.name()));
    }
    let place = if parsed.remote { Place::Remote } else { Place::Here };
    let secret = if parsed.client_secret {
        if place == Place::Remote || !oauth::supports(&config) {
            return fail("--client-secret requires an HTTP or SSE MCP server that runs on this machine");
        }
        if parsed.positional.len() > 2 {
            return fail("--client-secret takes no value; use MCP_CLIENT_SECRET or the terminal prompt");
        }
        let mut expanded = Value::Object(config.clone());
        mcp::expand(&mut expanded);
        let expanded = expanded.as_object().context("invalid MCP server configuration")?;
        if oauth::client_id(expanded).is_none() {
            return fail("--client-secret requires an OAuth client ID");
        }
        let store = oauth::Store::for_scope(&name, expanded, cwd, scope).context("OAuth client secret requires an MCP server URL")?;
        let secret = match read_client_secret() {
            Ok(secret) => secret,
            Err(error) => return fail(error),
        };
        Some((store, secret))
    } else {
        None
    };
    let file = mcp::add(cwd, scope, &name, config.clone(), place)?;
    if let Some((store, secret)) = secret {
        if let Err(error) = store.set_client_secret(secret) {
            if let Err(rollback) = mcp::remove(cwd, Some(scope), &name) {
                return fail(format!("Could not store the OAuth client secret: {error:#}; could not remove the server configuration: {rollback:#}"));
            }
            return fail(format!("Could not store the OAuth client secret: {error:#}"));
        }
    }
    let file = if scope == Scope::Project { PathBuf::from(mcp::project_key(cwd)).join(".mcp.json") } else { file };
    let url = config.get("url").and_then(Value::as_str).unwrap_or("");
    match (as_json, kind(&config)) {
        (true, kind) => println!("Added {kind} MCP server {name} to {} config", scope.name()),
        (false, "stdio") => println!("Added stdio MCP server {name} with command: {} to {} config", summary(&config), scope.name()),
        (false, kind) => {
            println!("Added {} MCP server {name} with URL: {url} to {} config", kind.to_uppercase(), scope.name());
            if let Some(headers) = config.get("headers") {
                println!("Headers: {}", serde_json::to_string_pretty(headers)?);
            }
        }
    }
    if place == Place::Remote {
        println!("Runs on: {}", Config::load()?.host);
    }
    if !as_json {
        println!("{}", file_modified(&file, scope, cwd));
    }
    if scope == Scope::Project {
        println!("Approved for this machine in .claude/settings.local.json");
    }
    Ok(0)
}

fn flag_config(parsed: &AddArgs, target: &str) -> Result<Object, String> {
    let mut env = Object::new();
    for pair in &parsed.env {
        let Some((key, value)) = pair.split_once('=').filter(|(k, _)| !k.is_empty()) else {
            return Err(format!("Invalid environment variable format: {pair}, environment variables should be added as: -e KEY1=value1 -e KEY2=value2"));
        };
        env.insert(key.to_owned(), json!(value));
    }
    let mut headers = Object::new();
    for header in &parsed.headers {
        let Some((key, value)) = header.split_once(':').filter(|(k, _)| !k.trim().is_empty()) else {
            return Err(format!("Invalid header format: \"{header}\". Expected format: \"Header-Name: value\""));
        };
        headers.insert(key.trim().to_owned(), json!(value.trim()));
    }
    let port = match &parsed.callback_port {
        Some(port) => match port.parse::<u16>() {
            Ok(port) if port > 0 => Some(port),
            _ => return Err("Error: --callback-port must be a whole number from 1 to 65535".into()),
        },
        None => None,
    };
    let transport = match parsed.transport.as_str() {
        "streamable-http" => "http",
        t @ ("stdio" | "sse" | "http") => t,
        other => return Err(format!("Invalid transport type: {other}. Must be one of: stdio, sse, http (or streamable-http)")),
    };
    let mut config = Object::new();
    config.insert("type".into(), json!(transport));
    if transport == "stdio" {
        config.insert("command".into(), json!(target));
        config.insert("args".into(), json!(parsed.positional[2..]));
        config.insert("env".into(), Value::Object(env));
        return Ok(config);
    }
    config.insert("url".into(), json!(target));
    if !headers.is_empty() {
        config.insert("headers".into(), Value::Object(headers));
    }
    let mut oauth = Object::new();
    if let Some(id) = &parsed.client_id {
        oauth.insert("clientId".into(), json!(id));
    }
    if let Some(port) = port {
        oauth.insert("callbackPort".into(), json!(port));
    }
    if !oauth.is_empty() {
        config.insert("oauth".into(), Value::Object(oauth));
    }
    Ok(config)
}

fn json_config(text: &str) -> Result<Object, String> {
    let Ok(Value::Object(mut config)) = serde_json::from_str::<Value>(text) else {
        return Err("Invalid configuration: not a JSON object. Check its quotes, commas and braces.".into());
    };
    if config.get("oauth").and_then(|value| value.get("clientSecret")).is_some() {
        return Err("Do not put OAuth client secrets in JSON; use --client-secret".into());
    }
    let required = match config.get("type").and_then(Value::as_str) {
        None | Some("stdio") => "command",
        Some("streamable-http") => {
            config.insert("type".into(), json!("http"));
            "url"
        }
        Some("sse" | "http" | "ws") => "url",
        Some(_) => return Err("Invalid configuration: unknown \"type\". Use \"stdio\", \"sse\", \"http\" (or \"streamable-http\") or \"ws\".".into()),
    };
    if !config.get(required).is_some_and(Value::is_string) {
        return Err(format!("Invalid configuration: {required}: expected string, received undefined"));
    }
    Ok(config)
}

fn remove(args: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let mut scope = None;
    let mut name = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let given = match arg.as_str() {
            "-s" | "--scope" => match args.get(i) {
                Some(v) => {
                    i += 1;
                    v.as_str()
                }
                None => return fail("error: option '-s, --scope <scope>' argument missing"),
            },
            a if a.starts_with("--scope=") => &a["--scope=".len()..],
            a if a.starts_with('-') => return fail(format!("error: unknown option '{a}'")),
            a => {
                name = Some(a.to_owned());
                continue;
            }
        };
        match Scope::parse(given) {
            Ok(s) => scope = Some(s),
            Err(e) => return fail(e),
        }
    }
    let Some(name) = name else { return fail("error: missing required argument 'name'") };
    match mcp::remove(cwd, scope, &name)? {
        Removal::Removed(scope, file) => {
            let file = if scope == Scope::Project { PathBuf::from(mcp::project_key(cwd)).join(".mcp.json") } else { file };
            println!("Removed MCP server \"{name}\" from {} config", scope.name());
            println!("{}", file_modified(&file, scope, cwd));
            Ok(0)
        }
        Removal::Missing => match scope {
            Some(Scope::Project) => fail(format!("No MCP server named \"{name}\" in .mcp.json")),
            Some(scope) => fail(format!("No MCP server named \"{name}\" in {} scope", scope.name())),
            None => fail(format!("No MCP server named \"{name}\". Configured servers: {}", names(&mcp::servers(cwd, true), false))),
        },
        Removal::Ambiguous(found) => {
            let mut message = format!("MCP server \"{name}\" exists in multiple scopes:\n");
            for (scope, file) in &found {
                let shown = file_modified(file, *scope, cwd);
                message.push_str(&format!("  - {} ({})\n", scope.label(), shown.trim_start_matches("File modified: ")));
            }
            message.push_str("\nTo remove from a specific scope, use:\n");
            let lines: Vec<String> = found.iter().map(|(scope, _)| format!("  claude mcp remove {name} -s {}", scope.name())).collect();
            message.push_str(&lines.join("\n"));
            fail(message)
        }
    }
}
