//! `__helper config`: the remote claude's Claude Code folder (`CLAUDE_CONFIG_DIR`), served over SFTP on
//! stdio for `sshfs -o passive` to mount. One folder over several:
//!
//! - **this machine's** user configuration: `settings.json` and `keybindings.json`, copies the laptop
//!   sent at start, and its skills, agents, commands and output styles, from the read-only
//!   user-content mount. A save of either file is also sent back to the laptop, which writes it into
//!   its own `~/.claude` (see [`crate::userconfig`]). So each laptop's sessions use its own
//!   configuration, and the remote machine's own `~/.claude/settings.json` is never involved.
//! - the workspace's sessions folder, `projects/<key>`: this machine's, from the sessions mount.
//! - the remote user's **global state**, `~/.claude.json` (onboarding, folder trust, the account).
//! - everything else from the remote user's real **`~/.claude`**: history, sessions, plugins, caches.
//!
//! Login and state stay shared with every laptop and with claude run on the remote machine itself;
//! the login is not in this folder at all (`CLAUDE_SECURESTORAGE_CONFIG_DIR` keeps it in its usual
//! place). Claude Code saves by writing `<file>.tmp.<pid>.<hex>` and renaming it over, and locks with
//! `<file>.lock`: those go wherever `<file>` goes, so a save stays on one side. It refuses to write
//! through a symlink, which is why this is a folder of real files rather than links into the others.

use std::collections::HashMap;
use std::fs::{File as FsFile, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version};

use crate::mount::sftp::{io_status, next_batch, ok};

/// The laptop's files, saved back to it.
pub const LAPTOP_FILES: [&str; 2] = ["settings.json", "keybindings.json"];
/// The laptop's folders, from the user-content mount, read-only.
pub const LAPTOP_FOLDERS: [&str; 4] = ["skills", "agents", "commands", "output-styles"];
/// The remote user's global state, kept beside `~/.claude` rather than in it.
const GLOBAL: &str = ".claude.json";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Side {
    State,
    Home,
    Laptop,
    /// The workspace's sessions folder, `projects/<key>`: this machine's, mounted on its own.
    Sessions,
    /// The laptop's skills, agents, commands and output styles.
    User,
}

/// A name with Claude Code's temporary and lock suffixes taken off: `x.tmp.123.abc` and `x.lock` are `x`.
fn stem(name: &str) -> &str {
    let name = name.strip_suffix(".lock").unwrap_or(name);
    match name.find(".tmp.") {
        Some(i) => &name[..i],
        None => name,
    }
}

fn side_of(first: &str) -> Side {
    let s = stem(first);
    if LAPTOP_FILES.contains(&s) {
        Side::Laptop
    } else if LAPTOP_FOLDERS.contains(&s) {
        Side::User
    } else if s == GLOBAL {
        Side::Home
    } else {
        Side::State
    }
}

fn attrs(meta: &std::fs::Metadata) -> FileAttributes {
    let mut a = FileAttributes::from(meta);
    (a.uid, a.gid, a.user, a.group) = (None, None, None, None);
    a
}

enum Open {
    /// A file, and the laptop file it saves when it was opened for writing.
    File(FsFile, Option<String>),
    Dir(Vec<File>),
}

/// Where each side's files are, on the remote machine.
pub struct Roots {
    /// The remote user's `~/.claude`.
    pub state: PathBuf,
    /// The remote user's home, for `~/.claude.json`.
    pub home: PathBuf,
    /// The laptop's copies of its settings and keybindings.
    pub laptop: PathBuf,
    /// The sessions mount.
    pub sessions: PathBuf,
    /// The user-content mount's `.claude`.
    pub user: PathBuf,
}

pub struct Config {
    /// By side, in [`Side`]'s order.
    roots: [PathBuf; 5],
    /// The workspace's project key: `projects/<key>` is the sessions side.
    key: String,
    /// The forwarded socket the laptop takes saves on.
    sink: PathBuf,
    sent: u64,
    handles: HashMap<String, Open>,
    next: u64,
}

