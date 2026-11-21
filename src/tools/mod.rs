//! The local replacements for Claude Code's file and shell tools, and for its tools that reach
//! other sessions.

mod agents;
mod bash;
mod edit;
mod glob;
mod grep;
mod notebook;
mod read;
mod write;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::json::Object;
use crate::permissions::rules::{Behavior, PathRules};

pub use bash::kill_all_commands;

pub enum Content {
    Text(String),
    Image { data: String, mime_type: &'static str },
}

pub struct ToolResult {
    pub content: Vec<Content>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> ToolResult {
        ToolResult { content: vec![Content::Text(text.into())], is_error: false }
    }

    pub fn fail(text: impl Into<String>) -> ToolResult {
        ToolResult { content: vec![Content::Text(text.into())], is_error: true }
    }

    fn with_nested_memory(mut self, ws: &Workspace, path: &Path) -> ToolResult {
        if let Some(memory) = ws.nested_memory(path) {
            self.content.push(Content::Text(memory));
        }
        self
    }

    pub fn to_json(&self) -> Value {
        let content: Vec<Value> = self
            .content
            .iter()
            .map(|c| match c {
                Content::Text(text) => json!({ "type": "text", "text": text }),
                Content::Image { data, mime_type } => json!({ "type": "image", "data": data, "mimeType": mime_type }),
            })
            .collect();
        if self.is_error { json!({ "content": content, "isError": true }) } else { json!({ "content": content }) }
    }
}

/// A tool input the model got wrong; becomes an error result, as Claude Code's own validation does.
pub struct InputError(pub String);

pub type ToolOutcome = Result<ToolResult, InputError>;

pub fn str_arg<'a>(input: &'a Object, key: &str) -> Result<&'a str, InputError> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| InputError(format!("{key} must be a string")))
}

pub fn opt_str<'a>(input: &'a Object, key: &str) -> Result<Option<&'a str>, InputError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        _ => Err(InputError(format!("{key} must be a string"))),
    }
}

pub fn opt_num(input: &Object, key: &str) -> Result<Option<f64>, InputError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().parse().map(Some).map_err(|_| InputError(format!("{key} must be a number"))),
        _ => Err(InputError(format!("{key} must be a number"))),
    }
}

pub fn opt_bool(input: &Object, key: &str) -> Result<Option<bool>, InputError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::String(s)) if s == "true" || s == "false" => Ok(Some(s == "true")),
        _ => Err(InputError(format!("{key} must be a boolean"))),
    }
}

/// Lexical join and normalization, like Node's `path.resolve`.
pub fn resolve(base: &Path, path: &str) -> PathBuf {
    let joined = base.join(path);
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn is_inside(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir)
}

struct State {
    /// The Bash tool's directory; `cd` in one command carries over to the next.
    cwd: PathBuf,
    reads: HashMap<PathBuf, SystemTime>,
    memory_files: HashSet<PathBuf>,
}

/// The state one session's tools share: the shell's working directory, which files were read
/// (and at which modification time), and which memory files reached the model.
pub struct Workspace {
    /// The session's working directory: where the host started claude.
    pub root: PathBuf,
    /// Further local directories the host granted (`--add-dir`, `additionalDirectories`).
    pub extra_dirs: Vec<PathBuf>,
    /// The `Read(…)` and `Edit(…)` rules of the session's settings.
    pub rules: Arc<PathRules>,
    /// The session's hooks, once the workspace is served: CwdChanged and what hooks set for Bash.
    hooks: OnceLock<Arc<crate::settings::Callbacks>>,
    state: Mutex<State>,
}

impl Workspace {
    pub fn new(root: PathBuf, extra_dirs: Vec<PathBuf>, rules: PathRules) -> Workspace {
        Workspace {
            state: Mutex::new(State { cwd: root.clone(), reads: HashMap::new(), memory_files: HashSet::new() }),
            root,
            extra_dirs,
            rules: Arc::new(rules),
            hooks: OnceLock::new(),
        }
    }

    pub fn set_hooks(&self, hooks: Arc<crate::settings::Callbacks>) {
        let _ = self.hooks.set(hooks);
    }

    pub fn hooks(&self) -> Option<&crate::settings::Callbacks> {
        self.hooks.get().map(Arc::as_ref)
    }

