//! An MCP server the host configured for claude, run on this machine. A local stdio command or a
//! server on localhost (an SDK host's own, for one) is out of the remote claude's reach, and headers
//! can hold credentials; so the relay connects to it here and offers its complete MCP interface to
//! claude as an SDK server. Servers that need an OAuth login are not supported.
//!
//! The client is MCP's wire protocol and nothing more: messages pass through as JSON, so resources,
//! prompts, tools, completions and server notifications all reach claude unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{INTERNAL_ERROR, LATEST_PROTOCOL_VERSION, METHOD_NOT_FOUND, rpc_error, rpc_id, rpc_result};
use crate::json::Object;
use crate::VERSION;

/// A tool call may run as long as the local Bash tool's maximum, plus margin.
const CALL_TIMEOUT: Duration = Duration::from_secs(660);

pub type Notify = Arc<dyn Fn(Value) + Send + Sync>;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

fn string_map(value: Option<&Value>) -> Vec<(String, String)> {
    value.and_then(Value::as_object).map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect()).unwrap_or_default()
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
}

struct Client {
    transport: Transport,
    pending: Mutex<HashMap<u64, oneshot::Sender<Object>>>,
    next: AtomicU64,
    notify: Notify,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    initialize: Mutex<Value>,
}


impl Client {
    async fn connect(config: &Object, cwd: &PathBuf, notify: Notify) -> Result<Arc<Client>, String> {
        let kind = config.get("type").and_then(Value::as_str);
        let url = config.get("url").and_then(Value::as_str);
        let headers = string_map(config.get("headers"));
        let client = match kind {
            Some("http") => {
                let url = url.ok_or("http server without a url")?.to_owned();
                Client::new(Transport::Http { url, headers, session: Mutex::new(None), version: Mutex::new(LATEST_PROTOCOL_VERSION.into()), last_event_id: Mutex::new(None) }, notify)
            }
            Some("sse") => {
                let url = url.ok_or("sse server without a url")?;
                Client::connect_sse(url, headers, notify).await?
            }
            None | Some("stdio") => {
                let command = config.get("command").and_then(Value::as_str).ok_or("stdio server without a command")?;
                let args: Vec<&str> = config.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let mut child = Command::new(command)
                    .args(args)
                    .envs(crate::settings::local_env().iter().map(|(k, v)| (k, v)))
                    .envs(string_map(config.get("env")))
                    .current_dir(cwd)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("could not start {command}: {e}"))?;
                let stdin = child.stdin.take().expect("piped");
                let stdout = child.stdout.take().expect("piped");
                let client = Client::new(Transport::Stdio { stdin: tokio::sync::Mutex::new(stdin), child: tokio::sync::Mutex::new(child) }, notify);
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
                client
            }
            Some(other) => return Err(format!("claudette cannot run MCP servers of type {other} locally")),
        };
        let result = client.request("initialize", json!({ "protocolVersion": LATEST_PROTOCOL_VERSION, "capabilities": {}, "clientInfo": { "name": "claudette", "version": VERSION } }), &CancellationToken::new()).await?;
        let result = result.map_err(|e| format!("initialize failed: {e}"))?;
        *client.initialize.lock().unwrap() = result.clone();
        if let (Transport::Http { version, .. }, Some(v)) = (&client.transport, result.get("protocolVersion").and_then(Value::as_str)) {
            *version.lock().unwrap() = v.to_owned();
        }
        client.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await?;
        if let Transport::Http { .. } = &client.transport {
            client.start_http_stream();
        }
        Ok(client)
    }
    fn new(transport: Transport, notify: Notify) -> Arc<Client> {
        Arc::new(Client { transport, pending: Mutex::new(HashMap::new()), next: AtomicU64::new(1), notify, tasks: Mutex::new(vec![]), initialize: Mutex::new(Value::Null) })
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
    async fn connect_sse(url: &str, headers: Vec<(String, String)>, notify: Notify) -> Result<Arc<Client>, String> {
        let mut request = HTTP.get(url).header("accept", "text/event-stream");
        for (k, v) in &headers {
            request = request.header(k, v);
        }
        let mut response = request.send().await.and_then(reqwest::Response::error_for_status).map_err(|e| e.to_string())?;
        let mut buffer = String::new();
        let post = loop {
            let chunk = response.chunk().await.map_err(|e| e.to_string())?.ok_or("the SSE stream ended before naming its endpoint")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            if let Some((_, data, _)) = take_events(&mut buffer).into_iter().find(|(event, _, _)| event == "endpoint") {
                break reqwest::Url::parse(url).and_then(|base| base.join(&data)).map_err(|e| e.to_string())?.to_string();
            }
        };
        let client = Client::new(Transport::Sse { post, headers }, notify);
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
            (Some(method), Some(id)) => {
                let reply = if method == "ping" { rpc_result(id, json!({})) } else { rpc_error(id, METHOD_NOT_FOUND, &format!("claudette does not answer {method}")) };
                let client = self.clone();
                tokio::spawn(async move {
                    let _ = client.send(reply).await;
                });
            }
            (Some(_), None) => {
                // Notifications are not limited to tools: resource, prompt, progress and log
                // updates are meaningful to Claude and must retain their original parameters.
                (self.notify)(Value::Object(message));
            }
            _ => {}
        }
    }

    async fn post(self: &Arc<Self>, url: &str, headers: &[(String, String)], message: &Value) -> Result<(), String> {
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
        let mut response = request.send().await.map_err(|e| e.to_string())?;
        if let Transport::Http { session, .. } = &self.transport
            && let Some(id) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok())
        {
            *session.lock().unwrap() = Some(id.to_owned());
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {}", body.trim()));
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
    fn send(self: &Arc<Self>, message: Value) -> futures_util::future::BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            match &self.transport {
                Transport::Stdio { stdin, .. } => {
                    let mut stdin = stdin.lock().await;
                    stdin.write_all(format!("{message}\n").as_bytes()).await.map_err(|e| format!("the server's stdin closed: {e}"))?;
                    stdin.flush().await.map_err(|e| e.to_string())
                }
                Transport::Http { url, headers, .. } => self.post(url, headers, &message).await,
                Transport::Sse { post, headers } => self.post(post, headers, &message).await,
            }
        })
    }

    /// A request and its response: Ok(Ok(result)), or Ok(Err(error object)) when the server refused.
    async fn request(self: &Arc<Self>, method: &str, params: Value, cancel: &CancellationToken) -> Result<Result<Value, Value>, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let exchange = async {
            self.send(message).await?;
            rx.await.map_err(|_| "the server closed the connection".to_owned())
        };
        let outcome = tokio::select! {
            outcome = tokio::time::timeout(CALL_TIMEOUT, exchange) => outcome.unwrap_or_else(|_| Err(format!("no answer within {}s", CALL_TIMEOUT.as_secs()))),
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

pub struct Bridge {
    pub name: String,
    /// Identifies the configuration, so an unchanged server keeps its connection across `mcp_set_servers`.
    pub key: String,
    config: Object,
    cwd: PathBuf,
    notify: Mutex<Option<Notify>>,
    client: tokio::sync::Mutex<Option<Arc<Client>>>,
}

impl Bridge {
    pub fn new(name: &str, config: Object, cwd: PathBuf) -> Bridge {
        Bridge { name: name.to_owned(), key: Value::Object(config.clone()).to_string(), config, cwd, notify: Mutex::new(None), client: tokio::sync::Mutex::new(None) }
    }

    /// Where the server's own MCP notifications go.
    pub fn set_notify(&self, notify: Notify) {
        *self.notify.lock().unwrap() = Some(notify);
    }

    async fn connect(self: &Arc<Self>) -> Result<Arc<Client>, String> {
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
        let client = Client::connect(&self.config, &self.cwd, notify).await?;
        *slot = Some(client.clone());
        Ok(client)
    }

    pub async fn handle(self: &Arc<Self>, message: &Object, cancel: &CancellationToken) -> Option<Value> {
        let id = rpc_id(message);
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(json!({}));
        if id.is_none() {
            // Relay notifications have no response frame. Sending them unchanged keeps
            // cancellation and every MCP-defined client notification useful to the server.
            if let Ok(client) = self.connect().await {
                let _ = client.send(Value::Object(message.clone())).await;
            }
            return None;
        }
        let id = id?;
        let call = async {
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
                other => self.connect().await?.request(other, params, cancel).await,
            }
        };
        Some(match call.await {
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
        );
        client.receive(r#"{"jsonrpc":"2.0","method":"notifications/resources/list_changed","params":{}}"#).await;
        assert_eq!(seen.lock().unwrap().as_ref().and_then(|m| m.get("method")).and_then(Value::as_str), Some("notifications/resources/list_changed"));
    }
}
