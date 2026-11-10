//! Where a local project lives on the remote machine, and how its paths map back.
//!
//! Each local project gets one remote folder, `~/.claudette/ws/<machine>/<name>-<hash>`:
//! `<machine>` is derived from this machine's OS identity, never stored; `<hash>` is of the
//! project's canonical path. Renaming the Mac or changing networks moves nothing, two local machines
//! never share a folder, and `my-app` and `my_app` stay apart even after Claude Code turns both
//! into the same project key.

use std::borrow::Cow;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::json::json_text;
use crate::mount::warm::PathParts;
use crate::sockets::Forwards;
use crate::ssh::{Script, shell_quote};
use crate::userconfig::Delivery;

/// The folder under the remote home that holds every workspace.
pub const ROOT: &str = ".claudette/ws";

fn hex(bytes: &[u8], chars: usize) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()[..chars].to_owned()
}

/// The OS's stable identity for this machine: the hardware UUID on macOS, machine-id on Linux.
fn os_machine_id() -> Option<String> {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("/usr/sbin/ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().find(|l| l.contains("\"IOPlatformUUID\""))?;
        return line.rsplit('"').nth(1).map(str::to_owned);
    }
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|f| std::fs::read_to_string(f).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// 12 hex digits of a hash of the machine's identity; the raw identifier never leaves this machine.
pub fn machine() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let id = os_machine_id().unwrap_or_else(|| nix::unistd::gethostname().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default());
        hex(&Sha256::digest(format!("claudette:{id}")), 12)
    })
}

/// The path that names a project however it was reached: symlinks resolved, the disk's own
/// capitalization (realpath gives both).
fn canonical(cwd: &Path) -> String {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()).to_string_lossy().into_owned()
}

/// The remote folder for a local directory, relative to the remote home.
pub fn remote_workdir(cwd: &Path) -> String {
    let path = canonical(cwd);
    let name: String = Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect())
        .filter(|n: &String| !n.is_empty())
        .unwrap_or_else(|| "root".into());
    format!("{ROOT}/{}/{name}-{}", machine(), hex(&Sha256::digest(path.as_bytes()), 8))
}

fn enter_lines(cwd: &Path) -> String {
    let dir = format!("\"$HOME\"/{}", shell_quote(&remote_workdir(cwd)));
    format!("mkdir -p {dir} || exit 1\ncd {dir} || exit 1")
}

/// Creates the remote folder and runs claude in it; for claude's own commands (`auth`, `mcp`, …).
pub fn enter(cwd: &Path) -> Script {
    Script { setup: enter_lines(cwd), cleanup: String::new() }
}

/// The sandbox for macOS (sandbox-exec): the home folder is closed except the workspace, Claude
/// Code's own state, its installation, and the Keychain its login lives in.
const MACOS_PROFILE: &str = r#"(version 1)
(allow default)
(deny file-read* file-write* (subpath (param "HOME")))
(allow file-read-metadata (subpath (param "HOME")))
(allow file-read* file-write* (subpath (param "WORK")) (subpath (string-append (param "HOME") "/.claude")) (prefix (string-append (param "HOME") "/.claude.json")) (subpath (string-append (param "HOME") "/Library/Keychains")) (subpath (string-append (param "HOME") "/Library/Caches")))
(allow file-read* (subpath (string-append (param "HOME") "/.local")) (subpath (string-append (param "HOME") "/Library/Preferences")) (subpath (param "HELPER")))"#;

/// The notice for a mount that ended while claude still runs, quoted for the remote shell.
const MOUNT_ENDED: &str = "'claudette: a mount of this machine ended on the remote machine (sshfs exited); claude can no longer read the project or save its transcript there'";

