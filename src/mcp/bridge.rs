//! An MCP server the host configured for claude, run on this machine. A local stdio command or a
//! server on localhost (an SDK host's own, for one) is out of the remote claude's reach, and headers
//! can hold credentials; so the relay connects to it here and offers its complete MCP interface to
//! claude as an SDK server.
//!
//! The client is MCP's wire protocol and nothing more: messages pass through as JSON, so resources,
//! prompts, tools, completions and server notifications all reach claude unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls_platform_verifier::ConfigVerifierExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http;
use tokio_util::sync::CancellationToken;

use super::{INTERNAL_ERROR, LATEST_PROTOCOL_VERSION, Source, oauth, rpc_error, rpc_id, rpc_result};
use crate::json::Object;
use crate::VERSION;

type WsSink = futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, WsMessage>;

/// A tool call may run as long as the local Bash tool's maximum, plus margin.
const CALL_TIMEOUT: Duration = Duration::from_secs(660);

const HELPER_TIMEOUT: Duration = Duration::from_secs(10);

const CONNECT_TIMEOUT_MS: u64 = 30_000;

pub type Notify = Arc<dyn Fn(Value) + Send + Sync>;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

fn string_map(value: Option<&Value>) -> Vec<(String, String)> {
    value.and_then(Value::as_object).map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect()).unwrap_or_default()
}

fn unreachable(error: reqwest::Error) -> Failure {
    let mut cause: &dyn std::error::Error = &error;
    while let Some(next) = cause.source() {
        cause = next;
    }
    Failure::Other(format!("{error}: {cause}"))
}

#[derive(Debug)]
pub enum Failure {
    Status(u16, String),
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Status(code, body) if body.is_empty() => write!(f, "HTTP {code}"),
            Failure::Status(code, body) => write!(f, "HTTP {code}: {body}"),
            Failure::Other(text) => f.write_str(text),
        }
    }
}

impl From<String> for Failure {
    fn from(text: String) -> Failure {
        Failure::Other(text)
    }
}

impl From<&str> for Failure {
    fn from(text: &str) -> Failure {
        Failure::Other(text.to_owned())
    }
}

impl Failure {
    fn unauthorized(&self) -> bool {
        matches!(self, Failure::Status(401 | 403, _))
    }
}

#[derive(Clone, Debug)]
pub struct Launch {
    pub dirs: Vec<PathBuf>,
    pub source: Source,
}

impl Launch {
    pub fn new(cwd: &Path, extra_dirs: &[PathBuf], source: Source) -> Launch {
        Launch { dirs: std::iter::once(cwd.to_path_buf()).chain(extra_dirs.iter().cloned()).collect(), source }
    }

    fn cwd(&self) -> &Path {
        &self.dirs[0]
    }

    fn roots(&self) -> Value {
        let mut uris: Vec<String> = vec![];
        for dir in &self.dirs {
            for path in [std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone()), dir.clone()] {
                let uri = format!("file://{}", path.display());
                if !uris.contains(&uri) {
                    uris.push(uri);
                }
            }
        }
        json!({ "roots": uris.into_iter().map(|uri| json!({ "uri": uri })).collect::<Vec<_>>() })
    }
}

fn is_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("GIT_CONFIG_KEY_") && upper["GIT_CONFIG_KEY_".len()..].bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    ["TOKEN", "SECRET", "PASSWORD", "KEY", "AUTH"].iter().any(|word| upper.contains(word)) || upper == "ANTHROPIC_CUSTOM_HEADERS"
}