impl Config {
    pub fn new(roots: Roots, key: &str, sink: &Path) -> Config {
        Config {
            roots: [roots.state, roots.home, roots.laptop, roots.sessions, roots.user],
            key: key.to_owned(),
            sink: sink.to_path_buf(),
            sent: 0,
            handles: HashMap::new(),
            next: 0,
        }
    }

    fn root(&self, side: Side) -> &Path {
        &self.roots[side as usize]
    }

    /// A client path as a path below the root; `..` stops at the root.
    fn relative(path: &str) -> PathBuf {
        let mut out = PathBuf::new();
        for c in Path::new(path).components() {
            match c {
                Component::Normal(n) => out.push(n),
                Component::ParentDir => {
                    out.pop();
                }
                _ => {}
            }
        }
        out
    }

    /// The side and local path of a client path, and its first component. The sessions folder is
    /// served from its own mount rather than mounted inside this one: macFUSE refuses a mount on a
    /// macFUSE volume.
    fn route(&self, path: &str) -> (Side, PathBuf, String) {
        let rel = Self::relative(path);
        let first = rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
        if let Ok(inside) = rel.strip_prefix(Path::new("projects").join(&self.key)) {
            return (Side::Sessions, self.root(Side::Sessions).join(inside), first);
        }
        let side = if first.is_empty() { Side::State } else { side_of(&first) };
        let local = self.root(side).join(&rel);
        (side, local, first)
    }

    /// The local path for a change: on the laptop side, only its two files (and their temporary
    /// files and locks) may change; its folders are the user's, read-only here.
    fn writable(&self, path: &str) -> Result<(PathBuf, Option<String>), StatusCode> {
        let (side, local, first) = self.route(path);
        match side {
            Side::User => Err(StatusCode::PermissionDenied),
            Side::Laptop if Self::relative(path).components().count() != 1 => Err(StatusCode::PermissionDenied),
            Side::Laptop => Ok((local, LAPTOP_FILES.contains(&first.as_str()).then_some(first))),
            _ => Ok((local, None)),
        }
    }

    fn handle(&mut self, open: Open) -> String {
        self.next += 1;
        self.handles.insert(self.next.to_string(), open);
        self.next.to_string()
    }

    /// Sends a saved laptop file back to the laptop: its name and a sequence number on the first
    /// line, then its bytes. Nothing waits for an answer, so a save never waits on the network; the
    /// link delivers it once the laptop is back.
    fn send(&mut self, name: &str) {
        let Ok(bytes) = std::fs::read(self.root(Side::Laptop).join(name)) else { return };
        self.sent += 1;
        let result = std::os::unix::net::UnixStream::connect(&self.sink).and_then(|mut conn| {
            conn.write_all(format!("{name} {}\n", self.sent).as_bytes())?;
            conn.write_all(&bytes)?;
            conn.shutdown(std::net::Shutdown::Write)
        });
        if let Err(e) = result {
            eprintln!("claudette: could not send {name} back to this machine: {e}");
        }
    }
}

