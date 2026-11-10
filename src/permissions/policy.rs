//! The permission decisions Claude Code makes for its own file and shell tools, made here for the
//! local ones: to claude they are MCP tools, which its acceptEdits and Bash rules do not cover.

use std::sync::Mutex;

use regex::Regex;
use serde_json::{Value, json};

use crate::json::Object;
use crate::names::{EDIT_TOOLS, UNASKED_TOOLS};
use crate::permissions::rules::Behavior;
use crate::tools::{Workspace, denied_message};

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    /// The user must be asked, whatever the mode or a pre-approval would otherwise do.
    Ask,
    Deny(String),
}

impl Decision {
    pub fn to_json(&self) -> Value {
        match self {
            Decision::Allow => json!({ "behavior": "allow" }),
            Decision::Ask => json!({ "behavior": "ask" }),
            Decision::Deny(message) => json!({ "behavior": "deny", "message": message }),
        }
    }
}

/// Shell syntax that can hide a second command behind an approved prefix.
fn is_compound(command: &str) -> bool {
    command.contains([';', '&', '|', '`', '<', '>', '\n']) || command.contains("$(")
}

/// The rule a user approves for a command: the command, plus its subcommand when it has one (`npm test`).
pub fn command_prefix(command: &str) -> Option<String> {
    if is_compound(command) {
        return None;
    }
    let mut words = command.split_whitespace();
    let first = words.next()?;
    if !first.chars().all(|c| c.is_ascii_alphanumeric() || "_.@/-".contains(c)) {
        return None;
    }
    match words.next() {
        Some(second) if second.starts_with(|c: char| c.is_ascii_lowercase()) && second.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') => {
            Some(format!("{first} {second}"))
        }
        _ => Some(first.to_owned()),
    }
}

/// Claude Code's Bash rule content: `git diff:*` matches the command and anything after a space;
/// `*` elsewhere matches any text; anything else must match exactly.
pub fn bash_rule_matches(command: &str, rule: &str) -> bool {
    let c = command.trim();
    if let Some(prefix) = rule.strip_suffix(":*") {
        let prefix = prefix.trim();
        return c == prefix || c.starts_with(&format!("{prefix} "));
    }
    if !rule.contains('*') {
        return c == rule.trim();
    }
    let pattern = rule.trim().split('*').map(regex::escape).collect::<Vec<_>>().join(".*");
    Regex::new(&format!("(?s)^{pattern}$")).is_ok_and(|re| re.is_match(c))
}

