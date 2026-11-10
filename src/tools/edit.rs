use serde_json::json;

use super::{ToolDefinition, ToolOutcome, ToolResult, Workspace, numbered, opt_bool, str_arg};
use crate::json::Object;

/// How many lines around the edit the result shows.
const SNIPPET_CONTEXT: usize = 4;

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "Edit",
        description: "Performs exact string replacements in a file on the user's local filesystem.\n\n\
- The file must have been read with the Read tool first, or this fails\n\
- old_string must match the file exactly, including indentation; when copying from Read output, leave out the line-number prefix\n\
- The edit fails when old_string is not unique; give more context, or set replace_all to change every occurrence\n\
- An empty old_string creates a new file with new_string as its content"
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "The absolute path to the file to modify" },
                "old_string": { "type": "string", "description": "The text to replace" },
                "new_string": { "type": "string", "description": "The text to replace it with (must be different from old_string)" },
                "replace_all": { "type": "boolean", "default": false, "description": "Replace all occurrences of old_string (default false)" }
            },
            "required": ["file_path", "old_string", "new_string"],
            "additionalProperties": false
        }),
        annotations: Some(json!({ "destructiveHint": true })),
    }
}

fn write(path: &std::path::Path, content: &str, ws: &Workspace) -> Result<(), ToolResult> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolResult::fail(format!("Could not create {}: {e}", parent.display())))?;
    }
    std::fs::write(path, content).map_err(|e| ToolResult::fail(format!("Could not write {}: {e}", path.display())))?;
    ws.mark_read(path);
    Ok(())
}

pub fn run(input: &Object, ws: &Workspace) -> ToolOutcome {
    let path = ws.resolve(str_arg(input, "file_path")?);
    let old = str_arg(input, "old_string")?;
    let new = str_arg(input, "new_string")?;
    let replace_all = opt_bool(input, "replace_all")?.unwrap_or(false);
    if old == new {
        return Ok(ToolResult::fail("No changes to make: old_string and new_string are exactly the same."));
    }
    let shown = path.display();
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => return Ok(ToolResult::fail(format!("EISDIR: {shown} is a directory"))),
        Err(_) => {
            if !old.is_empty() {
                return Ok(ToolResult::fail(format!("File does not exist: {shown}")));
            }
            if let Err(fail) = write(&path, new, ws) {
                return Ok(fail);
            }
            return Ok(ToolResult::ok(format!("File created successfully at: {shown}")).with_nested_memory(ws, &path));
        }
        Ok(_) => {}
    }
    if let Some(stale) = ws.check_fresh(&path) {
        return Ok(ToolResult::fail(stale));
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return Ok(ToolResult::fail(format!("Could not read {shown}: {e}"))),
    };
    if old.is_empty() {
        if !content.trim().is_empty() {
            return Ok(ToolResult::fail("Cannot create new file - file already exists."));
        }
        if let Err(fail) = write(&path, new, ws) {
            return Ok(fail);
        }
        return Ok(ToolResult::ok(format!("The file {shown} has been updated successfully.")).with_nested_memory(ws, &path));
    }
    let count = content.matches(old).count();
    if count == 0 {
        return Ok(ToolResult::fail(format!("String to replace not found in file.\nString: {old}")));
    }
    if count > 1 && !replace_all {
        return Ok(ToolResult::fail(format!(
            "Found {count} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. \
To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: {old}"
        )));
    }
    let at = content.find(old).unwrap_or(0);
    let updated = if replace_all { content.replace(old, new) } else { format!("{}{}{}", &content[..at], new, &content[at + old.len()..]) };
    if let Err(fail) = write(&path, &updated, ws) {
        return Ok(fail);
    }
    let lines: Vec<&str> = updated.split('\n').collect();
    let first_line = content[..at].matches('\n').count() + 1;
    let last_line = first_line + new.matches('\n').count();
    let from = first_line.saturating_sub(SNIPPET_CONTEXT).max(1);
    let to = (last_line + SNIPPET_CONTEXT).min(lines.len());
    let summary = if replace_all && count > 1 {
        format!("All {count} occurrences were replaced.")
    } else {
        "Here's the result of running `cat -n` on a snippet of the edited file:".to_owned()
    };
    Ok(ToolResult::ok(format!("The file {shown} has been updated. {summary}\n{}", numbered(&lines[from - 1..to], from))).with_nested_memory(ws, &path))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use serde_json::json;

    #[tokio::test]
    async fn existing_files_must_be_read_first_and_again_after_outside_changes() {
        let (dir, ws) = workspace(&[("f.txt", "one\n")]);
        let file = dir.join("f.txt");
        assert!(text(&call(&ws, "Edit", json!({ "file_path": file, "old_string": "one", "new_string": "two" })).await).contains("has not been read yet"));
        call(&ws, "Read", json!({ "file_path": file })).await;
        assert!(!call(&ws, "Edit", json!({ "file_path": file, "old_string": "one", "new_string": "two" })).await.is_error);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, "changed elsewhere\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(later).unwrap();
        assert!(text(&call(&ws, "Write", json!({ "file_path": file, "content": "mine" })).await).contains("modified since read"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "changed elsewhere\n");
    }

    #[tokio::test]
    async fn edit_needs_a_unique_match_unless_replace_all() {
        let (dir, ws) = workspace(&[("f.txt", "x = 1\nx = 1\n")]);
        let file = dir.join("f.txt");
        call(&ws, "Read", json!({ "file_path": file })).await;
        assert!(text(&call(&ws, "Edit", json!({ "file_path": file, "old_string": "x = 1", "new_string": "y" })).await).contains("Found 2 matches"));
        assert!(text(&call(&ws, "Edit", json!({ "file_path": file, "old_string": "zzz", "new_string": "y" })).await).contains("not found"));
        call(&ws, "Edit", json!({ "file_path": file, "old_string": "x = 1", "new_string": "y = $&2", "replace_all": true })).await;
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "y = $&2\ny = $&2\n");
    }

    #[tokio::test]
    async fn write_creates_directories_and_an_empty_old_string_creates_a_file() {
        let (dir, ws) = workspace(&[]);
        call(&ws, "Write", json!({ "file_path": dir.join("a/b/c.txt"), "content": "deep" })).await;
        assert_eq!(std::fs::read_to_string(dir.join("a/b/c.txt")).unwrap(), "deep");
        call(&ws, "Edit", json!({ "file_path": dir.join("new.txt"), "old_string": "", "new_string": "fresh" })).await;
        assert_eq!(std::fs::read_to_string(dir.join("new.txt")).unwrap(), "fresh");
    }

    #[tokio::test]
    async fn appends_nested_memory_after_a_successful_create() {
        let (dir, ws) = workspace(&[("sub/CLAUDE.md", "subtree instructions")]);
        let result = call(
            &ws,
            "Edit",
            json!({ "file_path": dir.join("sub/new.txt"), "old_string": "", "new_string": "text" }),
        )
        .await;
        assert!(text(&result).contains("subtree instructions"));
    }
}