/// The remote side of the virtual workspace. The project, served over SFTP on the forwarded socket
/// `sftp`, is mounted with `sshfs -o passive`, and claude runs in it, always confined to it:
///
/// - with bubblewrap (Linux), each session mounts privately and sees its mount at the stable folder,
///   so two sessions in one project never share a mount, and the project key stays the same; the
///   rest of the home folder is hidden.
/// - with sandbox-exec (macOS), the mount is the stable folder itself, or a sibling of it while
///   another session holds it; the rest of the home folder is closed.
///
/// A remote machine with neither refuses the session. The run's forwarded sockets are removed at
/// the end, with the mounts; its files (claude's MCP configuration, this machine's settings, each
/// mount's warm start) come with the run's first frames, readable by the remote user only, and are
/// removed too. `user`, the socket of the user's skills, agents, commands and output styles, and
/// `plugins`, the socket of the session's plugins (at [`plugins_dir`]), are mounted read-only.
///
/// claude's Claude Code folder (`CLAUDE_CONFIG_DIR`) is the run's own: `__helper config` serves it
/// (see [`crate::helper::config`]), with this machine's settings, keybindings and user content
/// over the remote user's state and login, which stay shared. Its caches are off: two sessions
/// lock and rewrite `.claude.json` through their own mounts, and a cached listing or file would let
/// one miss the other's lock or overwrite its change.
///
/// `sessions`, the socket of this project's sessions folder here, is mounted inside it as claude's
/// sessions folder for the workspace (`projects/<workspace>`). Sessions the remote machine kept for
/// the workspace in its own `~/.claude/projects` move into it, so they end up on this machine too; a
/// move that fails is tried again next time.
///
/// claude's inbox for messages from this machine's other sessions goes in the run's own folder
/// (`$T.inbox`, through `CLAUDE_CODE_TMPDIR`), which only this run knows; it is removed at the end.
///
/// The sshfs connections end at the run's helper on this machine's side, which keeps them through a
/// dropped link (see [`crate::helper`]). A mount that still ends (sshfs itself exits) is reported on
/// claude's stderr instead of leaving claude reading and writing nothing without a word. The
/// sessions folder's near end writes transcripts behind; the session ends once the laptop has
/// confirmed the last of them (the near end's held warm start, `$T.sessions.warm.held`, goes then).
pub fn mount(cwd: &Path, forwards: &Forwards, sftp: &str, user: Option<&str>, plugins: Option<&str>, sessions: &str, config: &Delivery) -> Script {
    let work = format!("\"$HOME\"/{}", shell_quote(&remote_workdir(cwd)));
    // /tmp's real path (/private/tmp on macOS), as `mount` reports mount points.
    let temp = run_prefix(&forwards.run);
    let mut setup = vec![
        format!("W={work}"),
        format!("T={temp}"),
        "MOUNTS=; WATCH=".to_owned(),
        "command -v bwrap >/dev/null 2>&1 || command -v sandbox-exec >/dev/null 2>&1 || { echo 'claudette: this machine needs bubblewrap (bwrap) to confine claude to the workspace' >&2; exit 1; }".to_owned(),
        "[ -n \"${CLAUDETTE_HELPER:-}\" ] || { echo 'claudette: no helper to serve the session its Claude Code folder' >&2; exit 1; }".to_owned(),
        "mkdir -p \"$W\" \"$HOME/.claude\" || exit 1".to_owned(),
        "mkdir -m 700 \"$T.inbox\" && export CLAUDE_CODE_TMPDIR=\"$T.inbox\" || exit 1".to_owned(),
    ];
    // This machine's files for the Claude Code folder.
    setup.push("mkdir -m 700 \"$T.laptop\" \"$T.config\" || exit 1".to_owned());
    for (file, name) in [(&config.settings, "settings.json"), (&config.keybindings, "keybindings.json")] {
        if let Some(file) = file {
            setup.push(format!("mv {} \"$T.laptop/{name}\" || exit 1", shell_quote(file)));
        }
    }
    setup.extend([
        "if command -v bwrap >/dev/null 2>&1 || mount | grep -qF \" on $W \"; then M=$(mktemp -d \"$W.XXXXXX\") || exit 1; else M=\"$W\"; fi".to_owned(),
        // Each mount of this machine goes through its near end (see [`crate::helper::near`]), which
        // starts warm and keeps what it learns for a few seconds; sshfs's own cache would only add to
        // that. Names (and absent names) stay cached in the kernel for 10 s, attributes for FUSE's 1 s.
        // `cache=no`, the name sshfs 2 and 3 both take (3 also calls it `dir_cache`).
        "O=passive,uid=$(id -u),gid=$(id -g),cache=no,entry_timeout=10,negative_timeout=10".to_owned(),
        "CO=passive,uid=$(id -u),gid=$(id -g),cache=no,entry_timeout=0,negative_timeout=0,attr_timeout=0".to_owned(),
        "[ \"$(uname)\" = Darwin ] && O=\"$O,noappledouble,volname=claudette\" && CO=\"$CO,nolocalcaches,noappledouble,volname=claudette-config\"".to_owned(),
    ]);
    // claude's sessions folder for the folder it runs in (the workspace under bubblewrap, the mount
    // under sandbox-exec), named as claude names it; the remote user's own folder for it, from before,
    // stays as the empty name the Claude Code folder lists.
    setup.extend([
        "if command -v bwrap >/dev/null 2>&1; then C=\"$W\"; else C=\"$M\"; fi".to_owned(),
        "K=\"$(cd \"$C\" && pwd -P | sed 's/[^a-zA-Z0-9]/-/g')\"; L=\"$HOME/.claude/projects/$K\"".to_owned(),
        "if [ -n \"$(ls -A \"$L\" 2>/dev/null)\" ] && ! mount | grep -qF \" on $L \"; then mv \"$L\" \"$(mktemp -u \"$L.moved.XXXXXX\")\" || exit 1; fi".to_owned(),
        "mkdir -p \"$L\" && mkdir -m 700 \"$T.sessions\" || exit 1".to_owned(),
    ]);
    let (sshfs, wait) = (sshfs_lines, wait_line);
    let near = |socket: &str, name: &str| {
        let writeback = if name == "sessions" { " --writeback" } else { "" };
        format!("\"$CLAUDETTE_HELPER\" __helper sftp {} \"$T.{name}.warm\"{writeback}", shell_quote(socket))
    };
    let helper = format!(
        "\"$CLAUDETTE_HELPER\" __helper config \"$HOME/.claude\" \"$HOME\" \"$T.laptop\" {} \"$K\" \"$T.sessions\" \"$T.user/.claude\" 2>\"$T.config.log\"",
        shell_quote(&config.sink)
    );
    // All at once: the workspace, the sessions folder, the user content, the plugins, and the Claude
    // Code folder, whose server only looks into the others once claude asks it to.
    let mut mounts: Vec<(&str, &str)> = vec![("\"$M\"", "\"$T"), ("\"$T.sessions\"", "\"$T.sessions"), ("\"$T.config\"", "\"$T.config")];
    setup.extend(sshfs(near(sftp, "project"), "\"$M\"", "\"$T", "$O"));
    setup.extend(sshfs(near(sessions, "sessions"), "\"$T.sessions\"", "\"$T.sessions", "$O"));
    setup.extend(sshfs(helper, "\"$T.config\"", "\"$T.config", "$CO"));
    if let Some(user) = user {
        setup.push("mkdir -m 700 \"$T.user\" || exit 1".to_owned());
        setup.extend(sshfs(near(user, "user"), "\"$T.user\"", "\"$T.user", "$O,ro"));
        mounts.push(("\"$T.user\"", "\"$T.user"));
    }
    if let Some(plugins) = plugins {
        setup.push("mkdir -m 700 \"$T.plugins\" || exit 1".to_owned());
        setup.extend(sshfs(near(plugins, "plugins"), "\"$T.plugins\"", "\"$T.plugins", "$O,ro"));
        mounts.push(("\"$T.plugins\"", "\"$T.plugins"));
    }
    setup.extend(mounts.iter().map(|(point, prefix)| wait(point, prefix)));
    setup.push("for d in \"$L\".moved.*; do [ -d \"$d\" ] && cp -Rn \"$d\"/. \"$T.sessions\"/ && rm -rf \"$d\"; done".to_owned());
    setup.push(watch_line());
    // The login stays where it is (the Keychain entry or ~/.claude/.credentials.json), shared.
    setup.push("export CLAUDE_CONFIG_DIR=\"$T.config\" CLAUDE_SECURESTORAGE_CONFIG_DIR=".to_owned());
    // bwrap names claude's process in "$T.bwrap": bwrap itself ends at once on a TERM, while claude
    // takes a moment to finish writing, so the cleanup waits for claude, not bwrap.
    setup.extend(["if command -v bwrap >/dev/null 2>&1; then".to_owned(), "  exec 9>\"$T.bwrap\"; set -- bwrap --info-fd 9 --dev-bind / /".to_owned()]);
    setup.extend(
        [
            "  set -- \"$@\" --tmpfs \"$HOME\" --bind \"$HOME/.claude\" \"$HOME/.claude\"",
            "  [ -e \"$HOME/.claude.json\" ] && set -- \"$@\" --bind \"$HOME/.claude.json\" \"$HOME/.claude.json\"",
            "  [ -d \"$HOME/.local\" ] && set -- \"$@\" --ro-bind \"$HOME/.local\" \"$HOME/.local\"",
            // The helper, for claude to start the inbox relay with.
            "  set -- \"$@\" --ro-bind \"${CLAUDETTE_HELPER%/*}\" \"${CLAUDETTE_HELPER%/*}\"",
            "  set -- \"$@\" --bind \"$M\" \"$W\" --chdir \"$W\" --",
            "else",
        ]
        .map(str::to_owned),
    );
    setup.push(format!("  printf '%s\\n' {} >\"$T.sb\" || exit 1", shell_quote(MACOS_PROFILE)));
    setup.push("  set -- sandbox-exec -D HOME=\"$HOME\" -D WORK=\"$M\" -D HELPER=\"${CLAUDETTE_HELPER%/*}\" -f \"$T.sb\"; cd \"$M\" || exit 1".to_owned());
    setup.push("fi".to_owned());

    let unmount = unmount_line;
    let mut steps = vec![
        "touch \"$T.ending\"; kill $WATCH 2>/dev/null".to_owned(),
        // claude, under bwrap, may still be writing to the mounts: up to 5 s, when the holder gives up.
        // An orphan the container's init does not reap stays a zombie, which `kill -0` still finds.
        "C=$(sed -n 's/.*\"child-pid\": *\\([0-9]*\\).*/\\1/p' \"$T.bwrap\" 2>/dev/null); i=0; while [ -n \"$C\" ] && kill -0 \"$C\" 2>/dev/null && ! { ps -o stat= -p \"$C\" 2>/dev/null | grep -q Z; } && [ $i -lt 100 ]; do i=$((i+1)); sleep 0.05; done".to_owned(),
        format!("[ -e \"$T.unmounted\" ] && echo {MOUNT_ENDED} >&2"),
        "cd /".to_owned(),
        unmount("\"$M\""),
        "[ \"$M\" = \"$W\" ] || rmdir \"$M\" 2>/dev/null".to_owned(),
        unmount("\"$T.config\""),
        "rmdir \"$T.config\" 2>/dev/null".to_owned(),
        unmount("\"$T.sessions\""),
        "rmdir \"$T.sessions\" 2>/dev/null".to_owned(),
        // A little longer than the near end waits for the laptop's answers.
        "i=0; while [ -e \"$T.sessions.warm.held\" ] && [ $i -lt 220 ]; do i=$((i+1)); sleep 0.05; done".to_owned(),
    ];
    if user.is_some() {
        steps.push(unmount("\"$T.user\""));
        steps.push("rmdir \"$T.user\" 2>/dev/null".to_owned());
    }
    if plugins.is_some() {
        steps.push(unmount("\"$T.plugins\""));
        steps.push("rmdir \"$T.plugins\" 2>/dev/null".to_owned());
    }
    let mut remove: Vec<String> = ["\"$T", "\"$T.user", "\"$T.plugins", "\"$T.config", "\"$T.sessions"].iter().flat_map(|prefix| [format!("{prefix}.fifo\""), format!("{prefix}.err\"")]).collect();
    remove.extend(["\"$T.config.log\"", "\"$T.sb\"", "\"$T.bwrap\"", "\"$T.ending\"", "\"$T.unmounted\""].map(str::to_owned));
    remove.extend(forwards.list.iter().map(|f| shell_quote(&f.remote)).chain(forwards.files.iter().map(|f| shell_quote(&f.remote))));
    // A near end that was stopped leaves its warm start held.
    remove.extend(forwards.files.iter().filter(|f| f.name.ends_with(".warm")).map(|f| shell_quote(&format!("{}.held", f.remote))));
    steps.push(format!("rm -f {}", remove.join(" ")));
    steps.push("rm -rf \"$T.inbox\" \"$T.laptop\"".to_owned());
    Script { setup: setup.join("\n"), cleanup: steps.join("; ") }
}