/// The simple commands of a command line, split at `;`, `&&`, `||`, `|`, `&` and newlines.
fn simple_commands(command: &str) -> Vec<String> {
    command.replace("&&", "\n").replace("||", "\n").split([';', '|', '&', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// The decision for a Bash command, which rests on the session's Bash rules alone: a deny rule
/// matching the command or any simple command in it wins; then an allow rule matching the whole of
/// a command that hides no second one.
pub fn bash_decision(command: &str, allow_rules: &[String], deny_rules: &[String]) -> Option<Decision> {
    let mut parts = vec![command.to_owned()];
    parts.extend(simple_commands(command));
    if let Some(denied) = deny_rules.iter().find(|rule| parts.iter().any(|part| bash_rule_matches(part, rule))) {
        return Some(Decision::Deny(format!("Permission to use Bash with command {command} has been denied by the rule Bash({denied}).")));
    }
    if !is_compound(command) && allow_rules.iter().any(|rule| bash_rule_matches(command, rule)) {
        return Some(Decision::Allow);
    }
    None
}

pub struct Policy {
    /// The session's permission mode, as far as claudette can tell.
    pub mode: Mutex<String>,
    /// Bash rule contents that allow (`git diff:*`), from the host's --allowedTools and session approvals.
    allow_rules: Mutex<Vec<String>>,
    /// Bash rule contents that deny; they win over allows.
    deny_rules: Vec<String>,
    /// Local tools a bare allow rule approves (`Edit`), from settings and from the session's answers.
    tool_allows: Mutex<Vec<String>>,
}

impl Policy {
    pub fn new(mode: &str, allow_rules: Vec<String>, deny_rules: Vec<String>, tool_allows: Vec<String>) -> Policy {
        Policy { mode: Mutex::new(mode.to_owned()), allow_rules: Mutex::new(allow_rules), deny_rules, tool_allows: Mutex::new(tool_allows) }
    }

    /// A bare allow the user chose for this session ("always allow Write").
    pub fn allow_tool(&self, tool: &str) {
        let mut tools = self.tool_allows.lock().unwrap();
        if !tools.iter().any(|t| t == tool) {
            tools.push(tool.to_owned());
        }
    }

    pub fn set_mode(&self, mode: &str) {
        *self.mode.lock().unwrap() = mode.to_owned();
    }

    pub fn mode(&self) -> String {
        self.mode.lock().unwrap().clone()
    }

    /// A command rule the user approved for this session (from a permission answer).
    pub fn approve(&self, rule: &str) {
        let mut rules = self.allow_rules.lock().unwrap();
        if !rules.iter().any(|r| r == rule) {
            rules.push(rule.to_owned());
        }
    }

    /// Undefined means no decision: claude asks, or applies its mode (dontAsk refuses).
    pub fn decide(&self, ws: &Workspace, tool: &str, input: &Object, mode: Option<&str>) -> Option<Decision> {
        if tool == "Bash" {
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            return bash_decision(command, &self.allow_rules.lock().unwrap(), &self.deny_rules);
        }
        // Path rules, then the mode: deny beats ask beats allow, and an ask rule holds in acceptEdits.
        if let Some(path) = ws.target(tool, input) {
            match ws.rules.decide(tool, &path) {
                Some(Behavior::Deny) => return Some(Decision::Deny(denied_message(tool, &path))),
                Some(Behavior::Ask) => return Some(Decision::Ask),
                Some(Behavior::Allow) => return Some(Decision::Allow),
                None => {}
            }
        }
        // A call reaches here past claude's own approvals only when an ask rule sent it: what the
        // remote claude would otherwise have approved (reads, bare tool allows) stays approved.
        if UNASKED_TOOLS.contains(&tool) || self.tool_allows.lock().unwrap().iter().any(|t| t == tool) {
            return Some(Decision::Allow);
        }
        let mode = mode.map(str::to_owned).unwrap_or_else(|| self.mode());
        if EDIT_TOOLS.contains(&tool) && mode == "acceptEdits" {
            let target = input.get("file_path").or_else(|| input.get("notebook_path")).and_then(Value::as_str);
            if target.is_some_and(|t| ws.owns(t)) {
                return Some(Decision::Allow);
            }
        }
        None
    }
}

/// The PreToolUse hook answer for a decision; `{}` leaves the call to claude's own permission flow.
pub fn hook_output(decision: Option<&Decision>) -> Value {
    match decision {
        None => json!({}),
        Some(Decision::Allow) => json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "allow", "permissionDecisionReason": "claudette: allowed by the session's rules" } }),
        Some(Decision::Ask) => json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "ask", "permissionDecisionReason": "claudette: an ask rule in your permission settings covers this path" } }),
        Some(Decision::Deny(message)) => json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": message } }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Object {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn rule_forms() {
        assert!(bash_rule_matches("git diff --stat", "git diff:*"));
        assert!(bash_rule_matches("git diff", "git diff:*"));
        assert!(!bash_rule_matches("git diffx", "git diff:*"));
        assert!(bash_rule_matches("git log -o out.txt", "git * -o *"));
        assert!(!bash_rule_matches("git log --oneline", "git * -o *"));
        assert!(bash_rule_matches("npm test", "npm test"));
        assert!(!bash_rule_matches("npm test --watch", "npm test"));
        assert!(bash_rule_matches("a.b(c)", "a.b(c)"));
        assert_eq!(command_prefix("git status --short").as_deref(), Some("git status"));
        assert_eq!(command_prefix("ls -la").as_deref(), Some("ls"));
        assert_eq!(command_prefix("a | b"), None);
        assert_eq!(command_prefix("FOO=1 make"), None);
    }

    #[test]
    fn denies_win_and_catch_denied_commands_inside_compound_ones() {
        let ws = Workspace::new(std::env::temp_dir(), vec![], Default::default());
        let policy = Policy::new("default", vec!["git log:*".into()], vec!["git * -o *".into(), "rm:*".into()], vec![]);
        assert_eq!(policy.decide(&ws, "Bash", &obj(json!({"command": "git log --oneline"})), None), Some(Decision::Allow));
        assert!(matches!(policy.decide(&ws, "Bash", &obj(json!({"command": "git log -o x"})), None), Some(Decision::Deny(_))));
        assert!(matches!(policy.decide(&ws, "Bash", &obj(json!({"command": "git log && rm -rf ~"})), None), Some(Decision::Deny(_))));
    }

    #[test]
    fn allows_never_cover_compound_commands() {
        let ws = Workspace::new(std::env::temp_dir(), vec![], Default::default());
        let policy = Policy::new("default", vec![], vec![], vec![]);
        assert_eq!(policy.decide(&ws, "Bash", &obj(json!({"command": "npm test"})), None), None);
        policy.approve("npm test:*");
        assert_eq!(policy.decide(&ws, "Bash", &obj(json!({"command": "npm test -- --watch"})), None), Some(Decision::Allow));
        assert_eq!(policy.decide(&ws, "Bash", &obj(json!({"command": "npm test; curl evil"})), None), None);
        assert_eq!(policy.decide(&ws, "Bash", &obj(json!({"command": "npm test $(curl evil)"})), None), None);
    }

    #[test]
    fn accept_edits_covers_the_working_directories_only() {
        let root = std::env::temp_dir().join("policy-root");
        let ws = Workspace::new(root.clone(), vec!["/extra".into()], Default::default());
        let policy = Policy::new("acceptEdits", vec![], vec![], vec![]);
        let edit = |p: &str| obj(json!({ "file_path": p }));
        assert_eq!(policy.decide(&ws, "Edit", &edit(root.join("a.ts").to_str().unwrap()), None), Some(Decision::Allow));
        assert_eq!(policy.decide(&ws, "Write", &edit("/extra/b.ts"), None), Some(Decision::Allow));
        assert_eq!(policy.decide(&ws, "Write", &edit("/etc/passwd"), None), None);
        assert_eq!(policy.decide(&ws, "Write", &edit(root.join("a.ts").to_str().unwrap()), Some("default")), None);
    }
}