async fn run_helper(name: &str, command: &str, url: &str, launch: &Launch) -> Result<Vec<(String, String)>, Failure> {
    let (dir, plugin_root) = match &launch.source {
        Source::Caller | Source::Local => (launch.cwd().to_path_buf(), None),
        Source::Project(dir) => (dir.clone(), None),
        Source::User => (crate::settings::user_dir(), None),
        Source::Plugin(root) => (root.clone(), Some(root.clone())),
    };
    let supplied = matches!(launch.source, Source::Project(_) | Source::Plugin(_));
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(command).current_dir(&dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    if supplied {
        shell.env_clear().envs(std::env::vars().filter(|(k, _)| !is_credential(k)));
    }
    shell.envs(crate::settings::local_env().iter().filter(|(k, _)| !supplied || !is_credential(k)).map(|(k, v)| (k, v)));
    shell.env("CLAUDE_CODE_MCP_SERVER_NAME", name).env("CLAUDE_CODE_MCP_SERVER_URL", url);
    if let Some(root) = plugin_root {
        shell.env("CLAUDE_PLUGIN_ROOT", root);
    }
    let out = match tokio::time::timeout(HELPER_TIMEOUT, shell.output()).await {
        Ok(out) => out.map_err(|e| format!("headersHelper for {name} could not start: {e}"))?,
        Err(_) => return Err(format!("headersHelper for {name} timed out after {}s", HELPER_TIMEOUT.as_secs()).into()),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(format!("headersHelper for {name} failed ({}){}", out.status, if stderr.is_empty() { String::new() } else { format!(": {stderr}") }).into());
    }
    match serde_json::from_slice::<Value>(&out.stdout) {
        Ok(Value::Object(headers)) if headers.values().all(Value::is_string) => Ok(string_map(Some(&Value::Object(headers)))),
        _ => Err(format!("headersHelper for {name} must print a JSON object of string values").into()),
    }
}

pub(super) async fn configured_headers(name: &str, config: &Object, url: &str, launch: &Launch) -> Result<Vec<(String, String)>, Failure> {
    let mut headers = string_map(config.get("headers"));
    if let Some(command) = config.get("headersHelper").and_then(Value::as_str) {
        for (key, value) in run_helper(name, command, url, launch).await? {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
            headers.push((key, value));
        }
    }
    Ok(headers)
}

fn has_authorization(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(key, _)| key.eq_ignore_ascii_case("authorization"))
}

fn auth_failure(name: &str, error: anyhow::Error) -> Failure {
    format!("{error:#}. Run `claude mcp login {name}` to authenticate").into()
}

async fn headers(name: &str, config: &Object, url: &str, launch: &Launch) -> Result<Vec<(String, String)>, Failure> {
    let mut headers = configured_headers(name, config, url, launch).await?;
    if !has_authorization(&headers)
        && let Some(store) = oauth::Store::new(name, config, launch.cwd(), &launch.source)
        && let Some(token) = store.access_token().await.map_err(|err| auth_failure(name, err))?
    {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }
    Ok(headers)
}

async fn refresh_oauth(name: &str, config: &Object, launch: &Launch, rejected: Option<&str>) -> Result<bool, Failure> {
    let Some(store) = oauth::Store::new(name, config, launch.cwd(), &launch.source) else { return Ok(false) };
    let url = config.get("url").and_then(Value::as_str).ok_or("MCP server without a URL")?;
    if has_authorization(&configured_headers(name, config, url, launch).await?) {
        return Ok(false);
    }
    store.refresh_rejected(rejected).await.map_err(|err| auth_failure(name, err))
}

/// Server-sent events from a growing buffer: (event, data, id) for each complete event.
fn take_events(buffer: &mut String) -> Vec<(String, String, Option<String>)> {
    let mut events = vec![];
    loop {
        let normalized = buffer.replace("\r\n", "\n");
        let Some(end) = normalized.find("\n\n") else {
            *buffer = normalized;
            return events;
        };
        let (block, rest) = (normalized[..end].to_owned(), normalized[end + 2..].to_owned());
        *buffer = rest;
        let mut event = String::from("message");
        let mut data = vec![];
        let mut id = None;
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_owned();
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push(v.strip_prefix(' ').unwrap_or(v).to_owned());
            } else if let Some(v) = line.strip_prefix("id:") {
                id = Some(v.trim().to_owned());
            }
        }
        if !data.is_empty() {
            events.push((event, data.join("\n"), id));
        }
    }
}

enum Transport {
    Stdio { stdin: tokio::sync::Mutex<ChildStdin>, child: tokio::sync::Mutex<Child> },
    Http { url: String, headers: Vec<(String, String)>, session: Mutex<Option<String>>, version: Mutex<String>, last_event_id: Mutex<Option<String>> },
    Sse { post: String, headers: Vec<(String, String)> },
    Ws { sink: tokio::sync::Mutex<WsSink> },
}

struct Client {
    transport: Transport,
    pending: Mutex<HashMap<u64, oneshot::Sender<Object>>>,
    next: AtomicU64,
    notify: Notify,
    roots: Value,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    initialize: Mutex<Value>,
}