/// Mounts `source`'s SFTP server at `point` (`prefix` names its fifo and error file) in the
/// background, adding sshfs to `$MOUNTS`.
fn sshfs_lines(source: String, point: &str, prefix: &str, options: &str) -> Vec<String> {
    vec![
        format!("mkfifo {prefix}.fifo\" || exit 1"),
        format!("{source} <{prefix}.fifo\" | sshfs -f -o \"{options}\" :/ {point} >{prefix}.fifo\" 2>{prefix}.err\" & MOUNTS=\"$MOUNTS $!\""),
    ]
}

/// Waits for a mount to appear, for 15 s. A mount takes one sshfs handshake, a few milliseconds on a
/// fast link, so it is looked for every 20 ms: every session start waits on each mount in turn.
fn wait_line(point: &str, prefix: &str) -> String {
    format!(
        "i=0; until mount | grep -qF \" on {inner} \"; do i=$((i+1)); if [ $i -gt 750 ]; then echo \"claudette: could not mount the workspace here: $(cat {prefix}.err\" 2>/dev/null)\" >&2; exit 1; fi; sleep 0.02; done",
        inner = point.trim_matches('"')
    )
}

/// Watches the mounts in `$MOUNTS` every 2 s and reports one that ended (sshfs exited) unless the
/// session is ending. On a terminal, claude's interface owns the screen: the notice waits for the end
/// of the session. The sleep runs in the background so the TERM that ends the watcher ends it too: a
/// sleep left behind would hold the session's stderr open, and the session with it, until it ran out.
fn watch_line() -> String {
    format!(
        "( trap 'kill $S 2>/dev/null; exit 0' TERM; while :; do sleep 2 & S=$!; wait $S; for m in $MOUNTS; do kill -0 \"$m\" 2>/dev/null || {{ [ -e \"$T.ending\" ] || {{ if [ -t 2 ]; then : >\"$T.unmounted\"; else echo {MOUNT_ENDED} >&2; fi; }}; exit 0; }}; done; done ) >/dev/null & WATCH=$!"
    )
}

