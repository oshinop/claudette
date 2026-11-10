//! Sits on the stream-json pipe between an Agent SDK host and the remote claude.
//!
//! Host → claude: `initialize` gains the local tool server, tool aliases and the local environment;
//! `mcp_set_servers` hands process-transport servers to local bridges; permission answers get the
//! local tools' rule names back.
//!
//! claude → host: `mcp_message` for relay-hosted servers is answered here and never reaches the host;
//! `can_use_tool` for local tools is answered here when the session's rules allow it, and otherwise
//! reaches the host under the built-in tool's name; transcript mirror frames are written locally;
//! everything else loses the `mcp__cc__` prefix and the remote working directory.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::{BoxFuture, Shared};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::args::ParsedArgs;
use crate::json::{Object, parse_object, str_field};
use crate::log::{trace, warn};
use crate::mcp::bridge::{Bridge, Notify};
use crate::mcp::local::LocalTools;
use crate::mcp::{Endpoint, INTERNAL_ERROR, id_key, rpc_error, rpc_id};
use crate::names::{EDIT_TOOLS, PREFIX, SERVER, is_local_tool, unprefix};
use crate::mount::transcripts::TranscriptMirror;
use crate::mount::workdir::PathMap;
use crate::permissions::policy::{Decision, Policy, command_prefix};
use crate::tools::Workspace;

/// Per-call limit for the local tools: Bash's 10-minute maximum plus margin.
const TOOL_TIMEOUT: u64 = 660_000;

/// Control requests the relay itself sends to claude carry this id prefix; their answers stop here.
const OWN_REQUEST: &str = "claudette-";

pub type EnvPrompt = Shared<BoxFuture<'static, String>>;

/// Writes one line to the host; synchronous, so lines reach the host in the order they were sent.
pub type HostOut = Arc<dyn Fn(String) + Send + Sync>;

/// Copies the MCP settings claude keeps for an SDK server (timeout, tool policy, loading).
fn sdk_entry(name: &str, config: &Object) -> Value {
    let mut entry = json!({ "type": "sdk", "name": name });
    for key in ["timeout", "tools", "alwaysLoad"] {
        if let Some(v) = config.get(key) {
            entry[key] = v.clone();
        }
    }
    entry
}

fn object_entry<'a>(parent: &'a mut Object, key: &str) -> &'a mut Object {
    if !parent.get(key).is_some_and(Value::is_object) {
        parent.insert(key.to_owned(), json!({}));
    }
    parent.get_mut(key).and_then(Value::as_object_mut).expect("just made an object")
}

pub struct Relay {
    pub policy: Policy,
    ws: Arc<Workspace>,
    local_tools: Vec<&'static str>,
    config_server_configs: Vec<(String, Object)>,
    /// Servers the remote claude starts itself (from its MCP config file); they ride along in every set.
    remote_servers: Vec<(String, Object)>,
    host_mirror: bool,
    /// The local tool server, unless the caller allowed none of the tools it replaces.
    local: Option<Arc<LocalTools>>,
    endpoints: Mutex<HashMap<String, Endpoint>>,
    /// can_use_tool requests forwarded to the host; their answers can carry permission updates.
    permission_requests: Mutex<HashSet<String>>,
    /// In-flight mcp_message requests from claude, by control request id.
    calls: Mutex<HashMap<String, CancellationToken>>,
    /// The same calls by `<server>:<JSON-RPC id>`, for notifications/cancelled.
    rpc_calls: Mutex<HashMap<String, CancellationToken>>,
    /// Calls claude withdrew with control_cancel_request; they get no answer.
    withdrawn: Mutex<HashSet<String>>,
    own_requests: Arc<AtomicU64>,
    paths: PathMap,
    mirror: TranscriptMirror,
    env_prompt: EnvPrompt,
    to_claude: UnboundedSender<String>,
    to_host: HostOut,
}

