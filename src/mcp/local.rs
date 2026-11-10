//! The local tools as one MCP server. claude calls it over the relay; the tools run here.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{LATEST_PROTOCOL_VERSION, METHOD_NOT_FOUND, negotiated_version, rpc_error, rpc_id, rpc_result};
use crate::json::Object;
use crate::tools::{self, ToolDefinition, ToolResult, Workspace};
use crate::VERSION;
use crate::mount::workdir::PathMap;

pub struct LocalTools {
    ws: Arc<Workspace>,
    tools: Vec<ToolDefinition>,
    /// claude runs in the mounted workspace and may name files by their path there.
    paths: PathMap,
}

impl LocalTools {
    /// `names`: the tools the caller allows, out of the ones claudette replaces.
    pub fn new(ws: Arc<Workspace>, names: &[&str]) -> LocalTools {
        let paths = PathMap::new(&ws.root);
        LocalTools { ws, tools: tools::definitions().into_iter().filter(|t| names.contains(&t.name)).collect(), paths }
    }

    pub fn initialize_result(&self, protocol_version: &str) -> Value {
        json!({ "protocolVersion": protocol_version, "capabilities": { "tools": {} }, "serverInfo": { "name": "claudette", "version": VERSION } })
    }

    pub fn tools_list_result(&self) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|t| {
                let mut entry = json!({ "name": t.name, "description": t.description, "inputSchema": t.input_schema });
                if let Some(annotations) = &t.annotations {
                    entry["annotations"] = annotations.clone();
                }
                // Never deferred behind tool search: these are claude's core tools.
                entry["_meta"] = json!({ "anthropic/alwaysLoad": true });
                entry
            })
            .collect();
        json!({ "tools": tools })
    }

    /// The handshake claude would otherwise fetch over the network, sent once with `initialize`.
    pub fn manifest(&self) -> Value {
        json!({ "initializeResult": self.initialize_result(LATEST_PROTOCOL_VERSION), "toolsListResult": self.tools_list_result() })
    }

    pub async fn handle(&self, message: &Object, cancel: &CancellationToken) -> Option<Value> {
        let id = rpc_id(message)?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        Some(match method {
            "initialize" => rpc_result(id, self.initialize_result(negotiated_version(message.get("params")))),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, self.tools_list_result()),
            "tools/call" => rpc_result(id, self.call(message.get("params"), cancel).await.to_json()),
            other => rpc_error(id, METHOD_NOT_FOUND, &format!("claudette's local tools have no method {other}")),
        })
    }

    async fn call(&self, params: Option<&Value>, cancel: &CancellationToken) -> ToolResult {
        let name = params.and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or("");
        if !self.tools.iter().any(|t| t.name == name) {
            return ToolResult::fail(format!("No such tool: {name}"));
        }
        let input = params.and_then(|p| p.get("arguments")).filter(|a| a.is_object()).map(|a| a.to_string()).unwrap_or_else(|| "{}".into());
        let input = match serde_json::from_str::<Value>(&self.paths.map(&input)) {
            Ok(Value::Object(input)) => input,
            _ => Object::new(),
        };
        tools::run(name, &input, &self.ws, cancel).await
    }
}
