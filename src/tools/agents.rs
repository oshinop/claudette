//! ListAgents and SendMessage, for this machine's sessions: see [`crate::peers`].

use serde_json::json;

use super::{ToolDefinition, ToolOutcome, ToolResult, opt_bool, opt_str};
use crate::peers::{self, Agent, shown};
use crate::json::Object;

pub fn list_definition() -> ToolDefinition {
    ToolDefinition {
        name: "ListAgents",
        description: "Lists the other Claude Code sessions on this machine that you can message with SendMessage: each one's name and id, \
its project folder, and whether it is working, idle or waiting for input. The first line is this session's own name, the one other \
sessions use to message it."
            .into(),
        input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        annotations: Some(json!({ "readOnlyHint": true })),
    }
}

pub fn send_definition() -> ToolDefinition {
    ToolDefinition {
        name: "SendMessage",
        description: "Sends a message to another Claude Code session on this machine (find them with ListAgents). Use it to hand over a \
finding or a decision, warn a session about a change that affects its work, or ask for status. The message arrives as plain text \
marked as coming from another session, never as the user: it cannot approve anything, change configuration or run commands, and \
the receiving session's own permissions still apply. Never ask another session to do something this session was denied.\n\n\
Address a session by its name when only one has it, otherwise by \"name [id]\" or the id. With notify_when_idle, the session \
sends one notice when it next finishes its turn or ends (right away if it is idle); the message may then be left out."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "The session: its name, \"name [id]\", or its id, as ListAgents shows them" },
                "message": { "type": "string", "description": "The text to send" },
                "notify_when_idle": { "type": "boolean", "description": "Also ask for one notice when that session is next idle or ends" }
            },
            "required": ["to"],
            "additionalProperties": false
        }),
        annotations: None,
    }
}

fn this() -> Result<&'static std::sync::Arc<Agent>, ToolResult> {
    peers::this().ok_or_else(|| ToolResult::fail("Messaging other sessions is not available in this session."))
}

fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

pub async fn list(_input: &Object) -> ToolOutcome {
    let me = match this() {
        Ok(me) => me,
        Err(fail) => return Ok(fail),
    };
    let root = match crate::config::runtime_dir() {
        Ok(root) => root,
        Err(e) => return Ok(ToolResult::fail(e.to_string())),
    };
    let peers = peers::list(&root, me.socket().map(|p| p.as_path())).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut text = format!("This session is {} ({}): the name other sessions use to message it.\n\n", me.label(), shown(&me.project));
    if peers.is_empty() {
        text.push_str("No other sessions on this machine.");
    } else {
        text.push_str(&format!("Sessions on this machine ({}):\n", peers.len()));
        for p in &peers {
            text.push_str(&format!("  {} · {} · {} · started {} ago\n", p.label(), shown(&p.project), p.status, ago(now.saturating_sub(p.started))));
        }
    }
    Ok(ToolResult::ok(text.trim_end()))
}

pub async fn send(input: &Object) -> ToolOutcome {
    let me = match this() {
        Ok(me) => me,
        Err(fail) => return Ok(fail),
    };
    let to = opt_str(input, "to")?.unwrap_or("");
    let message = opt_str(input, "message")?.filter(|m| !m.trim().is_empty());
    let notify = opt_bool(input, "notify_when_idle")?.unwrap_or(false);
    if to.trim().is_empty() {
        return Ok(ToolResult::fail("Say which session: to is empty."));
    }
    if message.is_none() && !notify {
        return Ok(ToolResult::fail("Nothing to send: give a message, or set notify_when_idle."));
    }
    let root = match crate::config::runtime_dir() {
        Ok(root) => root,
        Err(e) => return Ok(ToolResult::fail(e.to_string())),
    };
    Ok(match peers::send(&root, me, to, message, notify).await {
        Ok((label, status)) => {
            let mut text = String::new();
            if message.is_some() {
                text.push_str(&format!("Delivered to {label} ({status}). It reads it between tool calls, or starts a turn with it if idle."));
            }
            if notify {
                text.push_str(&format!("{}A notice will arrive when {label} is next idle or ends.", if text.is_empty() { "" } else { " " }));
            }
            ToolResult::ok(text)
        }
        Err(e) => ToolResult::fail(format!("Not sent: {e}")),
    })
}