impl Relay {
    pub fn new(
        ws: Arc<Workspace>,
        session: &ParsedArgs,
        env_prompt: EnvPrompt,
        to_claude: UnboundedSender<String>,
        to_host: HostOut,
    ) -> Arc<Relay> {
        let local = (!session.local_tools.is_empty()).then(|| Arc::new(LocalTools::new(ws.clone(), &session.local_tools)));
        let config_server_configs: Vec<(String, Object)> = session.config_servers.iter().filter(|(name, _)| *name != SERVER).map(|(n, c)| (n.clone(), c.clone())).collect();
        let relay = Arc::new(Relay {
            policy: Policy::new(session.permission_mode.as_deref().unwrap_or("default"), session.bash_allow.clone(), session.bash_deny.clone(), session.tool_allows.clone()),
            local_tools: session.local_tools.clone(),
            host_mirror: session.host_mirror,
            local: local.clone(),
            endpoints: Mutex::new(HashMap::new()),
            permission_requests: Mutex::new(HashSet::new()),
            calls: Mutex::new(HashMap::new()),
            rpc_calls: Mutex::new(HashMap::new()),
            withdrawn: Mutex::new(HashSet::new()),
            own_requests: Arc::new(AtomicU64::new(0)),
            paths: PathMap::new(&ws.root),
            mirror: TranscriptMirror::new(&ws.root),
            env_prompt,
            to_claude,
            to_host,
            ws,
            config_server_configs,
            remote_servers: session.remote_servers.iter().filter(|(name, _)| *name != SERVER).map(|(n, c)| (n.clone(), c.clone())).collect(),
        });
        let mut endpoints = relay.endpoints.lock().unwrap();
        if let Some(local) = local {
            endpoints.insert(SERVER.to_owned(), Endpoint::Local(local));
        }
        for (name, config) in &relay.config_server_configs {
            endpoints.insert(name.clone(), relay.bridge(name, config.clone()));
        }
        drop(endpoints);
        relay
    }

    fn bridge(&self, name: &str, config: Object) -> Endpoint {
        let bridge = Arc::new(Bridge::new(name, config, self.ws.root.clone()));
        bridge.set_notify(self.notifier(name));
        Endpoint::Bridge(bridge)
    }

    /// Sends every bridged server notification to claude.
    fn notifier(&self, server: &str) -> Notify {
        let (to_claude, counter, server) = (self.to_claude.clone(), self.own_requests.clone(), server.to_owned());
        Arc::new(move |message| {
            let request_id = format!("{OWN_REQUEST}{}", counter.fetch_add(1, Ordering::Relaxed) + 1);
            let frame = json!({ "type": "control_request", "request_id": request_id, "request": { "subtype": "mcp_message", "server_name": server, "message": message } });
            let _ = to_claude.send(frame.to_string());
        })
    }

    /// Replaces the remote working directory with the local one in a serialized message.
    pub fn map_paths(&self, json: &str) -> String {
        self.paths.map(json).into_owned()
    }

    fn send_claude(&self, line: String) {
        let _ = self.to_claude.send(line);
    }

    fn send_host(&self, line: String) {
        (self.to_host)(line);
    }

    // ── host → claude ─────────────────────────────────────────────────────────────────────────────

    /// One line from the host. Lines must be handled in order: await each before the next.
    pub async fn from_host(&self, line: String) {
        trace("in", &line);
        if !line.contains("\"control_") {
            if line.contains("\"type\":\"user\"") {
                crate::peers::set_status(crate::peers::Status::Working);
            }
            return self.send_claude(line);
        }
        let Some(mut frame) = parse_object(&line) else { return self.send_claude(line) };
        let kind = str_field(&frame, "type").map(str::to_owned);
        if kind.as_deref() == Some("control_request")
            && let Some(request) = frame.get_mut("request").and_then(Value::as_object_mut)
        {
            match str_field(request, "subtype") {
                Some("initialize") => self.rewrite_initialize(request).await,
                Some("mcp_set_servers") => self.rewrite_set_servers(request).await,
                subtype => {
                    if subtype == Some("set_permission_mode")
                        && let Some(mode) = str_field(request, "mode")
                    {
                        self.policy.set_mode(mode);
                    }
                    return self.send_claude(line);
                }
            }
            return self.send_rewritten(&frame);
        }
        if kind.as_deref() == Some("control_response")
            && let Some(response) = frame.get_mut("response").and_then(Value::as_object_mut)
            && let Some(id) = str_field(response, "request_id")
            && self.permission_requests.lock().unwrap().remove(id)
        {
            crate::peers::set_status(crate::peers::Status::Working);
            self.rewrite_permission_answer(response);
            return self.send_rewritten(&frame);
        }
        self.send_claude(line)
    }

