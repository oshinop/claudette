use base64::Engine;
use serde_json::json;

use super::{Content, ToolDefinition, ToolOutcome, ToolResult, Workspace, notebook, numbered, opt_num, str_arg};
use crate::json::Object;

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE: usize = 2000;
/// The API rejects images over 5 MB once base64-encoded.
const MAX_IMAGE_BYTES: u64 = 3_750_000;

fn image_type(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "Read",
        description: format!(
            "Reads a file from the user's local filesystem (the machine the user works on, not the one this session runs on).\n\n\
- file_path should be an absolute path; a relative path resolves against the shell's working directory\n\
- By default reads up to {DEFAULT_LIMIT} lines from the start; use offset and limit for long files\n\
- Lines longer than {MAX_LINE} characters are truncated\n\
- Output uses cat -n format: the line number, a tab, then the line\n\
- Reads images (PNG, JPG, GIF, WEBP) and returns them visually\n\
- Reads Jupyter notebooks (.ipynb) as cells with their outputs\n\
- Reads files only, not directories; use Bash with ls for a directory\n\
- Read several files in one response when you need several"
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "The absolute path to the file to read" },
                "offset": { "type": "number", "description": "The line number to start reading from. Only provide if the file is too large to read at once" },
                "limit": { "type": "number", "description": "The number of lines to read. Only provide if the file is too large to read at once." }
            },
            "required": ["file_path"],
            "additionalProperties": false
        }),
        annotations: Some(json!({ "readOnlyHint": true })),
    }
}

pub fn run(input: &Object, ws: &Workspace) -> ToolOutcome {
    let path = ws.resolve(str_arg(input, "file_path")?);
    let shown = path.display();
    let Ok(meta) = std::fs::metadata(&path) else {
        return Ok(ToolResult::fail(format!("File does not exist: {shown}")));
    };
    if meta.is_dir() {
        return Ok(ToolResult::fail(format!("EISDIR: illegal operation on a directory, read '{shown}'")));
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if let Some(mime_type) = image_type(&ext) {
        if meta.len() > MAX_IMAGE_BYTES {
            return Ok(ToolResult::fail(format!("Image is {} bytes; the limit is {MAX_IMAGE_BYTES}.", meta.len())));
        }
        let bytes = std::fs::read(&path).map_err(|e| super::InputError(e.to_string()))?;
        ws.mark_read(&path);
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        return Ok(ToolResult { content: vec![Content::Image { data, mime_type }], is_error: false }.with_nested_memory(ws, &path));
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => return Ok(ToolResult::fail(format!("Could not read {shown}: {e}"))),
    };
    if ext == "ipynb" {
        ws.mark_read(&path);
        if let Ok(text) = notebook::render(&String::from_utf8_lossy(&bytes)) {
            return Ok(ToolResult::ok(text).with_nested_memory(ws, &path));
        }
    }
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Ok(ToolResult::fail(format!("{shown} is a binary file. Use Bash (for example xxd or file) to inspect it.")));
    }
    ws.mark_read(&path);
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.is_empty() {
        return Ok(ToolResult::ok("<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>").with_nested_memory(ws, &path));
    }
    let start = opt_num(input, "offset")?.map(|n| n.floor().max(1.0) as usize).unwrap_or(1);
    if start > lines.len() {
        return Ok(ToolResult::ok(format!(
            "<system-reminder>Warning: the file exists but is shorter than the provided offset ({start}). The file has {} lines.</system-reminder>",
            lines.len()
        )).with_nested_memory(ws, &path));
    }
    let limit = opt_num(input, "limit")?.map(|n| n.floor().max(1.0) as usize).unwrap_or(DEFAULT_LIMIT);
    let truncated: Vec<String> = lines[start - 1..(start - 1 + limit).min(lines.len())]
        .iter()
        .map(|line| if line.chars().count() > MAX_LINE { format!("{}…", line.chars().take(MAX_LINE).collect::<String>()) } else { (*line).to_owned() })
        .collect();
    let refs: Vec<&str> = truncated.iter().map(String::as_str).collect();
    Ok(ToolResult::ok(numbered(&refs, start)).with_nested_memory(ws, &path))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use serde_json::json;

    #[tokio::test]
    async fn numbers_lines_from_the_offset_and_honors_the_limit() {
        let (dir, ws) = workspace(&[("f.txt", "a\nb\nc\nd\n")]);
        let r = call(&ws, "Read", json!({ "file_path": dir.join("f.txt"), "offset": 2, "limit": 2 })).await;
        assert_eq!(text(&r), "     2\tb\n     3\tc");
    }

    #[tokio::test]
    async fn refuses_binary_files_and_directories_and_returns_images() {
        let (dir, ws) = workspace(&[]);
        std::fs::write(dir.join("bin"), [0x7f, 0x45, 0x00, 0x01]).unwrap();
        assert!(call(&ws, "Read", json!({ "file_path": dir.join("bin") })).await.is_error);
        assert!(call(&ws, "Read", json!({ "file_path": dir })).await.is_error);
        std::fs::write(dir.join("p.png"), [0x89, 0x50, 0x4e, 0x47]).unwrap();
        let r = call(&ws, "Read", json!({ "file_path": dir.join("p.png") })).await;
        assert_eq!(r.to_json()["content"][0], json!({ "type": "image", "data": "iVBORw==", "mimeType": "image/png" }));
    }

    #[tokio::test]
    async fn appends_nested_memory_only_on_first_touch() {
        let (dir, ws) = workspace(&[("sub/CLAUDE.md", "subtree instructions"), ("sub/file.txt", "text")]);
        let path = dir.join("sub/file.txt");
        let first = call(&ws, "Read", json!({ "file_path": path })).await;
        assert!(text(&first).contains("subtree instructions"));
        let second = call(&ws, "Read", json!({ "file_path": dir.join("sub/file.txt") })).await;
        assert!(!text(&second).contains("subtree instructions"));
    }
}
