use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

pub const SERVER: &str = "cc";
pub const PREFIX: &str = "mcp__cc__";

pub const LOCAL_TOOLS: [&str; 9] = ["Read", "Write", "Edit", "Glob", "Grep", "Bash", "NotebookEdit", "ListAgents", "SendMessage"];

pub const READ_TOOLS: [&str; 3] = ["Read", "Glob", "Grep"];
pub const UNASKED_TOOLS: [&str; 5] = ["Read", "Glob", "Grep", "ListAgents", "SendMessage"];

pub const EDIT_TOOLS: [&str; 3] = ["Write", "Edit", "NotebookEdit"];

pub const REMOTE_TOOLS: [&str; 25] = [
    "Task", "Agent", "Skill", "WebFetch", "WebSearch", "ToolSearch", "TodoWrite", "TaskCreate", "TaskGet", "TaskList", "TaskUpdate", "TaskOutput",
    "TaskStop", "ExitPlanMode", "EnterPlanMode", "AskUserQuestion", "Workflow", "ReportFindings", "ScheduleWakeup", "CronCreate", "CronDelete",
    "CronList", "ListMcpResourcesTool", "ReadMcpResourceTool", "WaitForMcpServers",
];

pub fn is_local_tool(name: &str) -> bool {
    LOCAL_TOOLS.contains(&name)
}

static PREFIXED: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r#""{PREFIX}({})""#, LOCAL_TOOLS.join("|"))).expect("valid pattern"));

pub fn unprefix(json: &str) -> Cow<'_, str> {
    if !json.contains(PREFIX) {
        return Cow::Borrowed(json);
    }
    PREFIXED.replace_all(json, "\"$1\"")
}
