//! Grep, with ripgrep's own libraries: no `rg` needed on this machine, and .gitignore is honored.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{InputError, ToolDefinition, ToolOutcome, ToolResult, Workspace, opt_bool, opt_num, opt_str, str_arg};
use crate::json::Object;
use crate::permissions::rules::PathRules;

const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_OUTPUT: usize = 30_000;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Content,
    FilesWithMatches,
    Count,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "Grep",
        description: "Searches file contents on the user's local filesystem with ripgrep's search engine.\n\n\
- Full regular expression syntax (for example \"log.*Error\" or \"function\\\\s+\\\\w+\")\n\
- Filter files with glob (for example \"*.js\" or \"**/*.tsx\") or type (for example \"js\", \"py\")\n\
- output_mode: \"files_with_matches\" (default) lists files, \"content\" shows matching lines, \"count\" shows counts per file\n\
- Searches the shell's working directory unless path names a file or directory\n\
- Skips files that .gitignore excludes, and binary files\n\
- Use multiline: true for patterns that span lines"
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "The regular expression pattern to search for in file contents" },
                "path": { "type": "string", "description": "File or directory to search in. Defaults to the current working directory." },
                "glob": { "type": "string", "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\")" },
                "type": { "type": "string", "description": "File type to search (rg --type), e.g. js, py, rust" },
                "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Output mode; defaults to \"files_with_matches\"" },
                "-i": { "type": "boolean", "description": "Case insensitive search" },
                "-n": { "type": "boolean", "description": "Show line numbers in output (content mode). Defaults to true." },
                "-A": { "type": "number", "description": "Lines to show after each match (content mode)" },
                "-B": { "type": "number", "description": "Lines to show before each match (content mode)" },
                "-C": { "type": "number", "description": "Lines to show before and after each match (content mode)" },
                "context": { "type": "number", "description": "Alias for -C" },
                "head_limit": { "type": "number", "description": format!("Limit output to the first N lines or entries (default {DEFAULT_HEAD_LIMIT}; 0 for no limit)") },
                "offset": { "type": "number", "description": "Skip the first N lines or entries before applying head_limit" },
                "multiline": { "type": "boolean", "description": "Let patterns span lines; . matches newlines" }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
        annotations: Some(json!({ "readOnlyHint": true })),
    }
}

struct Options {
    mode: Mode,
    line_numbers: bool,
    before: usize,
    after: usize,
    multiline: bool,
    case_insensitive: bool,
    pattern: String,
    glob: Option<String>,
    file_type: Option<String>,
}

fn context_arg(input: &Object, key: &str) -> Result<Option<usize>, InputError> {
    Ok(opt_num(input, key)?.map(|n| n.max(0.0) as usize))
}

impl Options {
    fn from(input: &Object) -> Result<Result<Options, String>, InputError> {
        let mode = match opt_str(input, "output_mode")?.unwrap_or("files_with_matches") {
            "content" => Mode::Content,
            "files_with_matches" => Mode::FilesWithMatches,
            "count" => Mode::Count,
            other => return Ok(Err(format!("Unknown output_mode: {other}"))),
        };
        let context = match context_arg(input, "-C")? {
            Some(c) => Some(c),
            None => context_arg(input, "context")?,
        };
        let (mut before, mut after) = (0, 0);
        if mode == Mode::Content {
            before = context_arg(input, "-B")?.or(context).unwrap_or(0);
            after = context_arg(input, "-A")?.or(context).unwrap_or(0);
        }
        Ok(Ok(Options {
            mode,
            line_numbers: mode == Mode::Content && opt_bool(input, "-n")?.unwrap_or(true),
            before,
            after,
            multiline: opt_bool(input, "multiline")?.unwrap_or(false),
            case_insensitive: opt_bool(input, "-i")?.unwrap_or(false),
            pattern: str_arg(input, "pattern")?.to_owned(),
            glob: opt_str(input, "glob")?.filter(|g| !g.is_empty()).map(str::to_owned),
            file_type: opt_str(input, "type")?.filter(|t| !t.is_empty()).map(str::to_owned),
        }))
    }
}

