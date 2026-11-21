use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_FILE: OnceLock<Option<String>> = OnceLock::new();

pub fn open(path: Option<String>) {
    let _ = LOG_FILE.set(path);
}

pub fn enabled() -> bool {
    matches!(LOG_FILE.get(), Some(Some(_)))
}

pub fn trace(dir: &str, line: &str) {
    let Some(Some(path)) = LOG_FILE.get() else { return };
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let entry = serde_json::json!({ "t": t as u64, "pid": std::process::id(), "dir": dir, "line": line });
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{entry}");
    }
}

static HELD: Mutex<Option<Vec<String>>> = Mutex::new(None);

pub fn warn(message: &str) {
    trace("warn", message);
    stderr_line(&format!("claudette: {message}"));
}

pub fn stderr_line(line: &str) {
    match HELD.lock().unwrap().as_mut() {
        Some(held) => held.push(line.to_owned()),
        None => eprintln!("{line}"),
    }
}

pub fn hold_warnings() -> HeldWarnings {
    HELD.lock().unwrap().get_or_insert_with(Vec::new);
    HeldWarnings
}

pub struct HeldWarnings;

impl Drop for HeldWarnings {
    fn drop(&mut self) {
        for line in HELD.lock().unwrap().take().unwrap_or_default() {
            eprintln!("{line}");
        }
    }
}
