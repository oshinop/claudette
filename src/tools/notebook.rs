use std::hash::{BuildHasher, Hasher};

use serde::Serialize;
use serde_json::{Value, json};

use super::{ToolDefinition, ToolOutcome, ToolResult, Workspace, opt_str, str_arg};
use crate::json::Object;

fn joined(source: Option<&Value>) -> String {
    match source {
        Some(Value::Array(parts)) => parts.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn parse(text: &str) -> Result<Object, String> {
    let Ok(Value::Object(mut nb)) = serde_json::from_str::<Value>(text) else {
        return Err("not a notebook: not a JSON object".into());
    };
    let Some(Value::Array(cells)) = nb.get_mut("cells") else {
        return Err("not a notebook: no cells array".into());
    };
    cells.retain(Value::is_object);
    Ok(nb)
}

fn cells(nb: &mut Object) -> &mut Vec<Value> {
    match nb.get_mut("cells") {
        Some(Value::Array(cells)) => cells,
        _ => unreachable!("parse checked the cells array"),
    }
}

fn output_text(output: &Value) -> String {
    let Some(output) = output.as_object() else { return String::new() };
    match output.get("output_type").and_then(Value::as_str) {
        Some("stream") => return joined(output.get("text")),
        Some("error") => {
            let field = |k: &str| output.get(k).map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_else(|| "undefined".into());
            return format!("{}: {}", field("ename"), field("evalue"));
        }
        _ => {}
    }
    match output.get("data").and_then(Value::as_object) {
        Some(data) if data.contains_key("text/plain") => joined(data.get("text/plain")),
        Some(data) => format!("[{} output]", data.keys().cloned().collect::<Vec<_>>().join(", ")),
        None => String::new(),
    }
}

fn cell_id(cell: &Value, index: usize) -> String {
    cell.get("id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("cell-{index}"))
}

/// The cells of a notebook as text, with the ids NotebookEdit takes.
pub fn render(text: &str) -> Result<String, String> {
    let mut nb = parse(text)?;
    Ok(cells(&mut nb)
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let outputs: Vec<String> = cell.get("outputs").and_then(Value::as_array).map(|o| o.iter().map(output_text).filter(|t| !t.is_empty()).collect()).unwrap_or_default();
            let shown = if outputs.is_empty() { String::new() } else { format!("\n<outputs>\n{}\n</outputs>", outputs.join("\n")) };
            let kind = cell.get("cell_type").and_then(Value::as_str).unwrap_or("undefined");
            format!("<cell id=\"{}\" type=\"{kind}\">\n{}{shown}\n</cell>", cell_id(cell, index), joined(cell.get("source")))
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Source the way Jupyter stores it: lines that keep their newline.
fn to_source(text: &str) -> Value {
    Value::Array(text.split_inclusive('\n').map(|l| Value::String(l.to_owned())).collect())
}

fn find_cell(cells: &[Value], id: &str) -> Option<usize> {
    if let Some(i) = cells.iter().position(|c| c.get("id").and_then(Value::as_str) == Some(id)) {
        return Some(i);
    }
    id.strip_prefix("cell-").and_then(|n| n.parse().ok()).filter(|&i: &usize| i < cells.len())
}

fn random_id() -> String {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    format!("{:08x}", h.finish() as u32)
}

/// Written the way Jupyter writes notebooks: one-space indentation and a trailing newline.
fn serialize(nb: &Object) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(b" "));
    nb.serialize(&mut ser).expect("JSON values always serialize");
    out.push(b'\n');
    String::from_utf8(out).expect("serde_json writes UTF-8")
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "NotebookEdit",
        description: "Replaces, inserts or deletes one cell of a Jupyter notebook (.ipynb) on the user's local filesystem.\n\n\
- Read the notebook first; cell ids appear in the Read output\n\
- edit_mode \"replace\" (default) replaces the cell's source; \"insert\" adds a new cell after cell_id (or first, without cell_id) and needs cell_type; \"delete\" removes the cell"
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "notebook_path": { "type": "string", "description": "The absolute path to the notebook to edit" },
                "cell_id": { "type": "string", "description": "The id of the cell to edit; for insert, the new cell goes after it" },
                "new_source": { "type": "string", "description": "The new source of the cell" },
                "cell_type": { "type": "string", "enum": ["code", "markdown"], "description": "The cell type; required for insert" },
                "edit_mode": { "type": "string", "enum": ["replace", "insert", "delete"], "description": "replace (default), insert or delete" }
            },
            "required": ["notebook_path", "new_source"],
            "additionalProperties": false
        }),
        annotations: Some(json!({ "destructiveHint": true })),
    }
}