/// Collects one file's output the way ripgrep prints it.
struct Collect<'a> {
    /// The file name prefix; empty when searching a single file, as ripgrep omits it then.
    prefix: String,
    line_numbers: bool,
    lines: &'a mut Vec<String>,
    count: usize,
    content: bool,
}

impl Collect<'_> {
    fn push(&mut self, separator: char, line_number: Option<u64>, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        let number = match line_number {
            Some(n) if self.line_numbers => format!("{n}{separator}"),
            _ => String::new(),
        };
        let prefix = if self.prefix.is_empty() { String::new() } else { format!("{}{separator}", self.prefix) };
        self.lines.push(format!("{prefix}{number}{text}"));
    }
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        let first = mat.line_number();
        for (i, line) in mat.lines().enumerate() {
            self.count += 1;
            if self.content {
                self.push(':', first.map(|n| n + i as u64), line);
            }
        }
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, std::io::Error> {
        if self.content {
            self.push('-', ctx.line_number(), ctx.bytes());
        }
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, std::io::Error> {
        if self.content {
            self.lines.push("--".into());
        }
        Ok(true)
    }
}

fn files(target: &Path, opts: &Options, rules: &Arc<PathRules>) -> Result<Box<dyn Iterator<Item = PathBuf>>, String> {
    if target.is_file() {
        return Ok(Box::new(std::iter::once(target.to_path_buf())));
    }
    let mut walk = ignore::WalkBuilder::new(target);
    walk.hidden(false).require_git(false).filter_entry(|e| e.file_name() != ".git").sort_by_file_name(|a, b| a.cmp(b));
    if let Some(glob) = &opts.glob {
        let mut overrides = ignore::overrides::OverrideBuilder::new(target);
        overrides.add(glob).map_err(|e| format!("invalid glob: {e}"))?;
        walk.overrides(overrides.build().map_err(|e| format!("invalid glob: {e}"))?);
    }
    if let Some(file_type) = &opts.file_type {
        let mut types = ignore::types::TypesBuilder::new();
        types.add_defaults();
        types.select(file_type);
        walk.types(types.build().map_err(|e| e.to_string())?);
    }
    let rules = Arc::clone(rules);
    Ok(Box::new(
        walk.build().flatten().filter(|e| e.file_type().is_some_and(|t| t.is_file())).map(ignore::DirEntry::into_path).filter(move |p| !rules.read_denied(p)),
    ))
}

fn search(target: &Path, opts: &Options, rules: &Arc<PathRules>, cancel: &CancellationToken) -> Result<Vec<String>, String> {
    let mut matcher = RegexMatcherBuilder::new();
    matcher.case_insensitive(opts.case_insensitive);
    if opts.multiline {
        matcher.multi_line(true).dot_matches_new_line(true);
    } else {
        matcher.line_terminator(Some(b'\n'));
    }
    let matcher = matcher.build(&opts.pattern).map_err(|e| e.to_string())?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(opts.before)
        .after_context(opts.after)
        .multi_line(opts.multiline)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let single = target.is_file();
    let mut out = vec![];
    for path in files(target, opts, rules)? {
        if cancel.is_cancelled() {
            break;
        }
        let shown = path.display().to_string();
        let mut lines = vec![];
        let mut sink = Collect {
            prefix: if single { String::new() } else { shown.clone() },
            line_numbers: opts.line_numbers,
            lines: &mut lines,
            count: 0,
            content: opts.mode == Mode::Content,
        };
        if searcher.search_path(&matcher, &path, &mut sink).is_err() {
            continue;
        }
        let count = sink.count;
        if count == 0 {
            continue;
        }
        match opts.mode {
            Mode::FilesWithMatches => out.push(shown),
            Mode::Count => out.push(if single { count.to_string() } else { format!("{shown}:{count}") }),
            Mode::Content => {
                if (opts.before > 0 || opts.after > 0) && !out.is_empty() {
                    out.push("--".into());
                }
                out.append(&mut lines);
            }
        }
    }
    Ok(out)
}