impl Client {
    async fn connect(name: &str, config: &Object, launch: &Launch, notify: Notify) -> Result<Arc<Client>, Failure> {
        let kind = config.get("type").and_then(Value::as_str);
        let url = config.get("url").and_then(Value::as_str);
        let roots = launch.roots();
        match kind {
            Some("http" | "streamable-http") => {
                let url = url.ok_or("http server without a url")?;
                let headers = headers(name, config, url, launch).await?;
                let transport = Transport::Http { url: url.to_owned(), headers: headers.clone(), session: Mutex::new(None), version: Mutex::new(LATEST_PROTOCOL_VERSION.into()), last_event_id: Mutex::new(None) };
                let client = Client::new(transport, notify.clone(), roots.clone());
                match client.initialize().await {
                    Ok(()) => Ok(client),
                    Err(Failure::Status(400 | 404 | 405, _)) => {
                        client.close().await;
                        let client = Client::connect_sse(url, headers, notify, roots).await?;
                        client.initialize().await?;
                        Ok(client)
                    }
                    Err(e) => {
                        client.close().await;
                        Err(e)
                    }
                }
            }
            Some("sse") => {
                let url = url.ok_or("sse server without a url")?;
                let headers = headers(name, config, url, launch).await?;
                let client = Client::connect_sse(url, headers, notify, roots).await?;
                client.initialize().await?;
                Ok(client)
            }
            Some("ws") => {
                let url = url.ok_or("ws server without a url")?;
                let headers = headers(name, config, url, launch).await?;
                let client = Client::connect_ws(url, headers, notify, roots).await?;
                if let Err(e) = client.initialize().await {
                    client.close().await;
                    return Err(e);
                }
                Ok(client)
            }
            None | Some("stdio") => {
                let command = config.get("command").and_then(Value::as_str).ok_or("stdio server without a command")?;
                let args: Vec<&str> = config.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let mut child = Command::new(command)
                    .args(args)
                    .envs(crate::settings::local_env().iter().map(|(k, v)| (k, v)))
                    .env("CLAUDE_PROJECT_DIR", launch.cwd())
                    .envs(string_map(config.get("env")))
                    .current_dir(launch.cwd())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("could not start {command}: {e}"))?;
                let stdin = child.stdin.take().expect("piped");
                let stdout = child.stdout.take().expect("piped");
                let client = Client::new(Transport::Stdio { stdin: tokio::sync::Mutex::new(stdin), child: tokio::sync::Mutex::new(child) }, notify, roots);
                let weak = Arc::downgrade(&client);
                let reader = tokio::spawn(async move {
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let Some(client) = weak.upgrade() else { break };
                        client.receive(&line).await;
                    }
                    if let Some(client) = weak.upgrade() {
                        client.pending.lock().unwrap().clear();
                    }
                });
                client.tasks.lock().unwrap().push(reader);
                if let Err(e) = client.initialize().await {
                    client.close().await;
                    return Err(e);
                }
                Ok(client)
            }
            Some(other) => Err(format!("claudette cannot run MCP servers of type {other} locally").into()),
        }
    }

    fn new(transport: Transport, notify: Notify, roots: Value) -> Arc<Client> {
        Arc::new(Client { transport, pending: Mutex::new(HashMap::new()), next: AtomicU64::new(1), notify, roots, tasks: Mutex::new(vec![]), initialize: Mutex::new(Value::Null) })
    }

    async fn initialize(self: &Arc<Self>) -> Result<(), Failure> {
        let capabilities = json!({ "roots": { "listChanged": true }, "elicitation": { "form": {}, "url": {} } });
        let params = json!({ "protocolVersion": LATEST_PROTOCOL_VERSION, "capabilities": capabilities, "clientInfo": { "name": "claudette", "version": VERSION } });
        let result = self.request("initialize", params, &CancellationToken::new()).await?;
        let result = result.map_err(|e| format!("initialize failed: {e}"))?;
        *self.initialize.lock().unwrap() = result.clone();
        if let (Transport::Http { version, .. }, Some(v)) = (&self.transport, result.get("protocolVersion").and_then(Value::as_str)) {
            *version.lock().unwrap() = v.to_owned();
        }
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await?;
        if let Transport::Http { .. } = &self.transport {
            self.start_http_stream();
        }
        Ok(())
    }

    /// Streamable HTTP servers may use a GET stream for messages that are not responses to POSTs.
    fn start_http_stream(self: &Arc<Self>) {
        let Transport::Http { url, headers, .. } = &self.transport else { return };
        let (url, headers) = (url.clone(), headers.clone());
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            loop {
                let Some(client) = weak.upgrade() else { break };
                let (session, version, last_event_id) = match &client.transport {
                    Transport::Http { session, version, last_event_id, .. } => (session.lock().unwrap().clone(), version.lock().unwrap().clone(), last_event_id.lock().unwrap().clone()),
                    _ => break,
                };
                let mut request = HTTP.get(&url).header("accept", "text/event-stream").header("mcp-protocol-version", version);
                for (k, v) in &headers { request = request.header(k, v); }
                if let Some(session) = session { request = request.header("mcp-session-id", session); }
                if let Some(last_event_id) = last_event_id { request = request.header("last-event-id", last_event_id); }
                let Ok(mut response) = request.send().await else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                if response.status().as_u16() == 405 { break; }
                if !response.status().is_success() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                let mut buffer = String::new();
                while let Ok(Some(chunk)) = response.chunk().await {
                    buffer.push_str(&String::from_utf8_lossy(&chunk));
                    for (event, data, id) in take_events(&mut buffer) {
                        if let Some(id) = id
                            && let Transport::Http { last_event_id, .. } = &client.transport
                        {
                            *last_event_id.lock().unwrap() = Some(id);
                        }
                        if event == "message" { client.receive(&data).await; }
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        self.tasks.lock().unwrap().push(task);
    }

    /// The legacy SSE transport: a GET stream whose first event names the URL to POST messages to.
    async fn connect_sse(url: &str, headers: Vec<(String, String)>, notify: Notify, roots: Value) -> Result<Arc<Client>, Failure> {
        let mut request = HTTP.get(url).header("accept", "text/event-stream");
        for (k, v) in &headers {
            request = request.header(k, v);
        }
        let mut response = request.send().await.map_err(unreachable)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err(Failure::Status(status, response.text().await.unwrap_or_default().trim().to_owned()));
        }
        let mut buffer = String::new();
        let post = loop {
            let chunk = response.chunk().await.map_err(|e| e.to_string())?.ok_or("the SSE stream ended before naming its endpoint")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            if let Some((_, data, _)) = take_events(&mut buffer).into_iter().find(|(event, _, _)| event == "endpoint") {
                let base = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
                let post = base.join(&data).map_err(|e| e.to_string())?;
                if post.origin() != base.origin() {
                    return Err(format!("SSE POST endpoint has a different origin: {}", post.origin().ascii_serialization()).into());
                }
                break post.to_string();
            }
        };
        let client = Client::new(Transport::Sse { post, headers }, notify, roots);
        let weak = Arc::downgrade(&client);
        let reader = tokio::spawn(async move {
            while let Ok(Some(chunk)) = response.chunk().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                let Some(client) = weak.upgrade() else { break };
                for (event, data, _) in take_events(&mut buffer) {
                    if event == "message" {
                        client.receive(&data).await;
                    }
                }
            }
        });
        client.tasks.lock().unwrap().push(reader);
        Ok(client)
    }

    async fn connect_ws(url: &str, headers: Vec<(String, String)>, notify: Notify, roots: Value) -> Result<Arc<Client>, Failure> {
        let mut request = url.into_client_request().map_err(|e| e.to_string())?;
        for (k, v) in headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).chain([("sec-websocket-protocol", "mcp")]) {
            let name = http::HeaderName::from_bytes(k.as_bytes()).map_err(|e| format!("header {k}: {e}"))?;
            let value = http::HeaderValue::from_str(v).map_err(|e| format!("header {k}: {e}"))?;
            request.headers_mut().insert(name, value);
        }
        let tls = rustls::ClientConfig::with_platform_verifier().map_err(|e| e.to_string())?;
        let connector = tokio_tungstenite::Connector::Rustls(Arc::new(tls));
        let (stream, _) = match tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector)).await {
            Ok(connected) => connected,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                let body = response.body().as_ref().map(|b| String::from_utf8_lossy(b).trim().to_owned()).unwrap_or_default();
                return Err(Failure::Status(response.status().as_u16(), body));
            }
            Err(e) => return Err(e.to_string().into()),
        };
        let (sink, mut source) = stream.split();
        let client = Client::new(Transport::Ws { sink: tokio::sync::Mutex::new(sink) }, notify, roots);
        let weak = Arc::downgrade(&client);
        let reader = tokio::spawn(async move {
            while let Some(Ok(message)) = source.next().await {
                let Some(client) = weak.upgrade() else { break };
                match message {
                    WsMessage::Text(text) => client.receive(&text).await,
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
            if let Some(client) = weak.upgrade() {
                client.pending.lock().unwrap().clear();
            }
        });
        client.tasks.lock().unwrap().push(reader);
        Ok(client)
    }

    /// One message from the server: a response completes its request; requests and notifications
    /// from the server get their due.
    async fn receive(self: &Arc<Self>, text: &str) {
        let Ok(Value::Object(message)) = serde_json::from_str::<Value>(text) else { return };
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        match (method, rpc_id(&message)) {
            (None, Some(id)) => {
                if let Some(waiter) = id.as_u64().and_then(|id| self.pending.lock().unwrap().remove(&id)) {
                    let _ = waiter.send(message);
                }
            }
            (Some(method), Some(id)) if method == "ping" || method == "roots/list" => {
                let reply = if method == "ping" { rpc_result(id, json!({})) } else { rpc_result(id, self.roots.clone()) };
                let client = self.clone();
                tokio::spawn(async move {
                    let _ = client.send(reply).await;
                });
            }
            (Some(_), _) => {
                // Notifications are not limited to tools: resource, prompt, progress and log
                // updates are meaningful to Claude and must retain their original parameters.
                (self.notify)(Value::Object(message));
            }
            _ => {}
        }
    }

    async fn post(self: &Arc<Self>, url: &str, headers: &[(String, String)], message: &Value) -> Result<(), Failure> {
        let mut request = HTTP.post(url).header("content-type", "application/json").header("accept", "application/json, text/event-stream").body(message.to_string());
        for (k, v) in headers {
            request = request.header(k, v);
        }
        if let Transport::Http { session, version, .. } = &self.transport {
            if let Some(id) = session.lock().unwrap().clone() {
                request = request.header("mcp-session-id", id);
            }
            request = request.header("mcp-protocol-version", version.lock().unwrap().clone());
        }
        let mut response = request.send().await.map_err(unreachable)?;
        if let Transport::Http { session, .. } = &self.transport
            && let Some(id) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok())
        {
            *session.lock().unwrap() = Some(id.to_owned());
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err(Failure::Status(status, response.text().await.unwrap_or_default().trim().to_owned()));
        }
        let streamed = response.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
        if streamed {
            let mut buffer = String::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                for (event, data, id) in take_events(&mut buffer) {
                    if let Some(id) = id
                        && let Transport::Http { last_event_id, .. } = &self.transport
                    {
                        *last_event_id.lock().unwrap() = Some(id);
                    }
                    if event == "message" {
                        self.receive(&data).await;
                    }
                }
            }
        } else {
            let body = response.text().await.map_err(|e| e.to_string())?;
            if !body.trim().is_empty() {
                self.receive(&body).await;
            }
        }
        Ok(())
    }

    /// Boxed: a server's request answered while a POST is streaming sends from inside `post`.
    fn send(self: &Arc<Self>, message: Value) -> futures_util::future::BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move {
            match &self.transport {
                Transport::Stdio { stdin, .. } => {
                    let mut stdin = stdin.lock().await;
                    stdin.write_all(format!("{message}\n").as_bytes()).await.map_err(|e| format!("the server's stdin closed: {e}"))?;
                    stdin.flush().await.map_err(|e| Failure::Other(e.to_string()))
                }
                Transport::Http { url, headers, .. } => self.post(url, headers, &message).await,
                Transport::Sse { post, headers } => self.post(post, headers, &message).await,
                Transport::Ws { sink } => sink.lock().await.send(WsMessage::Text(message.to_string().into())).await.map_err(|e| Failure::Other(e.to_string())),
            }
        })
    }

    /// A request and its response: Ok(Ok(result)), or Ok(Err(error object)) when the server refused.
    async fn request(self: &Arc<Self>, method: &str, params: Value, cancel: &CancellationToken) -> Result<Result<Value, Value>, Failure> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let exchange = async {
            self.send(message).await?;
            rx.await.map_err(|_| Failure::from("the server closed the connection"))
        };
        let outcome = tokio::select! {
            outcome = tokio::time::timeout(CALL_TIMEOUT, exchange) => outcome.unwrap_or_else(|_| Err(format!("no answer within {}s", CALL_TIMEOUT.as_secs()).into())),
            _ = cancel.cancelled() => {
                let _ = self.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id } })).await;
                Err("cancelled".into())
            }
        };
        self.pending.lock().unwrap().remove(&id);
        let response = outcome?;
        match response.get("error") {
            Some(error) => Ok(Err(error.clone())),
            None => Ok(Ok(response.get("result").cloned().unwrap_or(json!({})))),
        }
    }

    async fn close(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        if let Transport::Stdio { child, .. } = &self.transport {
            let _ = child.lock().await.kill().await;
        }
    }
}