    fn send_rewritten(&self, frame: &Object) {
        let line = serde_json::to_string(frame).unwrap_or_default();
        trace("in-rewritten", &line);
        self.send_claude(line);
    }

    async fn rewrite_initialize(&self, request: &mut Object) {
        let mut servers: Vec<String> = request.get("sdkMcpServers").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
        let own = self.local.as_ref().map(|_| SERVER.to_owned());
        for name in own.into_iter().chain(self.config_server_configs.iter().map(|(n, _)| n.clone())) {
            if !servers.contains(&name) {
                servers.push(name);
            }
        }
        request.insert("sdkMcpServers".into(), json!(servers));
        if let Some(local) = &self.local {
            object_entry(request, "sdkMcpServerConfigs").insert(SERVER.into(), json!({ "timeout": TOOL_TIMEOUT }));
            // The local server's handshake is known here; sending it saves claude two round trips over ssh.
            object_entry(request, "sdkMcpServerManifests").insert(SERVER.into(), local.manifest());
        }
        // A built-in name the model emits anyway (a skill told it to use `Read`) reaches the local tool.
        let mut aliases: Object = self.local_tools.iter().map(|t| ((*t).to_owned(), json!(format!("{PREFIX}{t}")))).collect();
        if let Some(Value::Object(theirs)) = request.get("toolAliases") {
            aliases.extend(theirs.clone());
        }
        request.insert("toolAliases".into(), Value::Object(aliases));
        request.insert("excludeDynamicSections".into(), json!(true));
        let append = str_field(request, "appendSystemPrompt").unwrap_or("").to_owned();
        let env = self.env_prompt.clone().await;
        let joined: Vec<&str> = [append.as_str(), env.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
        request.insert("appendSystemPrompt".into(), json!(joined.join("\n\n")));
    }

    async fn rewrite_set_servers(&self, request: &mut Object) {
        let servers = request.get("servers").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut next = Object::new();
        let mut live: HashSet<String> = self
            .local
            .iter()
            .map(|_| SERVER.to_owned())
            .chain(self.config_server_configs.iter().chain(&self.remote_servers).map(|(n, _)| n.clone()))
            .collect();
        let mut stale = vec![];
        for (name, config) in servers {
            let Value::Object(config) = config else { continue };
            if live.contains(&name) {
                warn(&format!("ignoring the host's MCP server \"{name}\": the name is taken by claudette"));
                continue;
            }
            if str_field(&config, "type") == Some("sdk") {
                next.insert(name, Value::Object(config));
                continue;
            }
            let key = Value::Object(config.clone()).to_string();
            let existing = self.endpoints.lock().unwrap().get(&name).cloned();
            let unchanged = matches!(&existing, Some(Endpoint::Bridge(b)) if b.key == key);
            if !unchanged {
                if let Some(old) = existing {
                    stale.push(old);
                }
                let bridge = self.bridge(&name, config.clone());
                self.endpoints.lock().unwrap().insert(name.clone(), bridge);
            }
            live.insert(name.clone());
            next.insert(name.clone(), sdk_entry(&name, &config));
        }
        {
            let mut endpoints = self.endpoints.lock().unwrap();
            let gone: Vec<String> = endpoints.keys().filter(|n| !live.contains(*n)).cloned().collect();
            for name in gone {
                stale.extend(endpoints.remove(&name));
            }
        }
        for endpoint in stale {
            endpoint.close().await;
        }
        // The set replaces every dynamic server, so the relay's own ride along each time.
        if self.local.is_some() {
            next.insert(SERVER.into(), json!({ "type": "sdk", "name": SERVER, "timeout": TOOL_TIMEOUT }));
        }
        for (name, config) in &self.config_server_configs {
            next.insert(name.clone(), sdk_entry(name, config));
        }
        for (name, config) in &self.remote_servers {
            next.insert(name.clone(), Value::Object(config.clone()));
        }
        request.insert("servers".into(), Value::Object(next));
    }

    fn rewrite_permission_answer(&self, response: &mut Object) {
        let Some(Value::Object(result)) = response.get_mut("response") else { return };
        let Some(Value::Array(updates)) = result.get_mut("updatedPermissions") else { return };
        let mut kept = vec![];
        for update in updates.drain(..) {
            let Value::Object(mut update) = update else { continue };
            if str_field(&update, "type") == Some("setMode")
                && let Some(mode) = str_field(&update, "mode")
            {
                self.policy.set_mode(mode);
            }
            let allows = str_field(&update, "behavior") == Some("allow");
            // A rule saved to a settings file stays in the update: claude writes it to the project's
            // settings (on this machine, through the workspace) or the user's (on this machine, through
            // the session's Claude Code folder), and the next session's policy reads it.
            let saved = str_field(&update, "destination").is_some_and(|d| d != "session" && d != "cliArg");
            if let Some(Value::Array(rules)) = update.get_mut("rules") {
                rules.retain_mut(|rule| {
                    let Some(rule) = rule.as_object_mut() else { return true };
                    let Some(tool) = str_field(rule, "toolName").map(str::to_owned) else { return true };
                    if tool == "Bash"
                        && allows
                        && let Some(content) = str_field(rule, "ruleContent")
                    {
                        self.policy.approve(content);
                        return saved;
                    }
                    if is_local_tool(&tool) {
                        // A bare allow ("always allow Write") covers the tool's paths no ask rule covers.
                        if allows && str_field(rule, "ruleContent").is_none() {
                            self.policy.allow_tool(&tool);
                        }
                        rule.insert("toolName".into(), json!(format!("{PREFIX}{tool}")));
                    }
                    true
                });
                if rules.is_empty() {
                    continue;
                }
            }
            kept.push(Value::Object(update));
        }
        *updates = kept;
    }

    // ── claude → host ─────────────────────────────────────────────────────────────────────────────

    /// One line from the remote claude. MCP calls run concurrently; the returned task, if any,
    /// finishes when the call is answered.
    pub fn from_claude(self: &Arc<Self>, line: &str) -> Option<tokio::task::JoinHandle<()>> {
        trace("out", line);
        if (line.contains("\"control_") || line.contains("\"transcript_mirror\"") || line.contains("\"init\""))
            && let Some(frame) = parse_object(line)
        {
            match self.intercept(frame) {
                Intercepted::Handled => return None,
                Intercepted::Task(task) => return Some(task),
                Intercepted::Pass => {}
            }
        }
        if line.contains("\"type\":\"result\"") {
            crate::peers::set_status(crate::peers::Status::Idle);
        }
        self.send_host(self.map_paths(&unprefix(line)));
        None
    }

    fn intercept(self: &Arc<Self>, mut frame: Object) -> Intercepted {
        match str_field(&frame, "type") {
            Some("control_request") => {
                let Some(id) = str_field(&frame, "request_id").map(str::to_owned) else { return Intercepted::Pass };
                let Some(request) = frame.get("request").and_then(Value::as_object) else { return Intercepted::Pass };
                match str_field(request, "subtype") {
                    Some("mcp_message") => {
                        let Some(server) = str_field(request, "server_name").map(str::to_owned) else { return Intercepted::Pass };
                        let Some(endpoint) = self.endpoints.lock().unwrap().get(&server).cloned() else { return Intercepted::Pass };
                        let message = request.get("message").cloned();
                        let relay = self.clone();
                        Intercepted::Task(tokio::spawn(async move { relay.serve_mcp(id, server, endpoint, message).await }))
                    }
                    Some("can_use_tool") => {
                        self.ask_permission(id, frame);
                        Intercepted::Handled
                    }
                    _ => Intercepted::Pass,
                }
            }
            Some("control_cancel_request") => {
                let Some(id) = str_field(&frame, "request_id") else { return Intercepted::Pass };
                if let Some(call) = self.calls.lock().unwrap().get(id) {
                    self.withdrawn.lock().unwrap().insert(id.to_owned());
                    call.cancel();
                    return Intercepted::Handled;
                }
                self.permission_requests.lock().unwrap().remove(id);
                Intercepted::Pass
            }
            Some("control_response") => {
                let own = frame.get("response").and_then(Value::as_object).and_then(|r| str_field(r, "request_id")).is_some_and(|id| id.starts_with(OWN_REQUEST));
                if own { Intercepted::Handled } else { Intercepted::Pass }
            }
            Some("transcript_mirror") => {
                // Only a host with its own session store asks for these; claude's own transcripts
                // reach this machine through the mounted sessions folder.
                if !self.host_mirror {
                    return Intercepted::Handled;
                }
                // The host keeps its own store: give it the frame with the path its store expects.
                if let Some(local) = str_field(&frame, "filePath").and_then(|f| self.mirror.local_path(f)) {
                    frame.insert("filePath".into(), json!(local.to_string_lossy()));
                }
                self.send_host(self.map_paths(&serde_json::to_string(&frame).unwrap_or_default()));
                Intercepted::Handled
            }
            Some("system") => {
                if str_field(&frame, "subtype") == Some("init")
                    && let Some(mode) = str_field(&frame, "permissionMode")
                {
                    self.policy.set_mode(mode);
                }
                Intercepted::Pass
            }
            _ => Intercepted::Pass,
        }
    }

    async fn serve_mcp(&self, request_id: String, server: String, endpoint: Endpoint, message: Option<Value>) {
        let Some(Value::Object(message)) = message else { return self.answer_mcp(&request_id, None) };
        let cancel = CancellationToken::new();
        self.calls.lock().unwrap().insert(request_id.clone(), cancel.clone());
        let rpc = rpc_id(&message);
        let rpc_key = rpc.as_ref().map(|id| format!("{server}:{}", id_key(id)));
        if let Some(key) = &rpc_key {
            self.rpc_calls.lock().unwrap().insert(key.clone(), cancel.clone());
        }
        if str_field(&message, "method") == Some("notifications/cancelled")
            && let Some(target) = message.get("params").and_then(|p| p.get("requestId"))
            && let Some(call) = self.rpc_calls.lock().unwrap().get(&format!("{server}:{}", id_key(target)))
        {
            call.cancel();
        }
        let handled = tokio::spawn({
            let (endpoint, cancel) = (endpoint.clone(), cancel.clone());
            async move { endpoint.handle(message, cancel).await }
        })
        .await;
        let response = match handled {
            Ok(response) => response,
            Err(e) => rpc.map(|id| rpc_error(id, INTERNAL_ERROR, &e.to_string())),
        };
        self.calls.lock().unwrap().remove(&request_id);
        if let Some(key) = &rpc_key {
            self.rpc_calls.lock().unwrap().remove(key);
        }
        if self.withdrawn.lock().unwrap().remove(&request_id) {
            return;
        }
        self.answer_mcp(&request_id, response);
    }

    fn answer_mcp(&self, request_id: &str, response: Option<Value>) {
        // A notification gets the same empty acknowledgement the SDK gives its in-process servers.
        let mcp_response = response.unwrap_or_else(|| json!({ "jsonrpc": "2.0", "result": {}, "id": 0 }));
        let frame = json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": { "mcp_response": mcp_response } } });
        self.send_claude(frame.to_string());
    }

    fn ask_permission(&self, id: String, mut frame: Object) {
        if let Some(request) = frame.get_mut("request").and_then(Value::as_object_mut) {
            let raw = str_field(request, "tool_name").unwrap_or("");
            let name = raw.strip_prefix(PREFIX).filter(|n| is_local_tool(n)).map(str::to_owned);
            if let Some(name) = name {
                let input = request.get("input").and_then(Value::as_object).cloned().unwrap_or_default();
                match self.policy.decide(&self.ws, &name, &input, None) {
                    Some(Decision::Allow) => return self.answer_permission(&id, json!({ "behavior": "allow", "updatedInput": input })),
                    Some(Decision::Deny(message)) => return self.answer_permission(&id, json!({ "behavior": "deny", "message": message })),
                    Some(Decision::Ask) | None => {}
                }
                let mut suggestions = vec![];
                if name == "Bash" {
                    // claude would offer to approve the whole MCP tool; offer the command instead.
                    if let Some(prefix) = command_prefix(str_field(&input, "command").unwrap_or("")) {
                        suggestions.push(json!({ "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": format!("{prefix}:*") }], "behavior": "allow", "destination": "session" }));
                    }
                } else if EDIT_TOOLS.contains(&name.as_str()) {
                    let target = str_field(&input, "file_path").or_else(|| str_field(&input, "notebook_path"));
                    if target.is_some_and(|t| self.ws.owns(t)) {
                        suggestions.push(json!({ "type": "setMode", "mode": "acceptEdits", "destination": "session" }));
                    }
                }
                request.insert("tool_name".into(), json!(name));
                request.remove("mcp_server");
                if suggestions.is_empty() {
                    request.remove("permission_suggestions");
                } else {
                    request.insert("permission_suggestions".into(), json!(suggestions));
                }
            }
        }
        self.permission_requests.lock().unwrap().insert(id);
        crate::peers::set_status(crate::peers::Status::Waiting);
        self.send_host(self.map_paths(&unprefix(&serde_json::to_string(&frame).unwrap_or_default())));
    }

    fn answer_permission(&self, request_id: &str, response: Value) {
        self.send_claude(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": response } }).to_string());
    }

    pub async fn close(&self) {
        for call in self.calls.lock().unwrap().values() {
            call.cancel();
        }
        let endpoints: Vec<Endpoint> = self.endpoints.lock().unwrap().drain().map(|(_, e)| e).collect();
        for endpoint in endpoints {
            endpoint.close().await;
        }
    }
}