fn modified(path: &str) -> SystemTime {
    std::fs::metadata(path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

pub async fn run(input: &Object, ws: &Workspace, cancel: &CancellationToken) -> ToolOutcome {
    let opts = match Options::from(input)? {
        Ok(opts) => opts,
        Err(message) => return Ok(ToolResult::fail(message)),
    };
    let target = ws.resolve(opt_str(input, "path")?.unwrap_or("."));
    if !target.exists() {
        return Ok(ToolResult::fail(format!("Path does not exist: {}", target.display())));
    }
    let skip = opt_num(input, "offset")?.map_or(0, |n| n.max(0.0) as usize);
    let head = opt_num(input, "head_limit")?.map_or(DEFAULT_HEAD_LIMIT, |n| n.max(0.0) as usize);
    let mode = opts.mode;
    let cancel = cancel.clone();
    let rules = Arc::clone(&ws.rules);
    let found = tokio::task::spawn_blocking(move || search(&target, &opts, &rules, &cancel)).await.map_err(|e| InputError(e.to_string()))?;
    let mut entries = match found {
        Ok(entries) => entries,
        Err(message) => return Ok(ToolResult::fail(message)),
    };
    if mode == Mode::FilesWithMatches {
        entries.sort_by_cached_key(|p| std::cmp::Reverse(modified(p)));
    }
    let total = entries.len();
    let shown: Vec<String> = entries.into_iter().skip(skip).take(if head > 0 { head } else { usize::MAX }).collect();
    let more = if total > skip + shown.len() { format!("\n(Showing {} of {total}; use offset and head_limit for more.)", shown.len()) } else { String::new() };
    if mode == Mode::FilesWithMatches {
        return Ok(ToolResult::ok(if total == 0 {
            "No files found".to_owned()
        } else {
            format!("Found {total} file{}\n{}{more}", if total == 1 { "" } else { "s" }, shown.join("\n"))
        }));
    }
    if total == 0 {
        return Ok(ToolResult::ok("No matches found"));
    }
    let mut text = shown.join("\n");
    if text.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n(output truncated)");
    }
    Ok(ToolResult::ok(text + &more))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use serde_json::json;

    #[tokio::test]
    async fn modes_filters_and_context() {
        let (dir, ws) = workspace(&[
            ("a.ts", "one\nneedle here\nthree\n"),
            ("b.py", "Needle\n"),
            ("dist/c.ts", "needle\n"),
            (".gitignore", "dist/\n"),
        ]);
        let a = dir.join("a.ts").display().to_string();
        let b = dir.join("b.py").display().to_string();
        let files = text(&call(&ws, "Grep", json!({ "pattern": "needle", "-i": true })).await);
        assert!(files.starts_with("Found 2 files\n"), "{files}");
        assert!(files.contains(&a) && files.contains(&b) && !files.contains("dist"));
        assert_eq!(text(&call(&ws, "Grep", json!({ "pattern": "needle", "type": "py", "-i": true })).await), format!("Found 1 file\n{b}"));
        assert_eq!(text(&call(&ws, "Grep", json!({ "pattern": "needle", "glob": "*.ts", "output_mode": "content" })).await), format!("{a}:2:needle here"));
        assert_eq!(
            text(&call(&ws, "Grep", json!({ "pattern": "needle", "path": "a.ts", "output_mode": "content", "-B": 1 })).await),
            "1-one\n2:needle here"
        );
        assert_eq!(text(&call(&ws, "Grep", json!({ "pattern": "e", "path": "a.ts", "output_mode": "count" })).await), "3");
        assert_eq!(text(&call(&ws, "Grep", json!({ "pattern": "here\\nthree", "path": "a.ts", "output_mode": "content", "multiline": true })).await), "2:needle here\n3:three");
        assert_eq!(text(&call(&ws, "Grep", json!({ "pattern": "zzz" })).await), "No files found");
    }

    #[tokio::test]
    async fn head_limit_and_offset_page_the_entries() {
        let (_, ws) = workspace(&[("f.txt", "x\nx\nx\nx\n")]);
        let r = text(&call(&ws, "Grep", json!({ "pattern": "x", "path": "f.txt", "output_mode": "content", "offset": 1, "head_limit": 2 })).await);
        assert_eq!(r, "2:x\n3:x\n(Showing 2 of 4; use offset and head_limit for more.)");
    }
}
