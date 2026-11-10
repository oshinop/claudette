use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::GlobBuilder;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{InputError, ToolDefinition, ToolOutcome, ToolResult, Workspace, opt_str, str_arg};
use crate::json::Object;

const MAX_RESULTS: usize = 100;
/// Stops a pattern like `**` in a huge tree from scanning forever.
const MAX_SCANNED: usize = 200_000;

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "Glob",
        description: "Fast file pattern matching on the user's local filesystem.\n\n\
- Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\"\n\
- Returns matching file paths sorted by modification time, newest first\n\
- Searches the shell's working directory unless path names another directory\n\
- Skips files that .gitignore excludes\n\
- Use it to find files by name; use Grep to search their contents"
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "The glob pattern to match files against" },
                "path": { "type": "string", "description": "The directory to search in. Omit it to use the current working directory." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
        annotations: Some(json!({ "readOnlyHint": true })),
    }
}

/// An absolute pattern searches from its literal leading directories (`/a/b/**/*.rs` searches `/a/b`).
fn split_absolute(pattern: &str) -> Option<(PathBuf, String)> {
    if !pattern.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = pattern.split('/').collect();
    let literal = parts.iter().take_while(|p| !p.contains(['*', '?', '[', '{'])).count().min(parts.len() - 1);
    let base = parts[..literal].join("/");
    Some((PathBuf::from(if base.is_empty() { "/" } else { &base }), parts[literal..].join("/")))
}

fn search(dir: &Path, pattern: &str, cancel: &CancellationToken) -> Result<Vec<(PathBuf, SystemTime)>, InputError> {
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|e| InputError(format!("invalid glob pattern: {e}")))?
        .compile_matcher();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    let mut found = vec![];
    for (scanned, entry) in walker.flatten().enumerate() {
        if scanned >= MAX_SCANNED || cancel.is_cancelled() {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(dir) else { continue };
        if matcher.is_match(relative) {
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((entry.into_path(), modified));
        }
    }
    Ok(found)
}

pub async fn run(input: &Object, ws: &Workspace, cancel: &CancellationToken) -> ToolOutcome {
    let pattern = str_arg(input, "pattern")?;
    let (dir, pattern) = match split_absolute(pattern) {
        Some((base, rest)) => (base, rest),
        None => (ws.resolve(opt_str(input, "path")?.unwrap_or(".")), pattern.to_owned()),
    };
    if !dir.is_dir() {
        return Ok(ToolResult::fail(format!("Directory does not exist: {}", dir.display())));
    }
    let cancel = cancel.clone();
    let mut found = tokio::task::spawn_blocking(move || search(&dir, &pattern, &cancel))
        .await
        .map_err(|e| InputError(e.to_string()))??;
    found.retain(|(path, _)| !ws.rules.read_denied(path));
    if found.is_empty() {
        return Ok(ToolResult::ok("No files found"));
    }
    found.sort_by(|a, b| b.1.cmp(&a.1));
    let truncated = found.len() > MAX_RESULTS;
    let mut shown: Vec<String> = found.into_iter().take(MAX_RESULTS).map(|(p, _)| p.display().to_string()).collect();
    if truncated {
        shown.push("(Results are truncated. Consider using a more specific path or pattern.)".into());
    }
    Ok(ToolResult::ok(shown.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use serde_json::json;

    #[tokio::test]
    async fn matches_relative_to_the_directory_and_skips_ignored_files() {
        let (dir, ws) = workspace(&[("a.ts", ""), ("src/b.ts", ""), (".hidden/c.ts", ""), ("dist/d.ts", ""), (".gitignore", "dist/\n")]);
        let top = text(&call(&ws, "Glob", json!({ "pattern": "*.ts" })).await);
        assert_eq!(top, dir.join("a.ts").display().to_string());
        let mut all: Vec<String> = text(&call(&ws, "Glob", json!({ "pattern": "**/*.ts" })).await).lines().map(str::to_owned).collect();
        all.sort();
        let mut expected: Vec<String> = ["a.ts", ".hidden/c.ts", "src/b.ts"].iter().map(|f| dir.join(f).display().to_string()).collect();
        expected.sort();
        assert_eq!(all, expected);
        let absolute = text(&call(&ws, "Glob", json!({ "pattern": format!("{}/src/*.ts", dir.display()) })).await);
        assert_eq!(absolute, dir.join("src/b.ts").display().to_string());
        assert_eq!(text(&call(&ws, "Glob", json!({ "pattern": "*.nope" })).await), "No files found");
    }
}
