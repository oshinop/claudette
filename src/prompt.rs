//! The local environment, appended to the system prompt in place of the remote machine's.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::config::home;
use crate::names::LOCAL_TOOLS;

async fn git(args: &[&str], cwd: &Path) -> String {
    let out = Command::new("git").args(args).current_dir(cwd).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().await;
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        _ => String::new(),
    }
}

async fn git_status(cwd: &Path) -> String {
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd).await;
    if branch.is_empty() {
        return String::new();
    }
    let (status, log) = tokio::join!(git(&["status", "--short"], cwd), git(&["log", "--oneline", "-5"], cwd));
    let short: Vec<&str> = status.lines().take(40).collect();
    let short = if short.is_empty() { "(clean)".to_owned() } else { short.join("\n") };
    format!(
        "\n\n# gitStatus\nThis is the git status of the working directory at the start of the conversation. It is a snapshot and does not update.\nCurrent branch: {branch}\n\nStatus:\n{short}\n\nRecent commits:\n{log}"
    )
}

/// The CLAUDE.md files a local claude would load for this directory: the user's, then each ancestor's, nearest last.
fn memory(cwd: &Path) -> String {
    let mut candidates: Vec<PathBuf> = vec![home().join(".claude").join("CLAUDE.md")];
    let dirs: Vec<&Path> = cwd.ancestors().collect();
    for dir in dirs.into_iter().rev() {
        candidates.extend([dir.join("CLAUDE.md"), dir.join(".claude").join("CLAUDE.md"), dir.join("CLAUDE.local.md")]);
    }
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    let home = home();
    for path in candidates {
        load_memory(&path, &home, 0, &mut seen, &mut found);
    }
    if found.is_empty() {
        return String::new();
    }
    format!(
        "\n\n# claudeMd\nCodebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.\n\n{}",
        found.join("\n\n")
    )
}

/// https://code.claude.com/docs/en/memory.md#import-additional-files specifies
/// file-relative and absolute imports, home paths, four import hops, escaped spaces,
/// and no imports in code spans or fenced blocks (or quoted paths).
/// Imports go depth-first, in mention order, before the importing file: the docs'
/// AGENTS.md example says imported instructions precede the rest of CLAUDE.md.
/// Canonical paths make aliases and cycles load only once across all memory roots.
/// Missing or unreadable files are skipped, just like absent memory roots; the
/// memory docs do not specify a separate missing-import diagnostic.
fn load_memory(path: &Path, home: &Path, depth: usize, seen: &mut std::collections::HashSet<PathBuf>, found: &mut Vec<String>) {
    let Ok(identity) = std::fs::canonicalize(path) else { return };
    if seen.contains(&identity) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    seen.insert(identity);
    if depth < 4 {
        for import in memory_imports(&text) {
            let imported = if let Some(relative) = import.strip_prefix("~/") {
                home.join(relative)
            } else {
                path.parent().unwrap_or(Path::new(".")).join(import)
            };
            load_memory(&imported, home, depth + 1, seen, found);
        }
    }
    found.push(format!("Contents of {}:\n\n{}", path.display(), text.trim()));
}

fn memory_imports(text: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut span = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let line_offset = offset;
        offset += line.len();
        let bytes = line.as_bytes();
        let indent = bytes.iter().take_while(|&&b| b == b' ').count();
        let trimmed = &bytes[indent..];
        if indent <= 3 && span == 0 && matches!(trimmed.first(), Some(b'`' | b'~')) {
            let marker = trimmed[0];
            let count = trimmed.iter().take_while(|&&b| b == marker).count();
            if let Some((open, width)) = fence {
                if marker == open && count >= width && trimmed[count..].iter().all(u8::is_ascii_whitespace) {
                    fence = None;
                }
                continue;
            }
            if count >= 3 {
                fence = Some((marker, count));
                continue;
            }
        }
        if fence.is_some() {
            continue;
        }
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'`' {
                let count = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                if span == count {
                    span = 0;
                } else if span == 0 {
                    // An unmatched backtick is literal Markdown, not a code span.
                    let rest = &text.as_bytes()[line_offset + i + count..];
                    let mut end = 0;
                    let mut closes = false;
                    while end < rest.len() {
                        if rest[end] == b'`' {
                            let width = rest[end..].iter().take_while(|&&b| b == b'`').count();
                            if width == count {
                                closes = true;
                                break;
                            }
                            end += width;
                        } else {
                            end += 1;
                        }
                    }
                    if closes
                    {
                        span = count;
                    }
                }
                i += count;
            } else if span == 0 && bytes[i] == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace() || matches!(bytes[i - 1], b'(' | b'[')) {
                i += 1;
                let start = i;
                let mut path = String::new();
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'"' | b'\'' | b'`') {
                    if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b' ') {
                        path.push(' ');
                        i += 2;
                    } else {
                        let Some(ch) = line[i..].chars().next() else { break };
                        path.push(ch);
                        i += ch.len_utf8();
                    }
                }
                if i > start && !path.is_empty() {
                    imports.push(path);
                }
            } else {
                i += 1;
            }
        }
    }
    imports
}