fn unmount_line(point: &str) -> String {
    format!("fusermount3 -u -z {point} 2>/dev/null || fusermount -u -z {point} 2>/dev/null || umount {point} 2>/dev/null || diskutil unmount force {point} >/dev/null 2>&1")
}

/// Where `claude plugin …` mounts this machine's plugin store on the remote machine, as a plain path.
pub fn store_dir(run: &str) -> String {
    format!("/tmp/claudette-{run}.store")
}

/// For `claude plugin …`: mounts this machine's plugin store (served on the forwarded socket
/// `store`, see [`crate::mount::store`]) at [`store_dir`], with no caching, as the Claude Code
/// folder, and runs claude in its project folder. Unmounted and removed at the end.
pub fn store(forwards: &Forwards, store: &str, nc: &str) -> Script {
    let dir = shell_quote(&store_dir(&forwards.run));
    let mut setup = vec![
        format!("T={}", run_prefix(&forwards.run)),
        "MOUNTS=".to_owned(),
        "mkdir -m 700 \"$T.store\" || exit 1".to_owned(),
        "O=passive,uid=$(id -u),gid=$(id -g),cache=no,entry_timeout=0,negative_timeout=0,attr_timeout=0".to_owned(),
        "[ \"$(uname)\" = Darwin ] && O=\"$O,nolocalcaches,noappledouble,volname=claudette-plugins\"".to_owned(),
    ];
    setup.extend(sshfs_lines(format!("{} -U {}", shell_quote(nc), shell_quote(store)), "\"$T.store\"", "\"$T.store", "$O"));
    setup.push(wait_line("\"$T.store\"", "\"$T.store"));
    setup.push(format!("export CLAUDE_CONFIG_DIR={dir}/config CLAUDE_SECURESTORAGE_CONFIG_DIR="));
    setup.push(format!("cd {dir}/project || exit 1"));
    let mut remove = vec!["\"$T.store.fifo\"".to_owned(), "\"$T.store.err\"".to_owned()];
    remove.extend(forwards.list.iter().map(|f| shell_quote(&f.remote)).chain(forwards.files.iter().map(|f| shell_quote(&f.remote))));
    let cleanup = ["cd /".to_owned(), unmount_line("\"$T.store\""), "rmdir \"$T.store\" 2>/dev/null".to_owned(), format!("rm -f {}", remove.join(" "))];
    Script { setup: setup.join("\n"), cleanup: cleanup.join("; ") }
}

