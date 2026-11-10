//! Names of the local tools and the MCP server that carries them.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

/// The MCP server that carries the local tools. claude names its tools `mcp__<server>__<tool>`.
pub const SERVER: &str = "cc";
pub const PREFIX: &str = "mcp__cc__";

/// Built-in tools replaced by local versions with the same name and input: the file and shell tools,
/// and the tools that reach other sessions, which reach this machine's sessions instead.
pub const LOCAL_TOOLS: [&str; 9] = ["Read", "Write", "Edit", "Glob", "Grep", "Bash", "NotebookEdit", "ListAgents", "SendMessage"];

/// Local tools Claude Code never asks before using: the ones that only read, and the messaging tools.
pub const READ_TOOLS: [&str; 3] = ["Read", "Glob", "Grep"];
pub const UNASKED_TOOLS: [&str; 5] = ["Read", "Glob", "Grep", "ListAgents", "SendMessage"];

/// Local tools that acceptEdits approves inside the working directories.
pub const EDIT_TOOLS: [&str; 3] = ["Write", "Edit", "NotebookEdit"];

/// Built-in tools the remote claude may use: the ones that stay within its session or reach the web.
/// The Cron tools are session-scoped (tasks stop with the session; a task kept across sessions is saved
/// in the project's `.claude/scheduled_tasks.json`, which claude reaches through the workspace).
/// Everything else is off: the file and shell tools (local versions replace them), PowerShell, LSP
/// and Monitor (they need this machine), the worktree tools (they move claude's own directory), and
/// ListAgents and SendMessage (local versions replace them: on the remote machine they would reach every
/// session there). Built-ins a newer Claude Code adds stay off until they are listed here; claude ignores
/// names it does not have.
pub const REMOTE_TOOLS: [&str; 25] = [
    "Task", "Agent", "Skill", "WebFetch", "WebSearch", "ToolSearch", "TodoWrite", "TaskCreate", "TaskGet", "TaskList", "TaskUpdate", "TaskOutput",
    "TaskStop", "ExitPlanMode", "EnterPlanMode", "AskUserQuestion", "Workflow", "ReportFindings", "ScheduleWakeup", "CronCreate", "CronDelete",
    "CronList", "ListMcpResourcesTool", "ReadMcpResourceTool", "WaitForMcpServers",
];

pub fn is_local_tool(name: &str) -> bool {
    LOCAL_TOOLS.contains(&name)
}

static PREFIXED: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r#""{PREFIX}({})""#, LOCAL_TOOLS.join("|"))).expect("valid pattern"));

/// Gives the local tools their built-in names in a serialized message, so the host renders them as built-ins.
pub fn unprefix(json: &str) -> Cow<'_, str> {
    if !json.contains(PREFIX) {
        return Cow::Borrowed(json);
    }
    PREFIXED.replace_all(json, "\"$1\"")
}