fn os_version() -> String {
    let u = nix::sys::utsname::uname();
    match u {
        Ok(u) => format!("{} {}", u.sysname().to_string_lossy(), u.release().to_string_lossy()),
        Err(_) => std::env::consts::OS.to_owned(),
    }
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// Appended to the system prompt. The remote claude starts with `excludeDynamicSections`, which keeps
/// its own machine's working directory, git status and memory out of the system prompt; these are
/// the local ones in their place.
pub async fn environment_prompt(cwd: PathBuf, add_dirs: Vec<PathBuf>) -> String {
    let status = git_status(&cwd).await;
    let claude_md = memory(&cwd);
    let tools = format!("{} and {}", LOCAL_TOOLS[..LOCAL_TOOLS.len() - 1].join(", "), LOCAL_TOOLS[LOCAL_TOOLS.len() - 1]);
    let extra = if add_dirs.is_empty() {
        String::new()
    } else {
        format!("\nAdditional working directories: {}", add_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", "))
    };
    format!(
        "# Environment (claudette)\nThis session runs through claudette: you run on a remote machine, while your {tools} tools run on the user's machine. Files, paths and commands refer to the user's machine. Any other working directory you are told about belongs to the remote machine; ignore it and use this one.\n<env>\nWorking directory: {}{extra}\nIs directory a git repo: {}\nPlatform: {}\nOS Version: {}\nShell: bash\n</env>{status}{claude_md}",
        cwd.display(),
        if status.is_empty() { "No" } else { "Yes" },
        platform(),
        os_version()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct MemoryFixture(PathBuf);

    impl MemoryFixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!("claudette-memory-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, text: &str) {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn load(&self) -> String {
            let mut found = Vec::new();
            load_memory(&self.0.join("CLAUDE.md"), &self.0, 0, &mut std::collections::HashSet::new(), &mut found);
            found.join("\n\n")
        }
    }

    impl Drop for MemoryFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn relative_and_nested_imports_precede_their_parents() {
        let f = MemoryFixture::new();
        f.write("CLAUDE.md", "@rules/first.md\nROOT_MARKER");
        f.write("rules/first.md", "@second.md\nFIRST_MARKER");
        f.write("rules/second.md", "SECOND_MARKER");
        let text = f.load();
        assert!(text.find("SECOND_MARKER").unwrap() < text.find("FIRST_MARKER").unwrap());
        assert!(text.find("FIRST_MARKER").unwrap() < text.find("ROOT_MARKER").unwrap());
    }

    #[test]
    fn home_and_absolute_imports_load_only_once() {
        let f = MemoryFixture::new();
        f.write("CLAUDE.md", &format!("@~/shared.md\n@{}", f.0.join("shared.md").display()));
        f.write("shared.md", "UNIQUE_MARKER");
        assert_eq!(f.load().matches("UNIQUE_MARKER").count(), 1);
    }

    #[test]
    fn cycles_and_path_aliases_do_not_repeat_content() {
        let f = MemoryFixture::new();
        f.write("CLAUDE.md", "@other.md\n@./other.md\nROOT_MARKER");
        f.write("other.md", "@CLAUDE.md\nOTHER_MARKER");
        let text = f.load();
        assert_eq!(text.matches("ROOT_MARKER").count(), 1);
        assert_eq!(text.matches("OTHER_MARKER").count(), 1);
    }

    #[test]
    fn imports_stop_after_four_hops() {
        let f = MemoryFixture::new();
        f.write("CLAUDE.md", "@1.md");
        for n in 1..=5 {
            f.write(&format!("{n}.md"), &format!("@{}.md\nDEPTH_{n}", n + 1));
        }
        let text = f.load();
        assert!(text.contains("DEPTH_4"));
        assert!(!text.contains("DEPTH_5"));
    }

    #[test]
    fn code_spans_and_fences_are_literal() {
        let text = "`@one` ``@two ` inside``\n```md\n@three\n```\n~~~\n@four\n~~~\n@real";
        assert_eq!(memory_imports(text), ["real"]);
        assert_eq!(memory_imports("`multiline\n@hidden`\n@visible"), ["visible"]);
    }

    #[test]
    fn escaped_spaces_are_allowed_but_quoted_paths_are_not() {
        assert_eq!(memory_imports("@Design\\ Docs/guide.md @plain name.md @\"quoted.md\" @'other.md'"), ["Design Docs/guide.md", "plain"]);
    }

    #[test]
    fn missing_import_does_not_hide_the_importing_file() {
        let f = MemoryFixture::new();
        f.write("CLAUDE.md", "@missing.md\nROOT_MARKER");
        let text = f.load();
        assert!(text.contains("ROOT_MARKER"));
        assert_eq!(text.matches("Contents of ").count(), 1);
    }
}
