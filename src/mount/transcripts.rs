//! Claude Code's local transcript layout: where a project's sessions live on this machine.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::config::home;

/// Claude Code's limit before it shortens a project folder name and appends a hash.
const MAX_KEY: usize = 200;

/// Claude Code's 32-bit string hash (over UTF-16 code units), as it uses for long project folder names.
fn hash(text: &str) -> i32 {
    text.encode_utf16().fold(0i32, |h, c| h.wrapping_shl(5).wrapping_sub(h).wrapping_add(c as i32))
}

fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut digits = vec![];
    while n > 0 {
        digits.push(std::char::from_digit((n % 36) as u32, 36).unwrap());
        n /= 36;
    }
    digits.iter().rev().collect()
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The projects folder name Claude Code uses for a directory: its real path, non-alphanumerics as `-`.
pub fn project_key(dir: &Path) -> String {
    if env("CLAUDE_CONFIG_DIR").is_some()
        && let Some(name) = env("CLAUDE_CODE_PROJECT_DIR_NAME")
    {
        return name;
    }
    // A directory that is gone keeps its given name. macOS paths come back NFC from realpath.
    let path = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()).to_string_lossy().into_owned();
    let name: String = path.encode_utf16().map(|c| if c < 128 && (c as u8).is_ascii_alphanumeric() { c as u8 as char } else { '-' }).collect();
    if name.len() <= MAX_KEY {
        return name;
    }
    format!("{}-{}", &name[..MAX_KEY], base36(hash(&path).unsigned_abs() as u64))
}

pub fn projects_dir() -> PathBuf {
    env("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".claude")).join("projects")
}

/// The transcripts a run picks up, as paths in its sessions folder (`folder`): the one
/// `--resume <id>` names, or with `--continue` the latest. A bare `--resume` (the picker) or a
/// search term names none.
pub fn resumed(args: &[String], folder: &Path) -> Vec<String> {
    let is_id = |v: &str| v.len() == 36 && v.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    let mut out = vec![];
    for (i, arg) in args.iter().enumerate() {
        let id = match arg.as_str() {
            "--resume" | "-r" => args.get(i + 1).map(String::as_str),
            "--continue" | "-c" => {
                let latest = std::fs::read_dir(folder).into_iter().flatten().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl")).max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
                out.extend(latest.map(|e| format!("/{}", e.file_name().to_string_lossy())));
                continue;
            }
            other => other.strip_prefix("--resume="),
        };
        out.extend(id.filter(|v| is_id(v)).map(|v| format!("/{v}.jsonl")));
    }
    out
}

/// Where a remote transcript path lands in this machine's sessions folder, for a host that keeps
/// its own session store and asks for mirror frames (`--session-mirror`): its store expects the
/// path a local claude would have written. Only the part below the project folder is kept.
pub struct TranscriptMirror {
    dir: PathBuf,
}

impl TranscriptMirror {
    pub fn new(cwd: &Path) -> TranscriptMirror {
        TranscriptMirror { dir: projects_dir().join(project_key(cwd)) }
    }

    /// The local file for a remote transcript path, or None for a path outside a projects folder.
    pub fn local_path(&self, remote_file: &str) -> Option<PathBuf> {
        static BELOW_PROJECT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/projects/[^/]+/(.+)$").expect("valid pattern"));
        let normalized = remote_file.replace('\\', "/");
        let rest = BELOW_PROJECT.captures(&normalized)?.get(1)?.as_str();
        if rest.split('/').any(|part| part == "..") {
            return None;
        }
        Some(self.dir.join(rest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_paths_land_below_the_project_folder_and_never_outside_it() {
        let mirror = TranscriptMirror { dir: PathBuf::from("/local/projects/key") };
        assert_eq!(mirror.local_path("/home/remote/.claude/projects/-x/sess/subagents/a.jsonl"), Some(PathBuf::from("/local/projects/key/sess/subagents/a.jsonl")));
        assert_eq!(mirror.local_path("/home/remote/.claude/projects/-x/../../escape.jsonl"), None);
        assert_eq!(mirror.local_path("/tmp/elsewhere.jsonl"), None);
    }

    #[test]
    fn long_project_keys_are_shortened_with_claude_codes_hash() {
        assert_eq!(project_key(Path::new("/no/such/dir_x")), "-no-such-dir-x");
        // The reference value comes from Claude Code's own algorithm, run in JavaScript.
        let key = project_key(Path::new(&format!("/no/{}", "a".repeat(250))));
        assert_eq!(key.len(), MAX_KEY + 7);
        assert!(key.ends_with("aaaa-gcvt41"), "{key}");
    }
}