pub enum Health {
    Connected,
    NeedsAuth,
    Failed(String),
}

impl Health {
    pub fn to_json(&self) -> Value {
        match self {
            Health::Connected => json!({ "connected": true }),
            Health::NeedsAuth => json!({ "needsAuth": true }),
            Health::Failed(detail) => json!({ "failed": detail }),
        }
    }

    pub fn from_json(value: &Value) -> Health {
        if value.get("connected").is_some() {
            Health::Connected
        } else if value.get("needsAuth").is_some() {
            Health::NeedsAuth
        } else {
            Health::Failed(value.get("failed").and_then(Value::as_str).unwrap_or("no answer from the remote machine").to_owned())
        }
    }
}

pub async fn probe(name: &str, config: &Object, launch: &Launch) -> Health {
    let ms = std::env::var("MCP_TIMEOUT").ok().and_then(|v| v.parse().ok()).filter(|ms| *ms > 0).unwrap_or(CONNECT_TIMEOUT_MS);
    let ignore: Notify = Arc::new(|_| {});
    let connect = async {
        let result = Client::connect(name, config, launch, ignore.clone()).await;
        if matches!(result, Err(Failure::Status(401, _))) && refresh_oauth(name, config, launch, None).await? {
            return Client::connect(name, config, launch, ignore).await;
        }
        result
    };
    match tokio::time::timeout(Duration::from_millis(ms), connect).await {
        Err(_) => Health::Failed(format!("MCP server \"{name}\" connection timed out after {ms}ms")),
        Ok(Ok(client)) => {
            client.close().await;
            Health::Connected
        }
        Ok(Err(Failure::Status(401, _))) if config.get("headersHelper").is_none() && oauth::supports(config) => Health::NeedsAuth,
        Ok(Err(e)) => Health::Failed(e.to_string()),
    }
}