    /// The path a file tool call touches: its file, or for a search its folder (the working directory
    /// when not given). None for other tools.
    pub fn target(&self, tool: &str, input: &Object) -> Option<PathBuf> {
        let path = match tool {
            "Read" | "Edit" | "Write" => input.get("file_path")?.as_str()?,
            "NotebookEdit" => input.get("notebook_path")?.as_str()?,
            "Glob" | "Grep" => input.get("path").and_then(Value::as_str).unwrap_or("."),
            _ => return None,
        };
        Some(self.resolve(path))
    }

    pub fn cwd(&self) -> PathBuf {
        self.state.lock().unwrap().cwd.clone()
    }

    pub fn set_cwd(&self, cwd: PathBuf) {
        self.state.lock().unwrap().cwd = cwd;
    }

    pub fn resolve(&self, path: &str) -> PathBuf {
        resolve(&self.cwd(), path)
    }

    /// Inside the working directory or one of the added directories; relative paths resolve like the tools resolve them.
    pub fn owns(&self, path: &str) -> bool {
        let absolute = self.resolve(path);
        std::iter::once(&self.root).chain(self.extra_dirs.iter()).any(|dir| is_inside(&absolute, dir))
    }

    pub fn mark_read(&self, path: &Path) {
        if let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) {
            self.state.lock().unwrap().reads.insert(path.to_path_buf(), modified);
        }
    }

    /// Instructions for a resolved file path, once per memory file, from the working root's children
    /// down to the file's folder. The root and its ancestors are already in the environment prompt.
    pub fn nested_memory(&self, path: &Path) -> Option<String> {
        let parent = path.parent()?;
        let root = std::iter::once(&self.root)
            .chain(self.extra_dirs.iter())
            .filter(|dir| parent.starts_with(dir))
            .max_by_key(|dir| dir.components().count())?;
        let relative = parent.strip_prefix(root).ok()?;
        let mut dir = root.to_path_buf();
        let mut text = String::new();
        // Keep discovery and marking together so concurrent tool calls cannot attach a folder twice.
        let mut state = self.state.lock().unwrap();
        for component in relative.components() {
            dir.push(component);
            for name in ["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"] {
                let memory = dir.join(name);
                if state.memory_files.contains(&memory) || self.rules.decide("Read", &memory) == Some(Behavior::Deny) {
                    continue;
                }
                let Ok(contents) = std::fs::read_to_string(&memory) else { continue };
                state.memory_files.insert(memory.clone());
                // memory.md documents lazy loading and nearest-last order, but not its exact wrapper.
                // Use a plain system-reminder with the path and contents, as conversation context.
                text.push_str(&format!("\n\n<system-reminder>\nContents of {}:\n\n{contents}\n</system-reminder>", memory.display()));
            }
        }
        if text.is_empty() { None } else { Some(text) }
    }

    /// Claude Code refuses to overwrite a file it has not read, or one that changed since it read it.
    pub fn check_fresh(&self, path: &Path) -> Option<&'static str> {
        let seen = self.state.lock().unwrap().reads.get(path).copied();
        let Some(seen) = seen else {
            return Some("File has not been read yet. Read it first before writing to it.");
        };
        match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(modified) if modified > seen => {
                Some("File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.")
            }
            _ => None,
        }
    }
}

/// Lines with their numbers, the way `cat -n` prints them.
pub fn numbered(lines: &[&str], first_line: usize) -> String {
    lines.iter().enumerate().map(|(i, line)| format!("{:>6}\t{line}", first_line + i)).collect::<Vec<_>>().join("\n")
}

pub struct ToolDefinition {
    pub name: &'static str,
    pub description: String,
    pub input_schema: Value,
    pub annotations: Option<Value>,
}

/// The tools, in Claude Code's own order.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        bash::definition(),
        glob::definition(),
        grep::definition(),
        read::definition(),
        edit::definition(),
        write::definition(),
        notebook::definition(),
        agents::list_definition(),
        agents::send_definition(),
    ]
}

