use serde_json::json;

use super::{ToolDefinition, ToolOutcome, ToolResult, Workspace, str_arg};
use crate::json::Object;

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "Write",
        description: "Writes a file to the user's local filesystem, replacing it if it exists.\n\n\
- An existing file must have been read with the Read tool first, or this fails\n\
- Prefer the Edit tool for changes to an existing file; it sends only the change\n\
- Missing parent directories are created\n\
- file_path should be an absolute path"
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "The absolute path to the file to write (must be absolute, not relative)" },
                "content": { "type": "string", "description": "The content to write to the file" }
            },
            "required": ["file_path", "content"],
            "additionalProperties": false
        }),
        annotations: Some(json!({ "destructiveHint": true })),
    }
}

pub fn run(input: &Object, ws: &Workspace) -> ToolOutcome {
    let path = ws.resolve(str_arg(input, "file_path")?);
    let content = str_arg(input, "content")?;
    let existed = match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => return Ok(ToolResult::fail(format!("EISDIR: {} is a directory", path.display()))),
        Ok(_) => {
            if let Some(stale) = ws.check_fresh(&path) {
                return Ok(ToolResult::fail(stale));
            }
            true
        }
        Err(_) => false,
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Ok(ToolResult::fail(format!("Could not create {}: {e}", parent.display())));
    }
    if let Err(e) = std::fs::write(&path, content) {
        return Ok(ToolResult::fail(format!("Could not write {}: {e}", path.display())));
    }
    ws.mark_read(&path);
    Ok(ToolResult::ok(if existed {
        format!("The file {} has been updated successfully.", path.display())
    } else {
        format!("File created successfully at: {}", path.display())
    }).with_nested_memory(ws, &path))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use serde_json::json;

    #[tokio::test]
    async fn appends_nested_memory_after_a_successful_create() {
        let (dir, ws) = workspace(&[("sub/CLAUDE.md", "subtree instructions")]);
        let result = call(&ws, "Write", json!({ "file_path": dir.join("sub/new.txt"), "content": "text" })).await;
        assert!(text(&result).contains("subtree instructions"));
    }
}
