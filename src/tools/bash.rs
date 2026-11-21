use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::{ToolDefinition, ToolOutcome, ToolResult, Workspace, opt_bool, opt_num, str_arg};
use crate::json::Object;
use crate::ssh::shell_quote;

const DEFAULT_TIMEOUT: u64 = 120_000;
const MAX_TIMEOUT: u64 = 600_000;
const MAX_OUTPUT: usize = 30_000;
/// Bytes kept from a command's output: enough for MAX_OUTPUT characters of any UTF-8.
const MAX_OUTPUT_BYTES: usize = MAX_OUTPUT * 4;

/// Every command runs in its own process group, so a timeout or an interrupt stops everything it
/// started. Whatever is still running when the session ends goes with it, like Claude Code's shells.
static GROUPS: LazyLock<Mutex<HashSet<u32>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static COMMANDS: AtomicU64 = AtomicU64::new(0);

fn kill_group(pid: u32) {
    // Already gone is fine.
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
}

pub fn kill_all_commands() {
    forget_ended_groups();
    for pid in GROUPS.lock().unwrap().drain() {
        kill_group(pid);
    }
}

/// Drops the groups nothing runs in any more. Their ids get reused, by any process of this user, so
/// a group kept after its last process ended could later stop a stranger's.
fn forget_ended_groups() {
    GROUPS.lock().unwrap().retain(|pid| nix::sys::signal::killpg(nix::unistd::Pid::from_raw(*pid as i32), None).is_ok());
}

/// Background command logs and `cd` records: in the run folder, which goes away with the run; in a
/// per-process temporary folder when there is none, and in tests, where hubs come and go (and remove
/// their run folders) while other tests run commands.
fn scratch() -> std::io::Result<&'static PathBuf> {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| match crate::sockets::run_dir().filter(|_| !cfg!(test)) {
        Some(run) => run.join("bash"),
        None => std::env::temp_dir().join(format!("claudette-{}", std::process::id())),
    });
    std::fs::create_dir_all(dir)?;
    Ok(dir)
}

fn command(script: &str, ws: &Workspace) -> Command {
    // The shell's directory, unless it vanished; then the session root.
    let cwd = ws.cwd();
    let mut cmd = Command::new("bash");
    // First, what SessionStart, CwdChanged and FileChanged hooks wrote to `CLAUDE_ENV_FILE`.
    match ws.hooks() {
        Some(hooks) => cmd.arg("-c").arg(format!("{}{script}", hooks.env_source())),
        None => cmd.arg("-c").arg(script),
    };
    cmd.current_dir(if cwd.is_dir() { cwd } else { ws.root.clone() });
    cmd.envs(crate::settings::local_env().iter().map(|(k, v)| (k, v)));
    cmd.env("CLAUDECODE", "1").env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).process_group(0);
    cmd
}

fn run_in_background(script: &str, ws: &Workspace) -> ToolResult {
    let id = format!("bash_{}", COMMANDS.fetch_add(1, Ordering::Relaxed) + 1);
    let start = || -> std::io::Result<(PathBuf, u32)> {
        let file = scratch()?.join(format!("{id}.log"));
        let log = std::fs::File::options().create(true).append(true).open(&file)?;
        let child = command(script, ws).stdout(log.try_clone()?).stderr(log).spawn()?;
        Ok((file, child.id().unwrap_or(0)))
    };
    match start() {
        Ok((file, pid)) => {
            forget_ended_groups();
            GROUPS.lock().unwrap().insert(pid);
            ToolResult::ok(format!(
                "Command running in background with ID: {id}. Output is being written to: {}\nRead that file to check its output. Stop the command with: kill -- -{pid}",
                file.display()
            ))
        }
        Err(e) => ToolResult::fail(format!("Could not start bash for: {script}: {e}")),
    }
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    /// Characters dropped once `bytes` is full (UTF-8 lead bytes, so each character counts once).
    omitted: usize,
}

fn collect(mut pipe: impl AsyncRead + Unpin + Send + 'static, output: Arc<Mutex<Output>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            let mut out = output.lock().unwrap();
            let room = MAX_OUTPUT_BYTES.saturating_sub(out.bytes.len()).min(n);
            out.bytes.extend_from_slice(&chunk[..room]);
            out.omitted += chunk[room..n].iter().filter(|&&b| b & 0xC0 != 0x80).count();
        }
    })
}