impl russh_sftp::server::Handler for Config {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _version: u32, _extensions: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name { id, files: vec![File::dummy(format!("/{}", Self::relative(&path).to_string_lossy()))] })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let (_, local, _) = self.route(&path);
        Ok(Attrs { id, attrs: attrs(&std::fs::metadata(local).map_err(|e| io_status(&e))?) })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let (_, local, _) = self.route(&path);
        Ok(Attrs { id, attrs: attrs(&std::fs::symlink_metadata(local).map_err(|e| io_status(&e))?) })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.handles.get(&handle) {
            Some(Open::File(f, _)) => Ok(Attrs { id, attrs: attrs(&f.metadata().map_err(|e| io_status(&e))?) }),
            _ => Err(StatusCode::Failure),
        }
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let (_, local, _) = self.route(&path);
        let target = std::fs::read_link(local).map_err(|e| io_status(&e))?;
        Ok(Name { id, files: vec![File::dummy(target.to_string_lossy())] })
    }

    async fn setstat(&mut self, id: u32, path: String, a: FileAttributes) -> Result<Status, Self::Error> {
        let (local, laptop) = self.writable(&path)?;
        if let Some(size) = a.size {
            OpenOptions::new().write(true).open(&local).and_then(|f| f.set_len(size)).map_err(|e| io_status(&e))?;
        }
        if let Some(mode) = a.permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode & 0o7777)).map_err(|e| io_status(&e))?;
        }
        if let (Some(atime), Some(mtime)) = (a.atime, a.mtime) {
            let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(atime.into());
            let mt = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime.into());
            let times = std::fs::FileTimes::new().set_accessed(at).set_modified(mt);
            // A lock is a folder: proper-lockfile refreshes its mtime to say it is still held.
            let _ = FsFile::open(&local).and_then(|f| f.set_times(times));
        }
        if a.size.is_some()
            && let Some(name) = laptop
        {
            self.send(&name);
        }
        Ok(ok(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, a: FileAttributes) -> Result<Status, Self::Error> {
        if let (Some(Open::File(f, _)), Some(size)) = (self.handles.get(&handle), a.size) {
            f.set_len(size).map_err(|e| io_status(&e))?;
        }
        Ok(ok(id))
    }

    async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, a: FileAttributes) -> Result<Handle, Self::Error> {
        let writes = pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE);
        let (local, laptop) = if writes { self.writable(&filename)? } else { (self.route(&filename).1, None) };
        let file = OpenOptions::from(pflags).open(&local).map_err(|e| io_status(&e))?;
        if let Some(mode) = a.permissions.filter(|_| pflags.contains(OpenFlags::CREATE)) {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777));
        }
        Ok(Handle { id, handle: self.handle(Open::File(file, laptop)) })
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        let Some(Open::File(f, _)) = self.handles.get_mut(&handle) else { return Err(StatusCode::Failure) };
        f.seek(SeekFrom::Start(offset)).map_err(|e| io_status(&e))?;
        let mut data = vec![0u8; len as usize];
        let mut n = 0;
        while n < data.len() {
            match f.read(&mut data[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) => return Err(io_status(&e)),
            }
        }
        if n == 0 && len > 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(n);
        Ok(Data { id, data })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        let Some(Open::File(f, _)) = self.handles.get_mut(&handle) else { return Err(StatusCode::Failure) };
        f.seek(SeekFrom::Start(offset)).map_err(|e| io_status(&e))?;
        f.write_all(&data).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if let Some(Open::File(f, laptop)) = self.handles.remove(&handle) {
            let _ = f.sync_all();
            drop(f);
            if let Some(name) = laptop {
                self.send(&name);
            }
        }
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let (_, local, first) = self.route(&path);
        let mut files = vec![];
        let mut add = |dir: &Path, keep: &dyn Fn(&str) -> bool| {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if keep(&name)
                    && let Ok(meta) = std::fs::symlink_metadata(entry.path())
                {
                    files.push(File::new(name, attrs(&meta)));
                }
            }
        };
        if first.is_empty() {
            // The root: each side's own names, the remote ones the laptop's take the place of left out.
            std::fs::read_dir(self.root(Side::State)).map_err(|e| io_status(&e))?;
            add(self.root(Side::State), &|n| side_of(n) == Side::State);
            add(self.root(Side::Home), &|n| side_of(n) == Side::Home);
            add(self.root(Side::Laptop), &|n| side_of(n) == Side::Laptop);
            add(self.root(Side::User), &|n| side_of(n) == Side::User);
        } else {
            std::fs::read_dir(&local).map_err(|e| io_status(&e))?;
            add(&local, &|_| true);
        }
        Ok(Handle { id, handle: self.handle(Open::Dir(files)) })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(listing)) => match next_batch(listing) {
                Some(files) => Ok(Name { id, files }),
                None => Err(StatusCode::Eof),
            },
            _ => Err(StatusCode::Failure),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.writable(&filename)?.0).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        std::fs::create_dir(self.writable(&path)?.0).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.writable(&path)?.0).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, Self::Error> {
        let (from_side, to_side) = (self.route(&oldpath).0, self.route(&newpath).0);
        if from_side != to_side {
            return Err(StatusCode::OpUnsupported);
        }
        let ((from, _), (to, laptop)) = (self.writable(&oldpath)?, self.writable(&newpath)?);
        std::fs::rename(from, to).map_err(|e| io_status(&e))?;
        if let Some(name) = laptop {
            self.send(&name);
        }
        Ok(ok(id))
    }

    async fn symlink(&mut self, _id: u32, _linkpath: String, _targetpath: String) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied)
    }
}