pub struct Bridge {
    pub name: String,
    /// Identifies the configuration, so an unchanged server keeps its connection across `mcp_set_servers`.
    pub key: String,
    config: Object,
    launch: Launch,
    notify: Mutex<Option<Notify>>,
    client: tokio::sync::Mutex<Option<Arc<Client>>>,
}

impl Bridge {
    pub fn new(name: &str, config: Object, launch: Launch) -> Bridge {
        Bridge { name: name.to_owned(), key: Value::Object(config.clone()).to_string(), config, launch, notify: Mutex::new(None), client: tokio::sync::Mutex::new(None) }
    }

    /// Where the server's own MCP notifications go.
    pub fn set_notify(&self, notify: Notify) {
        *self.notify.lock().unwrap() = Some(notify);
    }

    async fn connect(self: &Arc<Self>) -> Result<Arc<Client>, Failure> {
        let mut slot = self.client.lock().await;
        if let Some(client) = &*slot {
            return Ok(client.clone());
        }
        let weak: Weak<Bridge> = Arc::downgrade(self);
        let notify: Notify = Arc::new(move |message| {
            if let Some(bridge) = weak.upgrade()
                && let Some(notify) = bridge.notify.lock().unwrap().clone()
            {
                notify(message);
            }
        });
        let client = Client::connect(&self.name, &self.config, &self.launch, notify).await?;
        *slot = Some(client.clone());
        Ok(client)
    }