/// The run's files on the remote machine start with this, in /tmp's real path.
fn run_prefix(run: &str) -> String {
    format!("\"$(cd /tmp && pwd -P)\"/{}", shell_quote(&format!("claudette-{run}")))
}

/// Where the run's plugins are mounted on the remote machine, as a plain path (`/tmp` may be a
/// symlink to the real folder `mount` reports, as on macOS).
pub fn plugins_dir(run: &str) -> String {
    format!("/tmp/claudette-{run}.plugins")
}

/// Turns the remote folder back into the local directory inside serialized messages, the sessions
/// folder for it (`<config>/projects/-home-me--claudette-ws-…`) into this machine's, and the run's
/// Claude Code folder (`/tmp/claudette-<run>.config`) and mounted user content
/// (`/tmp/claudette-<run>.user/.claude`) into this machine's `~/.claude`.
pub struct PathMap {
    remote: Regex,
    local: String,
    sessions: Regex,
    sessions_local: String,
    user: Regex,
    user_local: String,
}

impl PathMap {
    pub fn new(cwd: &Path) -> PathMap {
        PathMap::from_parts(&PathMap::parts(cwd))
    }

    /// What the map is made of: the remote folder, and this machine's folders as JSON text. The
    /// remote near cache makes the same map from them (see [`crate::helper::near`]).
    pub fn parts(cwd: &Path) -> PathParts {
        let sessions_local = crate::mount::transcripts::projects_dir().join(crate::mount::transcripts::project_key(cwd));
        PathParts {
            workdir: remote_workdir(cwd),
            local: json_text(&cwd.to_string_lossy()),
            sessions_local: json_text(&sessions_local.to_string_lossy()),
            user_local: json_text(&crate::settings::user_dir().to_string_lossy()),
        }
    }