async fn run_foreground(script: &str, timeout: u64, ws: &Workspace, cancel: &CancellationToken) -> ToolResult {
    let scratch = match scratch() {
        Ok(dir) => dir,
        Err(e) => return ToolResult::fail(format!("Could not create a scratch directory: {e}")),
    };
    let pwd_file = scratch.join(format!("pwd-{}", COMMANDS.fetch_add(1, Ordering::Relaxed) + 1));
    // stderr joins stdout so the output interleaves like a terminal; the trap records where `cd` left us.
    let wrapped = format!("exec 2>&1\ntrap 'pwd -P >| {}' EXIT\n{script}", shell_quote(&pwd_file.display().to_string()));
    let mut child = match command(&wrapped, ws).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(e) => return ToolResult::fail(format!("Could not run bash: {e}")),
    };
    let pid = child.id();
    if let Some(pid) = pid {
        forget_ended_groups();
        GROUPS.lock().unwrap().insert(pid);
    }
    let output = Arc::new(Mutex::new(Output::default()));
    let readers = [collect(child.stdout.take().expect("piped"), output.clone()), collect(child.stderr.take().expect("piped"), output.clone())];

    let mut interrupted = false;
    let mut timed_out = false;
    let status = tokio::select! {
        status = child.wait() => status,
        _ = tokio::time::sleep(Duration::from_millis(timeout)) => {
            timed_out = true;
            pid.inspect(|&p| kill_group(p));
            child.wait().await
        }
        _ = cancel.cancelled() => {
            interrupted = true;
            pid.inspect(|&p| kill_group(p));
            child.wait().await
        }
    };
    // A backgrounded child (`cmd &`) can hold the pipes open after the shell exits. Don't wait for it.
    let drained = async {
        for reader in &readers {
            while !reader.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(200), drained).await;
    for reader in &readers {
        reader.abort();
    }

    if let Ok(next) = std::fs::read_to_string(&pwd_file) {
        let next = PathBuf::from(next.trim());
        let old = ws.cwd();
        if !next.as_os_str().is_empty() && next.exists() && std::fs::canonicalize(&old).unwrap_or_else(|_| old.clone()) != next {
            ws.set_cwd(next.clone());
            if let Some(hooks) = ws.hooks() {
                hooks.cwd_changed(&old, &next).await;
            }
        }
        let _ = std::fs::remove_file(&pwd_file);
    }

    let status = match status {
        Ok(status) => status,
        Err(e) => return ToolResult::fail(format!("Could not run bash: {e}")),
    };
    let (mut text, omitted) = {
        let out = output.lock().unwrap();
        let decoded = String::from_utf8_lossy(&out.bytes);
        let kept: String = decoded.chars().take(MAX_OUTPUT).collect();
        let dropped = decoded.chars().count() - kept.chars().count();
        (kept.trim_end().to_owned(), out.omitted + dropped)
    };
    if omitted > 0 {
        text.push_str(&format!("\n\n... [{omitted} characters of output truncated] ..."));
    }
    let failure = if interrupted {
        Some("Command was interrupted".to_owned())
    } else if timed_out {
        Some(format!("Command timed out after {timeout}ms"))
    } else if !status.success() {
        Some(format!("Exit code {}", status.code().map_or("unknown (killed by a signal)".to_owned(), |c| c.to_string())))
    } else {
        None
    };
    match failure {
        Some(reason) => ToolResult::fail(format!("{text}\n{reason}").trim().to_owned()),
        None => ToolResult::ok(text),
    }
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "Bash",
        description: format!(
            "Executes a bash command on the user's local machine and returns its combined stdout and stderr.\n\n\
- The working directory persists between commands (cd carries over); environment variables and shell functions do not\n\
- Prefer absolute paths so commands do not depend on the current directory\n\
- Default timeout {DEFAULT_TIMEOUT}ms ({} minutes); maximum {MAX_TIMEOUT}ms ({} minutes)\n\
- Output beyond {MAX_OUTPUT} characters is truncated\n\
- Commands get no stdin; avoid interactive commands\n\
- Set run_in_background for long-running processes such as dev servers. You get an ID and an output file to Read; the process stops when the session ends\n\
- Prefer Read, Edit, Write, Glob and Grep over cat, sed, echo >, find and grep for file work\n\
- Chain dependent commands with &&; make independent calls in parallel in one response",
            DEFAULT_TIMEOUT / 60_000,
            MAX_TIMEOUT / 60_000
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command to execute" },
                "timeout": { "type": "number", "description": format!("Optional timeout in milliseconds (max {MAX_TIMEOUT})") },
                "description": { "type": "string", "description": "Clear, concise description of what this command does in 5-10 words" },
                "run_in_background": { "type": "boolean", "description": "Set to true to run this command in the background. Read its output file later." }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        annotations: None,
    }
}

pub async fn run(input: &Object, ws: &Workspace, cancel: &CancellationToken) -> ToolOutcome {
    let script = str_arg(input, "command")?;
    if opt_bool(input, "run_in_background")?.unwrap_or(false) {
        return Ok(run_in_background(script, ws));
    }
    let timeout = opt_num(input, "timeout")?.map_or(DEFAULT_TIMEOUT, |t| (t.max(0.0) as u64).clamp(1000, MAX_TIMEOUT));
    Ok(run_foreground(script, timeout, ws, cancel).await)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use serde_json::json;

    #[tokio::test]
    async fn cd_carries_over_and_failures_report_the_exit_code() {
        let (dir, ws) = workspace(&[("sub/x", "")]);
        let r = call(&ws, "Bash", json!({ "command": "cd sub && echo out && echo err >&2" })).await;
        assert_eq!(text(&r), "out\nerr");
        assert_eq!(text(&call(&ws, "Bash", json!({ "command": "pwd -P" })).await), dir.join("sub").display().to_string());
        let r = call(&ws, "Bash", json!({ "command": "echo partial; exit 3" })).await;
        assert!(r.is_error);
        assert_eq!(text(&r), "partial\nExit code 3");
    }

    #[tokio::test]
    async fn timeouts_kill_the_whole_group_and_background_output_goes_to_a_file() {
        let (_, ws) = workspace(&[]);
        let started = std::time::Instant::now();
        let r = call(&ws, "Bash", json!({ "command": "sleep 30 & sleep 30", "timeout": 1000 })).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(text(&r).ends_with("Command timed out after 1000ms"));
        let r = text(&call(&ws, "Bash", json!({ "command": "echo bg", "run_in_background": true })).await);
        assert!(r.starts_with("Command running in background with ID: bash_"));
        let file = r.split("Output is being written to: ").nth(1).unwrap().lines().next().unwrap().to_owned();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "bg\n");
    }

    #[tokio::test]
    async fn long_output_is_truncated_with_a_count() {
        let (_, ws) = workspace(&[]);
        let r = text(&call(&ws, "Bash", json!({ "command": "head -c 30010 /dev/zero | tr '\\0' a" })).await);
        assert!(r.ends_with("... [10 characters of output truncated] ..."), "{}", &r[r.len() - 60..]);
    }
}