    async fn call(self: &Arc<Self>, method: &str, params: &Value, cancel: &CancellationToken) -> Result<Result<Value, Value>, Failure> {
        match method {
            "initialize" => {
                let client = self.connect().await?;
                Ok(Ok(client.initialize.lock().unwrap().clone()))
            }
            "ping" => Ok(Ok(json!({}))),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Ok(Err(json!({ "code": INTERNAL_ERROR, "message": "tools/call without a tool name" })));
                };
                let arguments = params.get("arguments").filter(|a| a.is_object()).cloned().unwrap_or(json!({}));
                self.connect().await?.request(method, json!({ "name": name, "arguments": arguments }), cancel).await
            }
            // Resources, prompts, completions and anything newer go to the server as they are.
            other => self.connect().await?.request(other, params.clone(), cancel).await,
        }
    }

    pub async fn handle(self: &Arc<Self>, message: &Object, cancel: &CancellationToken) -> Option<Value> {
        let id = rpc_id(message);
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(json!({}));
        if id.is_none() || !message.contains_key("method") {
            // Relay notifications have no response frame. Sending them unchanged keeps
            // cancellation and every MCP-defined client notification useful to the server.
            if let Ok(client) = self.connect().await {
                let _ = client.send(Value::Object(message.clone())).await;
            }
            return None;
        }
        let id = id?;
        let mut outcome = self.call(method, &params, cancel).await;
        if matches!(method, "initialize" | "tools/call") && matches!(outcome, Err(Failure::Status(401, _))) {
            let rejected = self.client.lock().await.as_ref().and_then(|client| {
                let headers = match &client.transport {
                    Transport::Http { headers, .. } | Transport::Sse { headers, .. } => headers,
                    _ => return None,
                };
                headers.iter().find(|(key, _)| key.eq_ignore_ascii_case("authorization"))
                    .and_then(|(_, value)| value.strip_prefix("Bearer ")).map(str::to_owned)
            });
            let refresh = tokio::select! {
                result = refresh_oauth(&self.name, &self.config, &self.launch, rejected.as_deref()) => result,
                _ = cancel.cancelled() => Err("cancelled".into()),
            };
            match refresh {
                Ok(retry) if retry || (method == "tools/call" && self.config.contains_key("headersHelper")) => {
                    self.close().await;
                    outcome = self.call(method, &params, cancel).await;
                }
                Err(error) => outcome = Err(error),
                Ok(_) => {}
            }
        } else if method == "tools/call" && outcome.as_ref().is_err_and(Failure::unauthorized) && self.config.contains_key("headersHelper") {
            self.close().await;
            outcome = self.call(method, &params, cancel).await;
        }
        if matches!(outcome, Err(Failure::Status(401, _))) && oauth::supports(&self.config) {
            outcome = Err(format!("HTTP 401. Run `claude mcp login {}` to authenticate", self.name).into());
        }
        Some(match outcome {
            Ok(Ok(result)) => rpc_result(id, result),
            Ok(Err(error)) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
            Err(text) => rpc_error(id, INTERNAL_ERROR, &format!("claudette could not reach the MCP server {} on this machine: {text}", self.name)),
        })
    }

    pub async fn close(&self) {
        if let Some(client) = self.client.lock().await.take() {
            client.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_events_preserves_event_ids_for_reconnects() {
        let mut buffer = r#"id: 42
event: message
data: {"method":"notifications/message"}

"#.to_owned();
        let events = take_events(&mut buffer);
        assert_eq!(events, vec![("message".to_owned(), r#"{"method":"notifications/message"}"#.to_owned(), Some("42".to_owned()))]);
        assert!(buffer.is_empty());
    }

    #[tokio::test]
    async fn receive_forwards_resource_notifications() {
        let seen = Arc::new(Mutex::new(None));
        let target = seen.clone();
        let client = Client::new(
            Transport::Http {
                url: String::new(),
                headers: vec![],
                session: Mutex::new(None),
                version: Mutex::new(LATEST_PROTOCOL_VERSION.to_owned()),
                last_event_id: Mutex::new(None),
            },
            Arc::new(move |message| *target.lock().unwrap() = Some(message)),
            Value::Null,
        );
        client.receive(r#"{"jsonrpc":"2.0","method":"notifications/resources/list_changed","params":{}}"#).await;
        assert_eq!(seen.lock().unwrap().as_ref().and_then(|m| m.get("method")).and_then(Value::as_str), Some("notifications/resources/list_changed"));
    }

    #[test]
    fn credential_variables_match_claude_codes_pattern() {
        for name in ["ANTHROPIC_API_KEY", "my_registry_token", "DB_PASSWORD", "GH_AUTH", "ANTHROPIC_CUSTOM_HEADERS", "client_secret"] {
            assert!(is_credential(name), "{name}");
        }
        for name in ["PATH", "HOME", "GIT_CONFIG_KEY_0", "LANG"] {
            assert!(!is_credential(name), "{name}");
        }
    }
}