    pub fn from_parts(parts: &PathParts) -> PathMap {
        // Wherever the remote home is: `/home/me/.claudette/ws/<machine>/<name>-<hash>`.
        let folder = regex::escape(&json_text(&parts.workdir));
        // The character after the folder is kept by the replacement; Rust's regex has no lookahead.
        // A session's own mount, beside the folder, is `<folder>.XXXXXX`.
        let remote = Regex::new(&format!(r#"(?:/[^"\\/\s]+)*/{folder}(?:\.[A-Za-z0-9]{{6}})?(["/\\\s]|$)"#)).expect("valid pattern");
        // claude names the sessions folder after its folder's real path, every other character a `-`.
        let key: String = parts.workdir.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let sessions = Regex::new(&format!(r#"(?:/[^"\\/\s]+)*/projects/[A-Za-z0-9-]*-{}(?:-[A-Za-z0-9]{{6}})?(["/\\\s]|$)"#, regex::escape(&key))).expect("valid pattern");
        let user = Regex::new(r#"(?:/private)?/tmp/claudette-[0-9a-f]{16}\.(?:user/\.claude|config)(["/\\\s]|$)"#).expect("valid pattern");
        PathMap {
            remote,
            local: parts.local.clone(),
            sessions,
            sessions_local: parts.sessions_local.clone(),
            user,
            user_local: parts.user_local.clone(),
        }
    }

    pub fn map<'a>(&self, json: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(json);
        if out.contains("/.claudette/ws/") {
            out = Cow::Owned(self.remote.replace_all(&out, |caps: &regex::Captures| format!("{}{}", self.local, &caps[1])).into_owned());
        }
        if out.contains("-claudette-ws-") {
            out = Cow::Owned(self.sessions.replace_all(&out, |caps: &regex::Captures| format!("{}{}", self.sessions_local, &caps[1])).into_owned());
        }
        if out.contains(".user/.claude") || out.contains(".config") {
            out = Cow::Owned(self.user.replace_all(&out, |caps: &regex::Captures| format!("{}{}", self.user_local, &caps[1])).into_owned());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopping_the_mount_watcher_lets_the_session_end_at_once() {
        // The session's stderr is a pipe ssh waits on; nothing of the watcher may keep it open.
        let script = format!("MOUNTS=$$; T=/nonexistent/t\n{}\nsleep 0.2; kill $WATCH", watch_line());
        let started = std::time::Instant::now();
        let out = std::process::Command::new("sh").arg("-c").arg(&script).output().expect("sh runs");
        assert!(out.status.success());
        assert!(started.elapsed() < std::time::Duration::from_millis(1500), "the session's stderr stayed open for {:?}", started.elapsed());
    }

    #[test]
    fn distinct_projects_get_distinct_folders_and_one_project_one_folder() {
        let base = std::env::temp_dir().join(format!("ct-workdir-{}", std::process::id()));
        let (dash, under) = (base.join("my-app"), base.join("my_app"));
        std::fs::create_dir_all(&dash).unwrap();
        std::fs::create_dir_all(&under).unwrap();
        let link = base.join("link");
        let _ = std::os::unix::fs::symlink(&dash, &link);
        assert_ne!(remote_workdir(&dash), remote_workdir(&under));
        assert_eq!(remote_workdir(&dash), remote_workdir(&link));
        assert!(remote_workdir(&dash).starts_with(&format!("{ROOT}/{}/my-app-", machine())));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn remote_paths_map_to_the_local_directory_and_nothing_else_does() {
        let cwd = Path::new("/Users/me/app");
        let map = PathMap::new(cwd);
        let remote = format!("/home/remote/{}", remote_workdir(cwd));
        let json = format!(r#"{{"cwd":"{remote}","file":"{remote}/src/a.rs","other":"{remote}x/b"}}"#);
        assert_eq!(map.map(&json), format!(r#"{{"cwd":"/Users/me/app","file":"/Users/me/app/src/a.rs","other":"{remote}x/b"}}"#));
        assert_eq!(map.map(&format!("cd {remote} && ls")), "cd /Users/me/app && ls");
        // The remote sessions folder for the workspace, and for a session's mount beside it.
        let key: String = remote.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let local = crate::mount::transcripts::projects_dir().join(crate::mount::transcripts::project_key(cwd));
        let config = "/tmp/claudette-0123456789abcdef.config";
        let hook = format!(r#"{{"transcript_path":"{config}/projects/{key}/s.jsonl","other":"{config}/projects/{key}-AbC123/s.jsonl","not":"/home/remote/.claude/projects/{key}x/s.jsonl"}}"#);
        assert_eq!(
            map.map(&hook),
            format!(r#"{{"transcript_path":"{0}/s.jsonl","other":"{0}/s.jsonl","not":"/home/remote/.claude/projects/{key}x/s.jsonl"}}"#, local.display())
        );
        // The rest of the run's Claude Code folder is this machine's ~/.claude.
        let user = crate::settings::user_dir();
        assert_eq!(map.map(&format!(r#"{{"file":"/private{config}/settings.json"}}"#)), format!(r#"{{"file":"{}/settings.json"}}"#, user.display()));
    }
}