enum Intercepted {
    /// The frame stops here.
    Handled,
    /// An MCP call now running.
    Task(tokio::task::JoinHandle<()>),
    /// The frame goes on to the host unchanged.
    Pass,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::parse_args;
    use crate::mount::workdir::remote_workdir;
    use futures_util::FutureExt;
    use tokio::sync::mpsc::UnboundedReceiver;

    const SDK_BASE: [&str; 5] = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json"];

    struct Setup {
        dir: std::path::PathBuf,
        relay: Arc<Relay>,
        claude: UnboundedReceiver<String>,
        host: Arc<Mutex<Vec<String>>>,
    }

    impl Setup {
        fn new(extra: &[&str]) -> Setup {
            let dir = std::env::temp_dir().join(format!("ct-relay-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
            std::fs::create_dir_all(&dir).unwrap();
            let dir = dir.canonicalize().unwrap();
            let argv: Vec<String> = SDK_BASE.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            let args = parse_args(&argv, &dir).unwrap();
            let (to_claude, claude) = tokio::sync::mpsc::unbounded_channel();
            let host: Arc<Mutex<Vec<String>>> = Arc::default();
            let sink = host.clone();
            let ws = Arc::new(Workspace::new(dir.clone(), vec![], Default::default()));
            let prompt = futures_util::future::ready("LOCAL-ENV".to_owned()).boxed().shared();
            let relay = Relay::new(ws, &args, prompt, to_claude, Arc::new(move |l| sink.lock().unwrap().push(l)));
            Setup { dir, relay, claude, host }
        }

        async fn from_host(&self, frame: Value) {
            self.relay.from_host(frame.to_string()).await;
        }

        async fn from_claude(&self, frame: Value) {
            if let Some(task) = self.relay.from_claude(&frame.to_string()) {
                task.await.unwrap();
            }
        }

        fn to_claude(&mut self) -> Vec<Value> {
            std::iter::from_fn(|| self.claude.try_recv().ok()).map(|l| serde_json::from_str(&l).unwrap()).collect()
        }

        fn to_host(&self) -> Vec<Value> {
            self.host.lock().unwrap().iter().map(|l| serde_json::from_str(l).unwrap()).collect()
        }
    }

    fn can_use_tool(id: &str, tool: &str, input: Value) -> Value {
        json!({ "type": "control_request", "request_id": id, "request": { "subtype": "can_use_tool", "tool_name": tool, "input": input, "mcp_server": { "name": "cc" }, "permission_suggestions": [{ "type": "addRules", "rules": [{ "toolName": tool }] }] } })
    }

    fn answer(id: &str, response: Value) -> Value {
        json!({ "type": "control_response", "response": { "subtype": "success", "request_id": id, "response": response } })
    }

    #[tokio::test]
    async fn initialize_registers_the_local_tools_with_their_handshake_aliases_and_the_local_environment() {
        let mut s = Setup::new(&[]);
        s.from_host(json!({ "type": "control_request", "request_id": "i1", "request": { "subtype": "initialize", "sdkMcpServers": ["host"], "appendSystemPrompt": "HOST", "toolAliases": { "Bash": "mcp__host__sh" } } })).await;
        let sent = s.to_claude();
        let req = &sent[0]["request"];
        assert_eq!(req["sdkMcpServers"], json!(["host", "cc"]));
        let tools: Vec<&str> = req["sdkMcpServerManifests"]["cc"]["toolsListResult"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(tools, ["Bash", "Glob", "Grep", "Read", "Edit", "Write", "NotebookEdit", "ListAgents", "SendMessage"]);
        assert_eq!(req["toolAliases"]["Read"], "mcp__cc__Read");
        assert_eq!(req["toolAliases"]["Bash"], "mcp__host__sh");
        assert_eq!(req["excludeDynamicSections"], true);
        assert_eq!(req["appendSystemPrompt"], "HOST\n\nLOCAL-ENV");
    }

    #[tokio::test]
    async fn set_servers_keeps_host_sdk_servers_bridges_the_rest_without_credentials_and_keeps_the_local_tools() {
        let mut s = Setup::new(&["--mcp-config", r#"{"mcpServers":{"cfg":{"type":"stdio","command":"x"}}}"#]);
        let servers = json!({
            "hostsdk": { "type": "sdk", "name": "hostsdk" },
            "t3": { "type": "http", "url": "http://127.0.0.1:9/mcp", "headers": { "Authorization": "Bearer secret" }, "timeout": 5000 },
            "cc": { "type": "stdio", "command": "imposter" }
        });
        s.from_host(json!({ "type": "control_request", "request_id": "m1", "request": { "subtype": "mcp_set_servers", "servers": servers } })).await;
        assert_eq!(
            s.to_claude()[0]["request"]["servers"],
            json!({
                "hostsdk": { "type": "sdk", "name": "hostsdk" },
                "t3": { "type": "sdk", "name": "t3", "timeout": 5000 },
                "cc": { "type": "sdk", "name": "cc", "timeout": 660000 },
                "cfg": { "type": "sdk", "name": "cfg" }
            })
        );
    }

    #[tokio::test]
    async fn local_tool_calls_are_answered_here_and_withdrawn_ones_get_no_answer() {
        let mut s = Setup::new(&[]);
        let call = |id: &str, rpc: u64, name: &str, args: Value| {
            json!({ "type": "control_request", "request_id": id, "request": { "subtype": "mcp_message", "server_name": "cc", "message": { "jsonrpc": "2.0", "id": rpc, "method": "tools/call", "params": { "name": name, "arguments": args } } } })
        };
        s.from_claude(call("c1", 7, "Write", json!({ "file_path": "made.txt", "content": "hi" }))).await;
        assert_eq!(std::fs::read_to_string(s.dir.join("made.txt")).unwrap(), "hi");
        let text = format!("File created successfully at: {}", s.dir.join("made.txt").display());
        assert_eq!(s.to_claude(), [json!({ "type": "control_response", "response": { "subtype": "success", "request_id": "c1", "response": { "mcp_response": { "jsonrpc": "2.0", "id": 7, "result": { "content": [{ "type": "text", "text": text }] } } } } })]);

        let task = s.relay.from_claude(&call("c2", 8, "Bash", json!({ "command": "sleep 20" })).to_string()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        s.from_claude(json!({ "type": "control_cancel_request", "request_id": "c2" })).await;
        task.await.unwrap();
        assert_eq!(s.to_claude(), Vec::<Value>::new());
        assert_eq!(s.to_host(), Vec::<Value>::new());
    }

    #[tokio::test]
    async fn names_lose_the_local_prefix_and_the_remote_folder_maps_on_path_boundaries_only() {
        let s = Setup::new(&[]);
        let remote = format!("/home/remote/{}", remote_workdir(&s.dir));
        s.from_claude(json!({ "type": "assistant", "tool": "mcp__cc__Read", "other": "mcp__cc__Unknown", "here": format!("{remote}/a.ts"), "sibling": format!("{remote}-other/b.ts") })).await;
        let local = format!("{}/a.ts", s.dir.display());
        assert_eq!(s.to_host()[0], json!({ "type": "assistant", "tool": "Read", "other": "mcp__cc__Unknown", "here": local, "sibling": format!("{remote}-other/b.ts") }));
    }

    #[tokio::test]
    async fn accept_edits_approves_local_edits_here_and_asks_the_host_for_the_rest_under_built_in_names() {
        let mut s = Setup::new(&["--permission-mode=acceptEdits"]);
        let inside = s.dir.join("x.ts").display().to_string();
        s.from_claude(can_use_tool("p1", "mcp__cc__Edit", json!({ "file_path": inside }))).await;
        s.from_claude(can_use_tool("p2", "mcp__cc__Write", json!({ "file_path": "/etc/elsewhere" }))).await;
        assert_eq!(s.to_claude(), [answer("p1", json!({ "behavior": "allow", "updatedInput": { "file_path": inside } }))]);
        let asked = s.to_host();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0]["request"], json!({ "subtype": "can_use_tool", "tool_name": "Write", "input": { "file_path": "/etc/elsewhere" } }));
    }

    #[tokio::test]
    async fn an_approved_bash_prefix_stays_here_unless_saved_and_never_covers_compound_commands() {
        let mut s = Setup::new(&[]);
        s.from_claude(can_use_tool("b1", "mcp__cc__Bash", json!({ "command": "npm test" }))).await;
        assert_eq!(
            s.to_host()[0]["request"]["permission_suggestions"],
            json!([{ "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": "npm test:*" }], "behavior": "allow", "destination": "session" }])
        );
        let updates = json!([
            { "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": "npm test:*" }], "behavior": "allow", "destination": "session" },
            { "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": "make:*" }], "behavior": "allow", "destination": "localSettings" },
            { "type": "addRules", "rules": [{ "toolName": "Edit" }, { "toolName": "WebFetch" }], "behavior": "allow", "destination": "session" }
        ]);
        s.from_host(answer("b1", json!({ "behavior": "allow", "updatedPermissions": updates }))).await;
        assert_eq!(
            s.to_claude().last().unwrap()["response"]["response"]["updatedPermissions"],
            json!([
                { "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": "make:*" }], "behavior": "allow", "destination": "localSettings" },
                { "type": "addRules", "rules": [{ "toolName": "mcp__cc__Edit" }, { "toolName": "WebFetch" }], "behavior": "allow", "destination": "session" }
            ])
        );
        for (id, command) in [("b2", "npm test -- --watch"), ("b3", "npm test && rm -rf build"), ("b4", "npm testing"), ("b5", "make all")] {
            s.from_claude(can_use_tool(id, "mcp__cc__Bash", json!({ "command": command }))).await;
        }
        let answered: Vec<Value> = s.to_claude().iter().map(|f| f["response"]["request_id"].clone()).collect();
        assert_eq!(answered, [json!("b2"), json!("b5")]);
        let asked: Vec<Value> = s.to_host().iter().map(|f| f["request_id"].clone()).collect();
        assert_eq!(asked, [json!("b1"), json!("b3"), json!("b4")]);
    }

    #[tokio::test]
    async fn a_command_the_hosts_rules_deny_is_refused_here() {
        let mut s = Setup::new(&["--disallowedTools=Bash(rm:*)", "--allowedTools=Bash(git status:*)"]);
        s.from_claude(can_use_tool("d1", "mcp__cc__Bash", json!({ "command": "git status && rm -rf /" }))).await;
        s.from_claude(can_use_tool("d2", "mcp__cc__Bash", json!({ "command": "git status --short" }))).await;
        let behaviors: Vec<Value> = s.to_claude().iter().map(|f| f["response"]["response"]["behavior"].clone()).collect();
        assert_eq!(behaviors, [json!("deny"), json!("allow")]);
        assert_eq!(s.to_host(), Vec::<Value>::new());
    }
}
