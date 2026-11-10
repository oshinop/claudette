//! MCP servers the relay hosts on this machine. claude reaches them with `mcp_message` control
//! requests (SDK mode) or over forwarded sockets (direct mode). Requests receive one response and
//! notifications are forwarded without a response.

pub mod bridge;
pub mod config;
pub mod local;

use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::json::Object;

pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INTERNAL_ERROR: i64 = -32603;

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [LATEST_PROTOCOL_VERSION, "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];

/// A JSON-RPC id: a string or a number.
pub fn rpc_id(message: &Object) -> Option<Value> {
    message.get("id").filter(|id| id.is_string() || id.is_number()).cloned()
}

/// An id as text, for keying calls (`"7"` and `7` are the same call to a client that sends both).
pub fn id_key(id: &Value) -> String {
    id.as_str().map_or_else(|| id.to_string(), str::to_owned)
}

pub fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The protocol version to answer `initialize` with: the client's when we speak it, else our newest.
pub fn negotiated_version(params: Option<&Value>) -> &'static str {
    let requested = params.and_then(|p| p.get("protocolVersion")).and_then(Value::as_str);
    SUPPORTED_PROTOCOL_VERSIONS.iter().copied().find(|v| Some(*v) == requested).unwrap_or(LATEST_PROTOCOL_VERSION)
}

#[derive(Clone)]
pub enum Endpoint {
    Local(Arc<local::LocalTools>),
    Bridge(Arc<bridge::Bridge>),
}

impl Endpoint {
    pub async fn handle(&self, message: Object, cancel: CancellationToken) -> Option<Value> {
        match self {
            Endpoint::Local(local) => local.handle(&message, &cancel).await,
            Endpoint::Bridge(bridge) => bridge.handle(&message, &cancel).await,
        }
    }

    pub async fn close(&self) {
        if let Endpoint::Bridge(bridge) = self {
            bridge.close().await;
        }
    }
}