pub fn run(input: &Object, ws: &Workspace) -> ToolOutcome {
    let path = ws.resolve(str_arg(input, "notebook_path")?);
    let shown = path.display();
    if path.extension().and_then(|e| e.to_str()) != Some("ipynb") {
        return Ok(ToolResult::fail("File must be a Jupyter notebook (.ipynb)"));
    }
    if std::fs::metadata(&path).is_err() {
        return Ok(ToolResult::fail(format!("Notebook does not exist: {shown}")));
    }
    if let Some(stale) = ws.check_fresh(&path) {
        return Ok(ToolResult::fail(stale));
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return Ok(ToolResult::fail(format!("Could not read {shown}: {e}"))),
    };
    let mut nb = match parse(&text) {
        Ok(nb) => nb,
        Err(e) => return Ok(ToolResult::fail(e)),
    };
    let mode = opt_str(input, "edit_mode")?.unwrap_or("replace");
    let cell_id_arg = opt_str(input, "cell_id")?;
    let cell_type = opt_str(input, "cell_type")?;
    let source = str_arg(input, "new_source")?;
    let index = match cell_id_arg {
        None => None,
        Some(id) => match find_cell(cells(&mut nb), id) {
            Some(i) => Some(i),
            None => return Ok(ToolResult::fail(format!("Cell not found: {id}"))),
        },
    };

    let summary = match mode {
        "insert" => {
            let Some(kind @ ("code" | "markdown")) = cell_type else {
                return Ok(ToolResult::fail("cell_type (code or markdown) is required for insert"));
            };
            let minor = nb.get("nbformat_minor").and_then(Value::as_u64).unwrap_or(0);
            let with_ids = nb.get("nbformat").and_then(Value::as_u64) == Some(4) && minor >= 5;
            let mut cell = json!({ "cell_type": kind, "metadata": {}, "source": to_source(source) });
            if with_ids {
                cell["id"] = Value::String(random_id());
            }
            if kind == "code" {
                cell["execution_count"] = Value::Null;
                cell["outputs"] = json!([]);
            }
            let at = index.map_or(0, |i| i + 1);
            let id = cell_id(&cell, at);
            cells(&mut nb).insert(at, cell);
            format!("Inserted cell {id}")
        }
        "delete" => {
            let (Some(id), Some(i)) = (cell_id_arg, index) else {
                return Ok(ToolResult::fail("cell_id is required for delete"));
            };
            cells(&mut nb).remove(i);
            format!("Deleted cell {id}")
        }
        "replace" => {
            let (Some(id), Some(i)) = (cell_id_arg, index) else {
                return Ok(ToolResult::fail("cell_id is required for replace"));
            };
            let cell = &mut cells(&mut nb)[i];
            cell["source"] = to_source(source);
            if let Some(kind) = cell_type.filter(|k| !k.is_empty()) {
                cell["cell_type"] = Value::String(kind.to_owned());
            }
            if cell.get("cell_type").and_then(Value::as_str) == Some("code") {
                cell["execution_count"] = Value::Null;
                cell["outputs"] = json!([]);
            }
            format!("Updated cell {id}")
        }
        other => return Ok(ToolResult::fail(format!("Unknown edit_mode: {other}"))),
    };

    if let Err(e) = std::fs::write(&path, serialize(&nb)) {
        return Ok(ToolResult::fail(format!("Could not write {shown}: {e}")));
    }
    ws.mark_read(&path);
    Ok(ToolResult::ok(format!("{summary} in {shown}")).with_nested_memory(ws, &path))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use serde_json::{Value, json};

    const NB: &str = r#"{"cells":[{"cell_type":"code","id":"a1","source":["x = 1\n","x"],"outputs":[{"output_type":"execute_result","data":{"text/plain":["1"]}}],"execution_count":1,"metadata":{}}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#;

    #[tokio::test]
    async fn reads_cells_with_outputs_and_edits_by_id() {
        let (dir, ws) = workspace(&[("n.ipynb", NB)]);
        let path = dir.join("n.ipynb");
        let shown = text(&call(&ws, "Read", json!({ "file_path": path })).await);
        assert_eq!(shown, "<cell id=\"a1\" type=\"code\">\nx = 1\nx\n<outputs>\n1\n</outputs>\n</cell>");
        let r = call(&ws, "NotebookEdit", json!({ "notebook_path": path, "cell_id": "a1", "new_source": "y = 2\ny" })).await;
        assert!(!r.is_error, "{}", text(&r));
        call(&ws, "NotebookEdit", json!({ "notebook_path": path, "cell_id": "a1", "edit_mode": "insert", "cell_type": "markdown", "new_source": "# hi" })).await;
        let nb: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(nb["cells"][0]["source"], json!(["y = 2\n", "y"]));
        assert_eq!(nb["cells"][0]["outputs"], json!([]));
        assert_eq!(nb["cells"][1]["cell_type"], "markdown");
        assert_eq!(nb["cells"][1]["id"].as_str().unwrap().len(), 8);
        let missing = call(&ws, "NotebookEdit", json!({ "notebook_path": path, "cell_id": "nope", "new_source": "" })).await;
        assert!(text(&missing).contains("Cell not found"));
    }

    #[tokio::test]
    async fn appends_nested_memory_after_a_successful_edit() {
        let (dir, ws) = workspace(&[("sub/CLAUDE.md", "subtree instructions"), ("sub/n.ipynb", NB)]);
        let path = dir.join("sub/n.ipynb");
        ws.mark_read(&path);
        let result = call(&ws, "NotebookEdit", json!({ "notebook_path": path, "cell_id": "a1", "new_source": "y" })).await;
        assert!(text(&result).contains("subtree instructions"));
    }
}