pub async fn run(name: &str, input: &Object, ws: &Workspace, cancel: &tokio_util::sync::CancellationToken) -> ToolResult {
    // Deny rules hold however the call got approved: a pre-approval or a permission answer cannot lift them.
    if let Some(path) = ws.target(name, input)
        && ws.rules.decide(name, &path) == Some(Behavior::Deny)
    {
        return ToolResult::fail(denied_message(name, &path));
    }
    let outcome = match name {
        "Bash" => bash::run(input, ws, cancel).await,
        "Glob" => glob::run(input, ws, cancel).await,
        "Grep" => grep::run(input, ws, cancel).await,
        "Read" => read::run(input, ws),
        "Edit" => edit::run(input, ws),
        "Write" => write::run(input, ws),
        "NotebookEdit" => notebook::run(input, ws),
        "ListAgents" => agents::list(input).await,
        "SendMessage" => agents::send(input).await,
        other => return ToolResult::fail(format!("No such tool: {other}")),
    };
    outcome.unwrap_or_else(|InputError(message)| ToolResult::fail(format!("InputValidationError: {message}")))
}

pub fn denied_message(tool: &str, path: &Path) -> String {
    format!("Permission to use {tool} on {} has been denied by your permission settings.", path.display())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn workspace(files: &[(&str, &str)]) -> (PathBuf, Workspace) {
        let dir = std::env::temp_dir().join(format!("ct-tools-{}-{}", std::process::id(), unique()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        for (name, content) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        (dir.clone(), Workspace::new(dir, vec![], PathRules::default()))
    }

    fn unique() -> u128 {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() + COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as u128
    }

    pub async fn call(ws: &Workspace, name: &str, input: Value) -> ToolResult {
        run(name, input.as_object().unwrap(), ws, &tokio_util::sync::CancellationToken::new()).await
    }

    pub fn text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .map(|c| match c {
                Content::Text(t) => t.clone(),
                Content::Image { mime_type, .. } => format!("[{mime_type}]"),
            })
            .collect()
    }
}

#[cfg(test)]
mod nested_memory_tests {
    use super::*;
    use crate::permissions::rules::{Behavior, Source};
    use super::test_support::workspace;

    #[test]
    fn attaches_each_memory_file_once_in_parent_to_child_order() {
        let (dir, ws) = workspace(&[
            ("CLAUDE.md", "root"),
            ("sub/CLAUDE.md", "parent"),
            ("sub/.claude/CLAUDE.md", "project"),
            ("sub/CLAUDE.local.md", "local"),
            ("sub/deep/CLAUDE.md", "nearest"),
            ("sub/deep/file.txt", "file"),
        ]);
        let file = dir.join("sub/deep/file.txt");
        let attached = ws.nested_memory(&file).unwrap();
        let root_memory = format!("Contents of {}", dir.join("CLAUDE.md").display());
        assert!(!attached.contains(root_memory.as_str()));
        assert!(attached.find("parent").unwrap() < attached.find("project").unwrap());
        assert!(attached.find("project").unwrap() < attached.find("local").unwrap());
        assert!(attached.find("local").unwrap() < attached.find("nearest").unwrap());
        assert!(ws.nested_memory(&file).is_none());
    }

    #[test]
    fn skips_denied_memory_and_files_outside_working_directories() {
        let (dir, _) = workspace(&[("sub/CLAUDE.md", "secret"), ("sub/file.txt", "file")]);
        let rules = PathRules::new(
            &[Source { anchor: &dir, rules: vec![(Behavior::Deny, "Read(./sub/CLAUDE.md)".into())] }],
            &dir,
            &dir,
        );
        let ws = Workspace::new(dir.clone(), vec![], rules);
        assert!(ws.nested_memory(&dir.join("sub/file.txt")).is_none());
        assert!(ws.nested_memory(&std::env::temp_dir().join("outside-claudette/file.txt")).is_none());
    }

    #[test]
    fn loads_memory_below_an_added_directory() {
        let (dir, _) = workspace(&[]);
        let extra = std::env::temp_dir().join(format!("ct-tools-extra-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(extra.join("sub")).unwrap();
        std::fs::write(extra.join("sub/CLAUDE.md"), "extra instructions").unwrap();
        let ws = Workspace::new(dir, vec![extra.clone()], PathRules::default());
        assert!(ws.nested_memory(&extra.join("sub/file.txt")).unwrap().contains("extra instructions"));
    }
}
