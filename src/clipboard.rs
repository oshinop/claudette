use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const LIMIT: u64 = 16 << 20;

struct Tool {
    copy: &'static [&'static str],
    paste: &'static [&'static str],
}

const MACOS: Tool = Tool { copy: &["pbcopy"], paste: &["pbpaste"] };
const WAYLAND: Tool = Tool { copy: &["wl-copy"], paste: &["wl-paste", "--no-newline"] };
const XCLIP: Tool = Tool { copy: &["xclip", "-selection", "clipboard"], paste: &["xclip", "-selection", "clipboard", "-o"] };
const XSEL: Tool = Tool { copy: &["xsel", "--clipboard", "--input"], paste: &["xsel", "--clipboard", "--output"] };

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| p.is_file())
}

fn tool() -> Result<&'static Tool, String> {
    if cfg!(target_os = "macos") {
        return Ok(&MACOS);
    }
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    let candidates: &[&Tool] = match (set("WAYLAND_DISPLAY"), set("DISPLAY")) {
        (true, true) => &[&WAYLAND, &XCLIP, &XSEL],
        (true, false) => &[&WAYLAND],
        (false, true) => &[&XCLIP, &XSEL],
        (false, false) => &[],
    };
    candidates
        .iter()
        .copied()
        .find(|t| on_path(t.copy[0]).is_some())
        .ok_or_else(|| "no clipboard on this machine: it needs a graphical session with wl-clipboard, xclip or xsel".to_owned())
}

async fn run(argv: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut child = tokio::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(if input.is_some() { Stdio::null() } else { Stdio::piped() })
        .stderr(if input.is_some() { Stdio::null() } else { Stdio::piped() })
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", argv[0]))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input).await.map_err(|e| format!("could not write to {}: {e}", argv[0]))?;
    }
    let mut out = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout.take(LIMIT).read_to_end(&mut out).await.map_err(|e| format!("could not read from {}: {e}", argv[0]))?;
    }
    let done = child.wait_with_output().await.map_err(|e| format!("{} failed: {e}", argv[0]))?;
    if !done.status.success() {
        return Err(format!("{} failed ({}): {}", argv[0], done.status, String::from_utf8_lossy(&done.stderr).trim()));
    }
    Ok(out)
}

pub async fn copy(data: &[u8]) -> Result<(), String> {
    run(tool()?.copy, Some(data)).await.map(drop)
}

pub async fn paste() -> Result<Vec<u8>, String> {
    run(tool()?.paste, None).await
}
