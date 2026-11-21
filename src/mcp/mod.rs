pub mod bridge;
pub mod cli;
pub mod config;
mod desktop;
pub mod local;
mod oauth;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::json::Object;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Caller,
    User,
    Local,
    Project(PathBuf),
    Plugin(PathBuf),
}

#[derive(Clone, Debug)]
pub struct LocalServer {
    pub config: Object,
    pub source: Source,
}

pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INTERNAL_ERROR: i64 = -32603;

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [LATEST_PROTOCOL_VERSION, "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];

pub fn rpc_id(message: &Object) -> Option<Value> {
    message.get("id").filter(|id| id.is_string() || id.is_number()).cloned()
}

pub fn id_key(id: &Value) -> String {
    id.as_str().map_or_else(|| id.to_string(), str::to_owned)
}

pub fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

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
