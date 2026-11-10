//! The optional frame log (`CLAUDETTE_LOG`) and warnings.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_FILE: OnceLock<Option<String>> = OnceLock::new();

pub fn open(path: Option<String>) {
    let _ = LOG_FILE.set(path);
}

/// Whether a frame log is open; lets callers skip building a line nobody records.
pub fn enabled() -> bool {
    matches!(LOG_FILE.get(), Some(Some(_)))
}

/// Records one line crossing the relay. `dir`: host→claude ("in"), claude→host ("out"), or a note.
pub fn trace(dir: &str, line: &str) {
    let Some(Some(path)) = LOG_FILE.get() else { return };
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let entry = serde_json::json!({ "t": t as u64, "pid": std::process::id(), "dir": dir, "line": line });
    // A broken log must never break the session.
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{entry}");
    }
}

pub fn warn(message: &str) {
    eprintln!("claudette: {message}");
    trace("warn", message);
}