/// stdin that says when it ends: russh-sftp serves on a task of its own and returns at once.
struct Watched<R> {
    inner: R,
    done: Option<tokio::sync::oneshot::Sender<()>>,
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Watched<R> {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(result) = &poll
            && (result.is_err() || buf.filled().len() == before)
            && let Some(done) = self.done.take()
        {
            let _ = done.send(());
        }
        poll
    }
}

/// `__helper config <remote ~/.claude> <remote home> <laptop copies> <sink socket> <project key>
/// <sessions mount> <user-content .claude>`: serves until sshfs closes its end.
pub async fn serve(args: &[String]) -> i32 {
    let [state, home, laptop, sink, key, sessions, user] = args else {
        eprintln!("claudette: usage: __helper config <state> <home> <laptop> <sink> <key> <sessions> <user>");
        return 2;
    };
    let roots = Roots { state: state.into(), home: home.into(), laptop: laptop.into(), sessions: sessions.into(), user: user.into() };
    let config = Config::new(roots, key, Path::new(sink));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let stdin = Watched { inner: tokio::io::stdin(), done: Some(tx) };
    russh_sftp::server::run(tokio::io::join(stdin, tokio::io::stdout()), config).await;
    let _ = rx.await;
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::server::Handler;

    struct Setup {
        dir: PathBuf,
        config: Config,
        sink: std::os::unix::net::UnixListener,
    }

    fn setup() -> Setup {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Short: the sink is a socket, whose path has room for about 100 bytes.
        let dir = std::env::temp_dir().join(format!("ctc-{}-{n}", std::process::id()));
        for d in ["claude/projects/-w-app", "home", "laptop", "user/skills/demo", "claude/skills/remote-only", "sessions"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("home/.claude.json"), r#"{"hasCompletedOnboarding":true}"#).unwrap();
        std::fs::write(dir.join("claude/settings.json"), r#"{"theme":"remote's own"}"#).unwrap();
        std::fs::write(dir.join("laptop/settings.json"), r#"{"theme":"laptop's"}"#).unwrap();
        std::fs::write(dir.join("user/skills/demo/SKILL.md"), "demo").unwrap();
        let sink = std::os::unix::net::UnixListener::bind(dir.join("sink")).unwrap();
        sink.set_nonblocking(true).unwrap();
        let roots = Roots { state: dir.join("claude"), home: dir.join("home"), laptop: dir.join("laptop"), sessions: dir.join("sessions"), user: dir.join("user") };
        let config = Config::new(roots, "-w-app", &dir.join("sink"));
        Setup { dir, config, sink }
    }

    fn sent(sink: &std::os::unix::net::UnixListener) -> Vec<String> {
        let mut out = vec![];
        while let Ok((mut conn, _)) = sink.accept() {
            conn.set_nonblocking(false).unwrap();
            let mut s = String::new();
            conn.read_to_string(&mut s).unwrap();
            out.push(s);
        }
        out
    }

    async fn save(c: &mut Config, path: &str, content: &str) {
        let tmp = format!("{path}.tmp.42.abcdef");
        let h = c.open(1, tmp.clone(), OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE, FileAttributes::empty()).await.unwrap().handle;
        c.write(2, h.clone(), 0, content.as_bytes().to_vec()).await.unwrap();
        c.close(3, h).await.unwrap();
        c.rename(4, tmp, path.to_owned()).await.unwrap();
    }

    async fn names(c: &mut Config, path: &str) -> Vec<String> {
        let h = c.opendir(1, path.into()).await.unwrap().handle;
        let mut names: Vec<String> = c.readdir(2, h).await.unwrap().files.into_iter().map(|f| f.filename).collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn each_file_comes_from_its_side_and_a_laptop_save_goes_back() {
        let Setup { dir, mut config, sink } = setup();
        assert_eq!(names(&mut config, "/").await, [".claude.json", "projects", "settings.json", "skills"]);
        // The laptop's settings and skills, never the remote user's own.
        let h = config.open(1, "/settings.json".into(), OpenFlags::READ, FileAttributes::empty()).await.unwrap().handle;
        assert_eq!(config.read(2, h, 0, 100).await.unwrap().data, br#"{"theme":"laptop's"}"#);
        let h = config.open(3, "/skills/demo/SKILL.md".into(), OpenFlags::READ, FileAttributes::empty()).await.unwrap().handle;
        assert_eq!(config.read(4, h, 0, 100).await.unwrap().data, b"demo");

        save(&mut config, "/settings.json", r#"{"theme":"light"}"#).await;
        assert_eq!(std::fs::read_to_string(dir.join("laptop/settings.json")).unwrap(), r#"{"theme":"light"}"#);
        assert_eq!(std::fs::read_to_string(dir.join("claude/settings.json")).unwrap(), r#"{"theme":"remote's own"}"#);
        assert_eq!(sent(&sink), ["settings.json 1\n{\"theme\":\"light\"}"]);

        // State and global state are the remote user's real files.
        save(&mut config, "/.claude.json", r#"{"hasCompletedOnboarding":true,"numStartups":2}"#).await;
        assert!(std::fs::read_to_string(dir.join("home/.claude.json")).unwrap().contains("numStartups"));
        config.mkdir(5, "/projects/-w-other".into(), FileAttributes::empty()).await.unwrap();
        assert!(dir.join("claude/projects/-w-other").is_dir());
        // The workspace's sessions folder is this machine's, from its own mount.
        let h = config.open(6, "/projects/-w-app/s.jsonl".into(), OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::APPEND, FileAttributes::empty()).await.unwrap().handle;
        config.write(7, h.clone(), 0, b"{}\n".to_vec()).await.unwrap();
        config.close(8, h).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("sessions/s.jsonl")).unwrap(), "{}\n");
        assert!(!dir.join("claude/projects/-w-app/s.jsonl").exists());
        assert_eq!(names(&mut config, "/projects/-w-app").await, ["s.jsonl"]);
        assert!(sent(&sink).is_empty(), "only the laptop's files go back");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn the_laptops_folders_are_read_only_and_a_rename_never_crosses_sides() {
        let Setup { dir, mut config, sink: _sink } = setup();
        assert_eq!(config.mkdir(1, "/skills/new".into(), FileAttributes::empty()).await.err(), Some(StatusCode::PermissionDenied));
        assert_eq!(config.remove(2, "/skills".into()).await.err(), Some(StatusCode::PermissionDenied));
        std::fs::write(dir.join("claude/history.jsonl"), "").unwrap();
        assert_eq!(config.rename(3, "/history.jsonl".into(), "/settings.json.tmp.1.abc".into()).await.err(), Some(StatusCode::OpUnsupported));
        // A lock beside a laptop file is on the laptop side, and can be taken and released.
        config.mkdir(4, "/settings.json.lock".into(), FileAttributes::empty()).await.unwrap();
        assert!(dir.join("laptop/settings.json.lock").is_dir());
        config.rmdir(5, "/settings.json.lock".into()).await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
