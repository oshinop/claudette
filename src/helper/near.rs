//! `__helper sftp <socket> <warm file> [--writeback]`: the remote end of one of this machine's mounts.
//! sshfs speaks SFTP to it instead of to a plain relay to the forwarded socket, and it answers what it
//! already knows without a round trip to the laptop. On a slow link every round trip is claude
//! waiting, one after another.
//!
//! - **Warm start.** What claude reads first from the mount comes with the run's first frames (see
//!   [`crate::mount::warm`]): the server's greeting, listings, attributes and small files. The mount
//!   comes up, and claude starts, without waiting on the laptop.
//! - **A short-lived cache.** Attributes and complete listings, warm or fetched, answer lookups for
//!   [`TTL`], as sshfs's own cache (off under this one) did for longer; a complete listing also says
//!   which names are not there. Whatever a change made through the mount touches is dropped.
//! - **Write-behind** (`--writeback`, the sessions folder). A transcript is written by the claude whose
//!   session it is and nobody else. The near end keeps a copy of each one claude writes, as the laptop
//!   has it (the same [`LineWriter`] turns the remote paths into the laptop's), and answers claude's
//!   opens, writes, reads and lookups of it at once. Every write still leaves for the laptop as it is
//!   made, in order, under a handle the near end chose ([`OPEN_AS`]), so a hook there that reads the
//!   transcript finds it written; the laptop's answers are only checked. Once sshfs has gone, the near
//!   end waits for the last of them before it exits, and the session's end waits for the near end.

use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{BufMut, Bytes, BytesMut};
use russh_sftp::protocol::{Attrs, Close, Data, Extended, File, FileAttributes, Handle, Init, Name, OpenFlags, Packet, Read, StatusCode, Version};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Notify, mpsc};

use crate::mount::sftp::{LineWriter, OPEN_AS, OpenAs, next_batch};
use crate::mount::warm::Warm;
use crate::mount::workdir::PathMap;

/// How long what the near end learned stands for the laptop's answer. sshfs's own cache, which this
/// one replaces, kept attributes and listings for 20 s; the kernel keeps absent names for 10 s.
const TTL: Duration = Duration::from_secs(10);
/// How long the near end waits, once sshfs has gone, for the laptop to confirm the writes it sent.
const DRAIN: Duration = Duration::from_secs(10);
/// A transcript claude opens to write that the near end does not keep yet is fetched whole first, in
/// reads of this size sent together, up to [`FETCH_MAX`] (what a warm start may carry); a larger one
/// is written through, a round trip at a time.
const FETCH_CHUNK: u32 = 1 << 20;
const FETCH_MAX: u64 = 8 << 20;
/// Bytes waiting to leave for the laptop past which sshfs's requests wait too: while the link is
/// down, writes answered at once would otherwise pile up without end.
const QUEUE_MAX: usize = 16 << 20;
/// Largest packet taken from either side.
const MAX_PACKET: u32 = 4 << 20;

const SSH_FXP_VERSION: u8 = 2;

/// A path as the servers resolve it: absolute, `.` dropped, `..` stopping at the root.
fn norm(path: &str) -> String {
    let mut parts: Vec<&str> = vec![];
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// A normalized path's folder and name; `None` for the root.
fn split(path: &str) -> Option<(&str, &str)> {
    if path == "/" {
        return None;
    }
    let at = path.rfind('/')?;
    Some((if at == 0 { "/" } else { &path[..at] }, &path[at + 1..]))
}

/// The files the near end writes behind: Claude Code's transcripts.
fn is_transcript(path: &str) -> bool {
    path.ends_with(".jsonl")
}

fn unix_now() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

/// A request or reply (type, id, payload) with another id, length-prefixed for the wire.
fn with_id(raw: &Bytes, id: u32) -> Bytes {
    let mut out = BytesMut::with_capacity(raw.len() + 4);
    out.put_u32(raw.len() as u32);
    out.put_u8(raw[0]);
    out.put_u32(id);
    out.put_slice(&raw[5..]);
    out.freeze()
}

fn is_ok(packet: &Option<Packet>) -> bool {
    matches!(packet, Some(Packet::Status(s)) if s.status_code == StatusCode::Ok)
}

/// The code of a status answer; anything else is a failure.
fn status_of(packet: &Option<Packet>) -> StatusCode {
    match packet {
        Some(Packet::Status(s)) => s.status_code,
        _ => StatusCode::Failure,
    }
}

struct Fresh<T> {
    value: T,
    at: Instant,
}

impl<T> Fresh<T> {
    fn new(value: T) -> Fresh<T> {
        Fresh { value, at: Instant::now() }
    }

    fn get(&self) -> Option<&T> {
        (self.at.elapsed() < TTL).then_some(&self.value)
    }
}

#[derive(Default)]
struct Cache {
    /// lstat answers; `None`: no such file.
    attrs: HashMap<String, Fresh<Option<FileAttributes>>>,
    /// Complete listings, with lstat attributes.
    listings: HashMap<String, Fresh<Vec<File>>>,
    contents: HashMap<String, Fresh<Bytes>>,
}

impl Cache {
    /// What lstat says of `path`, when known: `Some(None)` when it does not exist.
    fn lookup(&self, path: &str) -> Option<Option<FileAttributes>> {
        if let Some(attrs) = self.attrs.get(path).and_then(Fresh::get) {
            return Some(attrs.clone());
        }
        let (dir, name) = split(path)?;
        let files = self.listing(dir)?;
        Some(files.iter().find(|f| f.filename == name).map(|f| f.attrs.clone()))
    }

    fn listing(&self, path: &str) -> Option<&Vec<File>> {
        self.listings.get(path).and_then(Fresh::get)
    }

    fn content(&self, path: &str) -> Option<Bytes> {
        self.contents.get(path).and_then(Fresh::get).cloned()
    }

    fn is_dir(&self, path: &str) -> bool {
        self.listing(path).is_some() || self.lookup(path).flatten().is_some_and(|a| a.is_dir())
    }

    /// Drops what a change to `path` makes stale: it, and its folder's attributes and listing.
    fn forget(&mut self, path: &str) {
        self.attrs.remove(path);
        self.contents.remove(path);
        self.listings.remove(path);
        if let Some((dir, _)) = split(path) {
            self.attrs.remove(dir);
            self.listings.remove(dir);
        }
    }

    /// Also everything below `path`, a folder renamed or removed.
    fn forget_below(&mut self, path: &str) {
        self.forget(path);
        let prefix = format!("{}/", path.trim_end_matches('/'));
        self.attrs.retain(|p, _| !p.starts_with(&prefix));
        self.listings.retain(|p, _| !p.starts_with(&prefix));
        self.contents.retain(|p, _| !p.starts_with(&prefix));
    }
}

/// A transcript the near end keeps: its bytes as the laptop has them.
struct Kept {
    data: Vec<u8>,
    attrs: FileAttributes,
}

impl Kept {
    fn new(mode: u32) -> Kept {
        let now = unix_now();
        let attrs = FileAttributes { permissions: Some(0o100000 | (mode & 0o777)), atime: Some(now), mtime: Some(now), ..FileAttributes::default() };
        Kept { data: vec![], attrs }
    }

    fn attrs(&self) -> FileAttributes {
        let mut attrs = self.attrs.clone();
        attrs.size = Some(self.data.len() as u64);
        attrs
    }

    fn touch(&mut self) {
        self.attrs.mtime = Some(unix_now());
    }
}

/// A handle the near end answers for itself.
enum Local {
    Dir(Vec<File>),
    /// A cached file's contents as they were when it was opened.
    Snapshot { data: Bytes, attrs: FileAttributes },
    /// A kept transcript; `lines` when opened to write, then the laptop has it open under the same
    /// handle. `failed`: the laptop refused one of its writes, which the next one reports.
    Kept { path: String, lines: Option<LineWriter>, failed: Option<StatusCode> },
}

/// What a change does to the kept transcripts once the laptop has made it.
enum KeptChange {
    None,
    Remove(String),
    Rename(String, String),
}

/// What to do with the laptop's answer to a request.
enum Then {
    Pass,
    Lstat(String),
    OpenDir(String),
    ReadDir(String),
    Open { path: String, writes: bool },
    Close(String),
    Changed { forget: Vec<String>, below: bool, kept: KeptChange },
    MadeDir(String),
    /// A write-behind request of this handle: its answer is only checked.
    Behind(String),
    /// The laptop's open of a kept transcript, under this handle: refused, the copy is dropped.
    OpenedBehind { handle: String, path: String },
    FetchOpen(String),
    /// A read of the fetch's handle, at this offset.
    FetchRead(String, u64),
}

struct Pending {
    /// The id sshfs gave the request, when the answer is for sshfs.
    client: Option<u32>,
    then: Then,
    /// [`Near::epoch`] when the request left: an answer from before a change is not cached.
    epoch: u64,
}

/// A transcript being fetched before claude's open of it is answered. sshfs's later requests wait.
struct Fetch {
    path: String,
    handle: String,
    attrs: FileAttributes,
    data: Vec<u8>,
    /// Where the next read starts, and how many reads are unanswered.
    next: u64,
    waiting: usize,
    /// claude's open: its id, flags and attributes, and the request itself.
    open: (u32, OpenFlags, FileAttributes, Bytes),
}

/// What a request on a handle comes to.
enum Act {
    /// Answered here; the request also goes to the laptop, whose answer is only checked.
    Behind,
    Answer(StatusCode),
    /// A handle of the laptop's: the request goes there; the path it changes, if known.
    Through(Option<String>),
}

/// What waits to leave for the laptop, in bytes, and a wake-up each time some of it has left.
#[derive(Default)]
struct Queue {
    bytes: AtomicUsize,
    drained: Notify,
}

struct Near {
    /// Set when writing transcripts behind: how the laptop maps their lines.
    writeback: Option<PathMap>,
    cache: Cache,
    kept: HashMap<String, Kept>,
    local: HashMap<String, Local>,
    /// The laptop's folder handles being read through: path, entries so far, epoch at opening.
    remote_dirs: HashMap<String, (String, Vec<File>, u64)>,
    /// The laptop's file handles: path, and whether opened to write.
    remote_files: HashMap<String, (String, bool)>,
    pending: HashMap<u32, Pending>,
    next_id: u32,
    next_handle: u64,
    /// Bumped by every change made through the mount.
    epoch: u64,
    version: Option<(u32, HashMap<String, String>)>,
    /// sshfs's INIT, until the laptop's VERSION answers it.
    client_waits: bool,
    fetch: Option<Fetch>,
    deferred: VecDeque<Bytes>,
    /// The first failure of a write behind: said at once, or at the end on a terminal, which claude's
    /// interface owns until then.
    warning: Option<String>,
    to_client: mpsc::UnboundedSender<Bytes>,
    to_server: mpsc::UnboundedSender<Bytes>,
    queue: Arc<Queue>,
}

impl Near {
    fn new(warm: Warm, writeback: bool, to_client: mpsc::UnboundedSender<Bytes>, to_server: mpsc::UnboundedSender<Bytes>, queue: Arc<Queue>) -> Near {
        let writeback = if writeback { warm.paths.as_ref().map(PathMap::from_parts) } else { None };
        let mut cache = Cache::default();
        for (path, attrs) in warm.attrs {
            cache.attrs.insert(path, Fresh::new(attrs));
        }
        for (path, files) in warm.listings {
            cache.listings.insert(path, Fresh::new(files));
        }
        let mut kept = HashMap::new();
        for (path, data) in warm.contents {
            // A transcript sent whole (one being resumed) is kept from the start.
            match cache.lookup(&path) {
                Some(Some(attrs)) if writeback.is_some() && is_transcript(&path) => {
                    kept.insert(path, Kept { data, attrs });
                }
                _ => {
                    cache.contents.insert(path, Fresh::new(Bytes::from(data)));
                }
            }
        }
        let near = Near {
            writeback,
            cache,
            kept,
            local: HashMap::new(),
            remote_dirs: HashMap::new(),
            remote_files: HashMap::new(),
            pending: HashMap::new(),
            next_id: 0,
            next_handle: 0,
            epoch: 0,
            version: warm.version,
            client_waits: false,
            fetch: None,
            deferred: VecDeque::new(),
            warning: None,
            to_client,
            to_server,
            queue,
        };
        // The laptop's server is greeted at once; its answer is only needed without a warm start.
        if let Ok(init) = Bytes::try_from(Packet::Init(Init { version: 3, extensions: HashMap::new() })) {
            near.send_server(init);
        }
        near
    }

    /// Nothing is waiting on the laptop.
    fn idle(&self) -> bool {
        self.pending.is_empty() && self.fetch.is_none()
    }

    /// Too much waits to leave for the laptop: sshfs's next requests wait with it.
    fn congested(&self) -> bool {
        self.queue.bytes.load(Ordering::Relaxed) > QUEUE_MAX
    }

    fn send_server(&self, bytes: Bytes) {
        self.queue.bytes.fetch_add(bytes.len(), Ordering::Relaxed);
        let _ = self.to_server.send(bytes);
    }

    fn unsaved(&mut self, code: StatusCode) {
        if self.warning.is_none() {
            let warning = format!("claudette: the local machine could not save the session's transcript: {code}");
            if !std::io::stderr().is_terminal() {
                eprintln!("{warning}");
            }
            self.warning = Some(warning);
        }
    }

    /// Writes sent behind that the laptop has not confirmed.
    fn behind(&self) -> bool {
        self.pending.values().any(|p| matches!(p.then, Then::Behind(_)))
    }

    fn reply(&self, packet: Packet) {
        if let Ok(bytes) = Bytes::try_from(packet) {
            let _ = self.to_client.send(bytes);
        }
    }

    fn status(&self, id: u32, code: StatusCode) {
        self.reply(Packet::error(id, code));
    }

    fn take_id(&mut self, client: Option<u32>, then: Then) -> u32 {
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.insert(self.next_id, Pending { client, then, epoch: self.epoch });
        self.next_id
    }

    /// Sends one of sshfs's requests on; the answer goes back to sshfs when `client` is its id.
    fn forward(&mut self, client: Option<u32>, raw: &Bytes, then: Then) {
        let id = self.take_id(client, then);
        self.send_server(with_id(raw, id));
    }

    /// Sends a request of the near end's own.
    fn send_own(&mut self, then: Then, packet: impl FnOnce(u32) -> Packet) {
        let id = self.take_id(None, then);
        if let Ok(bytes) = Bytes::try_from(packet(id)) {
            self.send_server(bytes);
        }
    }

    fn new_handle(&mut self) -> String {
        self.next_handle += 1;
        // Never a digit first: the laptop's own handles are numbers.
        format!("n{}", self.next_handle)
    }

    fn forget(&mut self, path: &str) {
        self.epoch += 1;
        self.cache.forget(path);
    }

    fn from_client(&mut self, raw: Bytes) {
        if self.fetch.is_some() {
            self.deferred.push_back(raw);
            return;
        }
        let Ok(packet) = Packet::try_from(&mut raw.clone()) else { return self.unknown(&raw) };
        match packet {
            Packet::Init(_) => self.init(),
            Packet::Lstat(r) => self.lstat(r.id, norm(&r.path), &raw),
            Packet::Stat(r) => self.stat(r.id, norm(&r.path), &raw),
            Packet::Fstat(r) => self.fstat(r.id, &r.handle, &raw),
            Packet::OpenDir(r) => self.opendir(r.id, norm(&r.path), &raw),
            Packet::ReadDir(r) => self.readdir(r.id, r.handle, &raw),
            Packet::Open(r) => self.open(r.id, norm(&r.filename), r.pflags, r.attrs, &raw),
            Packet::Read(r) => self.read(r.id, &r.handle, r.offset, r.len, &raw),
            Packet::Write(r) => self.write(r.id, r.handle, r.offset, &r.data, &raw),
            Packet::Close(r) => self.close(r.id, r.handle, &raw),
            Packet::FSetStat(r) => self.fsetstat(r.id, r.handle, &r.attrs, &raw),
            Packet::SetStat(r) => self.setstat(r.id, norm(&r.path), &r.attrs, &raw),
            Packet::Remove(r) => {
                let path = norm(&r.filename);
                self.change(r.id, &raw, vec![path.clone()], false, KeptChange::Remove(path));
            }
            Packet::Rename(r) => {
                let (from, to) = (norm(&r.oldpath), norm(&r.newpath));
                self.change(r.id, &raw, vec![from.clone(), to.clone()], true, KeptChange::Rename(from, to));
            }
            Packet::RmDir(r) => self.change(r.id, &raw, vec![norm(&r.path)], true, KeptChange::None),
            Packet::Symlink(r) => self.change(r.id, &raw, vec![norm(&r.linkpath)], false, KeptChange::None),
            Packet::MkDir(r) => {
                let path = norm(&r.path);
                self.forget(&path);
                self.forward(Some(r.id), &raw, Then::MadeDir(path));
            }
            Packet::RealPath(r) => self.forward(Some(r.id), &raw, Then::Pass),
            Packet::ReadLink(r) => self.forward(Some(r.id), &raw, Then::Pass),
            Packet::Extended(r) => {
                // An extension may change anything.
                self.epoch += 1;
                self.cache = Cache::default();
                self.forward(Some(r.id), &raw, Then::Pass);
            }
            _ => self.unknown(&raw),
        }
    }

    /// A request this end does not read: passed on, and nothing cached is trusted any more.
    fn unknown(&mut self, raw: &Bytes) {
        if raw.len() < 5 {
            return;
        }
        let id = u32::from_be_bytes(raw[1..5].try_into().expect("4 bytes"));
        self.epoch += 1;
        self.cache = Cache::default();
        self.forward(Some(id), raw, Then::Pass);
    }

    fn init(&mut self) {
        match &self.version {
            Some((version, extensions)) => self.reply(Packet::Version(Version { version: *version, extensions: extensions.clone() })),
            None => self.client_waits = true,
        }
    }

    fn lstat(&mut self, id: u32, path: String, raw: &Bytes) {
        if let Some(kept) = self.kept.get(&path) {
            return self.reply(Packet::Attrs(Attrs { id, attrs: kept.attrs() }));
        }
        match self.cache.lookup(&path) {
            Some(Some(attrs)) => self.reply(Packet::Attrs(Attrs { id, attrs })),
            Some(None) => self.status(id, StatusCode::NoSuchFile),
            None => self.forward(Some(id), raw, Then::Lstat(path)),
        }
    }

    fn stat(&mut self, id: u32, path: String, raw: &Bytes) {
        if let Some(kept) = self.kept.get(&path) {
            return self.reply(Packet::Attrs(Attrs { id, attrs: kept.attrs() }));
        }
        match self.cache.lookup(&path) {
            // What a symlink leads to is the laptop's to say.
            Some(Some(attrs)) if !attrs.is_symlink() => self.reply(Packet::Attrs(Attrs { id, attrs })),
            Some(None) => self.status(id, StatusCode::NoSuchFile),
            _ => self.forward(Some(id), raw, Then::Pass),
        }
    }

    fn fstat(&mut self, id: u32, handle: &str, raw: &Bytes) {
        let attrs = match self.local.get(handle) {
            None => return self.forward(Some(id), raw, Then::Pass),
            Some(Local::Snapshot { attrs, .. }) => Some(attrs.clone()),
            Some(Local::Kept { path, .. }) => self.kept.get(path).map(Kept::attrs),
            Some(Local::Dir(_)) => None,
        };
        match attrs {
            Some(attrs) => self.reply(Packet::Attrs(Attrs { id, attrs })),
            None => self.status(id, StatusCode::Failure),
        }
    }

    fn opendir(&mut self, id: u32, path: String, raw: &Bytes) {
        if let Some(files) = self.cache.listing(&path) {
            let mut files = files.clone();
            for (kept_path, kept) in &self.kept {
                if let Some((dir, name)) = split(kept_path)
                    && dir == path
                {
                    match files.iter_mut().find(|f| f.filename == name) {
                        Some(file) => file.attrs = kept.attrs(),
                        None => files.push(File { filename: name.to_owned(), longname: String::new(), attrs: kept.attrs() }),
                    }
                }
            }
            let handle = self.new_handle();
            self.local.insert(handle.clone(), Local::Dir(files));
            return self.reply(Packet::Handle(Handle { id, handle }));
        }
        if matches!(self.cache.lookup(&path), Some(None)) {
            return self.status(id, StatusCode::NoSuchFile);
        }
        self.forward(Some(id), raw, Then::OpenDir(path));
    }

    fn readdir(&mut self, id: u32, handle: String, raw: &Bytes) {
        let batch = match self.local.get_mut(&handle) {
            None => return self.forward(Some(id), raw, Then::ReadDir(handle)),
            Some(Local::Dir(files)) => next_batch(files),
            Some(_) => return self.status(id, StatusCode::Failure),
        };
        match batch {
            Some(files) => self.reply(Packet::Name(Name { id, files })),
            None => self.status(id, StatusCode::Eof),
        }
    }

    fn open(&mut self, id: u32, path: String, pflags: OpenFlags, attrs: FileAttributes, raw: &Bytes) {
        let writes = pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE);
        if self.kept.contains_key(&path) || (writes && self.writeback.is_some() && is_transcript(&path)) {
            return self.open_kept(id, path, pflags, attrs, raw);
        }
        if writes {
            return self.write_through(id, path, raw);
        }
        match self.cache.lookup(&path) {
            Some(None) => return self.status(id, StatusCode::NoSuchFile),
            Some(Some(attrs)) => {
                if let Some(data) = self.cache.content(&path) {
                    let handle = self.new_handle();
                    self.local.insert(handle.clone(), Local::Snapshot { data, attrs });
                    return self.reply(Packet::Handle(Handle { id, handle }));
                }
            }
            None => {}
        }
        self.forward(Some(id), raw, Then::Open { path, writes: false });
    }

    fn write_through(&mut self, id: u32, path: String, raw: &Bytes) {
        self.forget(&path);
        self.forward(Some(id), raw, Then::Open { path, writes: true });
    }

    /// An open of a transcript to write, or of one kept: kept from now on when what the laptop has
    /// is known (new, cut to nothing, or fetched first).
    fn open_kept(&mut self, id: u32, path: String, pflags: OpenFlags, attrs: FileAttributes, raw: &Bytes) {
        let exclusive = pflags.contains(OpenFlags::CREATE | OpenFlags::EXCLUDE);
        if self.kept.contains_key(&path) {
            if exclusive {
                return self.status(id, StatusCode::Failure);
            }
            return self.open_kept_now(id, path, pflags, attrs);
        }
        match self.cache.lookup(&path) {
            Some(None) if !pflags.contains(OpenFlags::CREATE) => self.status(id, StatusCode::NoSuchFile),
            Some(None) if split(&path).is_some_and(|(dir, _)| self.cache.is_dir(dir)) => {
                self.kept.insert(path.clone(), Kept::new(attrs.permissions.unwrap_or(0o644)));
                self.open_kept_now(id, path, pflags, attrs);
            }
            Some(Some(existing)) if existing.is_regular() => {
                if exclusive {
                    self.status(id, StatusCode::Failure);
                } else if pflags.contains(OpenFlags::TRUNCATE) {
                    self.kept.insert(path.clone(), Kept { data: vec![], attrs: existing });
                    self.open_kept_now(id, path, pflags, attrs);
                } else if existing.len() <= FETCH_MAX {
                    self.start_fetch(id, path, pflags, attrs, existing, raw);
                } else {
                    self.write_through(id, path, raw);
                }
            }
            _ => self.write_through(id, path, raw),
        }
    }

    fn open_kept_now(&mut self, id: u32, path: String, pflags: OpenFlags, attrs: FileAttributes) {
        let writes = pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE);
        let size = {
            let kept = self.kept.get_mut(&path).expect("a kept transcript");
            if pflags.contains(OpenFlags::TRUNCATE) && !kept.data.is_empty() {
                kept.data.clear();
                kept.touch();
            }
            kept.data.len() as u64
        };
        let handle = self.new_handle();
        if writes && let Ok(request) = russh_sftp::ser::to_bytes(&OpenAs { handle: handle.clone(), filename: path.clone(), pflags, attrs }) {
            self.send_own(Then::OpenedBehind { handle: handle.clone(), path: path.clone() }, |id| Packet::Extended(Extended { id, request: OPEN_AS.into(), data: request.to_vec() }));
        }
        self.local.insert(handle.clone(), Local::Kept { path, lines: writes.then(|| LineWriter::new(pflags, size)), failed: None });
        self.reply(Packet::Handle(Handle { id, handle }));
    }

    fn start_fetch(&mut self, id: u32, path: String, pflags: OpenFlags, attrs: FileAttributes, existing: FileAttributes, raw: &Bytes) {
        let handle = self.new_handle();
        let Ok(request) = russh_sftp::ser::to_bytes(&OpenAs { handle: handle.clone(), filename: path.clone(), pflags: OpenFlags::READ, attrs: FileAttributes::empty() }) else {
            return self.write_through(id, path, raw);
        };
        self.send_own(Then::FetchOpen(handle.clone()), |id| Packet::Extended(Extended { id, request: OPEN_AS.into(), data: request.to_vec() }));
        // The whole file at once, and a read past its end that says where it ends.
        let reads = existing.len() / u64::from(FETCH_CHUNK) + 1;
        self.fetch = Some(Fetch { path, handle, attrs: existing, data: vec![], next: 0, waiting: 0, open: (id, pflags, attrs, raw.clone()) });
        self.fetch_more(reads);
    }

    fn fetch_more(&mut self, reads: u64) {
        for _ in 0..reads {
            // Up to a read at FETCH_MAX itself, which finds the end of a file that size.
            let Some(fetch) = self.fetch.as_mut().filter(|f| f.next <= FETCH_MAX) else { return };
            let (handle, offset) = (fetch.handle.clone(), fetch.next);
            fetch.next += u64::from(FETCH_CHUNK);
            fetch.waiting += 1;
            self.send_own(Then::FetchRead(handle.clone(), offset), |id| Packet::Read(Read { id, handle, offset, len: FETCH_CHUNK }));
        }
    }

    fn fetching(&self, handle: &str) -> bool {
        self.fetch.as_ref().is_some_and(|f| f.handle == handle)
    }

    /// A read of the fetch answered. Answers come in the order the reads went.
    fn fetched(&mut self, offset: u64, packet: Option<Packet>) {
        let Some(fetch) = self.fetch.as_mut() else { return };
        fetch.waiting -= 1;
        let ended = match packet {
            Some(Packet::Data(chunk)) if offset == fetch.data.len() as u64 => {
                fetch.data.extend_from_slice(&chunk.data);
                chunk.data.len() < FETCH_CHUNK as usize
            }
            Some(Packet::Status(s)) if s.status_code == StatusCode::Eof && offset >= fetch.data.len() as u64 => true,
            // An error, or the file changed under the reads.
            _ => return self.end_fetch(false),
        };
        if fetch.waiting > 0 {
            return;
        }
        if ended {
            self.end_fetch(true);
        } else if fetch.next > FETCH_MAX {
            self.end_fetch(false);
        } else {
            // It grew since it was listed.
            self.fetch_more(1);
        }
    }

    fn end_fetch(&mut self, complete: bool) {
        let Some(fetch) = self.fetch.take() else { return };
        let handle = fetch.handle.clone();
        self.send_own(Then::Pass, |id| Packet::Close(Close { id, handle }));
        let (id, pflags, attrs, raw) = fetch.open;
        if complete {
            self.kept.insert(fetch.path.clone(), Kept { data: fetch.data, attrs: fetch.attrs });
            self.open_kept_now(id, fetch.path, pflags, attrs);
        } else {
            self.write_through(id, fetch.path, &raw);
        }
        while self.fetch.is_none()
            && let Some(raw) = self.deferred.pop_front()
        {
            self.from_client(raw);
        }
    }

    fn read(&mut self, id: u32, handle: &str, offset: u64, len: u32, raw: &Bytes) {
        let data: &[u8] = match self.local.get(handle) {
            None => return self.forward(Some(id), raw, Then::Pass),
            Some(Local::Snapshot { data, .. }) => data,
            Some(Local::Kept { path, .. }) => match self.kept.get(path) {
                Some(kept) => &kept.data,
                None => return self.status(id, StatusCode::Failure),
            },
            Some(Local::Dir(_)) => return self.status(id, StatusCode::Failure),
        };
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(data.len());
        let end = start.saturating_add(len as usize).min(data.len());
        if start == end && len > 0 {
            return self.status(id, StatusCode::Eof);
        }
        self.reply(Packet::Data(Data { id, data: data[start..end].to_vec() }));
    }

    fn write(&mut self, id: u32, handle: String, offset: u64, data: &[u8], raw: &Bytes) {
        let act = match self.local.get_mut(&handle) {
            None => Act::Through(self.remote_files.get(&handle).map(|(path, _)| path.clone())),
            Some(Local::Kept { path, lines: Some(lines), failed: None }) => match (self.kept.get_mut(path.as_str()), self.writeback.as_ref()) {
                (Some(kept), Some(paths)) => {
                    let mut file = Cursor::new(std::mem::take(&mut kept.data));
                    let written = lines.write(paths, &mut file, offset, data);
                    kept.data = file.into_inner();
                    kept.touch();
                    if written.is_ok() { Act::Behind } else { Act::Answer(StatusCode::Failure) }
                }
                _ => Act::Answer(StatusCode::Failure),
            },
            Some(Local::Kept { failed: Some(code), .. }) => Act::Answer(*code),
            Some(_) => Act::Answer(StatusCode::PermissionDenied),
        };
        self.act(id, handle, raw, act);
    }

    fn close(&mut self, id: u32, handle: String, raw: &Bytes) {
        let act = match self.local.remove(&handle) {
            None => return self.forward(Some(id), raw, Then::Close(handle)),
            Some(Local::Kept { path, lines: Some(mut lines), failed }) => {
                if let (Some(kept), Some(paths)) = (self.kept.get_mut(&path), self.writeback.as_ref()) {
                    let before = kept.data.len();
                    let mut file = Cursor::new(std::mem::take(&mut kept.data));
                    let _ = lines.flush(paths, &mut file, true);
                    kept.data = file.into_inner();
                    if kept.data.len() != before {
                        kept.touch();
                    }
                }
                // The laptop closes it too, whatever came of its writes.
                self.forward(None, raw, Then::Behind(handle));
                return self.status(id, failed.unwrap_or(StatusCode::Ok));
            }
            Some(_) => Act::Answer(StatusCode::Ok),
        };
        self.act(id, handle, raw, act);
    }

    fn fsetstat(&mut self, id: u32, handle: String, attrs: &FileAttributes, raw: &Bytes) {
        let act = match self.local.get_mut(&handle) {
            None => Act::Through(self.remote_files.get(&handle).map(|(path, _)| path.clone())),
            Some(Local::Kept { path, lines: Some(lines), failed: None }) => {
                if let Some(kept) = self.kept.get_mut(path.as_str()) {
                    if let Some(size) = attrs.size {
                        lines.truncate(size);
                        kept.data.resize(usize::try_from(size).unwrap_or(usize::MAX), 0);
                        kept.touch();
                    }
                    if let Some(mtime) = attrs.mtime {
                        kept.attrs.mtime = Some(mtime);
                    }
                }
                Act::Behind
            }
            Some(Local::Kept { failed: Some(code), .. }) => Act::Answer(*code),
            // Open to read only: a size cannot change, and times are let be, as the laptop does.
            Some(Local::Kept { lines: None, .. }) if attrs.size.is_none() => Act::Answer(StatusCode::Ok),
            Some(_) => Act::Answer(StatusCode::PermissionDenied),
        };
        self.act(id, handle, raw, act);
    }

    fn act(&mut self, id: u32, handle: String, raw: &Bytes, act: Act) {
        match act {
            Act::Behind => {
                self.forward(None, raw, Then::Behind(handle));
                self.status(id, StatusCode::Ok);
            }
            Act::Answer(code) => self.status(id, code),
            Act::Through(path) => {
                if let Some(path) = &path {
                    self.forget(path);
                }
                self.forward(Some(id), raw, Then::Changed { forget: path.into_iter().collect(), below: false, kept: KeptChange::None });
            }
        }
    }

    fn setstat(&mut self, id: u32, path: String, attrs: &FileAttributes, raw: &Bytes) {
        let Some(kept) = self.kept.get_mut(&path) else { return self.change(id, raw, vec![path], false, KeptChange::None) };
        // A kept transcript's: applied to the copy, and by the laptop after the writes sent before it.
        if let Some(size) = attrs.size {
            kept.data.resize(usize::try_from(size).unwrap_or(usize::MAX), 0);
            kept.touch();
        }
        if let Some(mtime) = attrs.mtime {
            kept.attrs.mtime = Some(mtime);
        }
        if let Some(mode) = attrs.permissions {
            kept.attrs.permissions = Some(0o100000 | (mode & 0o7777));
        }
        self.forward(None, raw, Then::Behind(String::new()));
        self.status(id, StatusCode::Ok);
    }

    fn change(&mut self, id: u32, raw: &Bytes, paths: Vec<String>, below: bool, kept: KeptChange) {
        self.forget_all(&paths, below);
        self.forward(Some(id), raw, Then::Changed { forget: paths, below, kept });
    }

    fn forget_all(&mut self, paths: &[String], below: bool) {
        self.epoch += 1;
        for path in paths {
            if below { self.cache.forget_below(path) } else { self.cache.forget(path) }
        }
    }

    fn rename_kept(&mut self, from: &str, to: &str) {
        let inside = |p: &str| p == from || p.strip_prefix(from).is_some_and(|rest| rest.starts_with('/'));
        let moved: Vec<String> = self.kept.keys().filter(|p| inside(p)).cloned().collect();
        self.kept.remove(to);
        for path in moved {
            if let Some(kept) = self.kept.remove(&path) {
                self.kept.insert(format!("{to}{}", &path[from.len()..]), kept);
            }
        }
        for local in self.local.values_mut() {
            if let Local::Kept { path, .. } = local
                && inside(path)
            {
                *path = format!("{to}{}", &path[from.len()..]);
            }
        }
    }

    fn from_server(&mut self, raw: Bytes) {
        if raw.first() == Some(&SSH_FXP_VERSION) {
            if let Ok(Packet::Version(version)) = Packet::try_from(&mut raw.clone()) {
                self.version.get_or_insert((version.version, version.extensions));
                if std::mem::take(&mut self.client_waits) {
                    self.init();
                }
            }
            return;
        }
        if raw.len() < 5 {
            return;
        }
        let id = u32::from_be_bytes(raw[1..5].try_into().expect("4 bytes"));
        let Some(pending) = self.pending.remove(&id) else { return };
        let packet = Packet::try_from(&mut raw.clone()).ok();
        let current = pending.epoch == self.epoch;
        match pending.then {
            Then::Pass => {}
            Then::Lstat(path) => match &packet {
                Some(Packet::Attrs(attrs)) if current => {
                    self.cache.attrs.insert(path, Fresh::new(Some(attrs.attrs.clone())));
                }
                Some(Packet::Status(s)) if current && s.status_code == StatusCode::NoSuchFile => {
                    self.cache.attrs.insert(path, Fresh::new(None));
                }
                _ => {}
            },
            Then::OpenDir(path) => {
                if let Some(Packet::Handle(h)) = &packet {
                    self.remote_dirs.insert(h.handle.clone(), (path, vec![], pending.epoch));
                }
            }
            Then::ReadDir(handle) => match &packet {
                Some(Packet::Name(name)) => {
                    if let Some((_, files, _)) = self.remote_dirs.get_mut(&handle) {
                        files.extend(name.files.iter().cloned());
                    }
                }
                Some(Packet::Status(s)) if s.status_code == StatusCode::Eof => {
                    if let Some((path, files, epoch)) = self.remote_dirs.remove(&handle)
                        && epoch == self.epoch
                    {
                        self.cache.listings.insert(path, Fresh::new(files));
                    }
                }
                _ => {
                    self.remote_dirs.remove(&handle);
                }
            },
            Then::Open { path, writes } => {
                if let Some(Packet::Handle(h)) = &packet {
                    self.remote_files.insert(h.handle.clone(), (path.clone(), writes));
                }
                if writes {
                    self.forget(&path);
                }
            }
            Then::Close(handle) => {
                self.remote_dirs.remove(&handle);
                if let Some((path, true)) = self.remote_files.remove(&handle) {
                    self.forget(&path);
                }
            }
            Then::Changed { forget, below, kept } => {
                self.forget_all(&forget, below);
                if is_ok(&packet) {
                    match kept {
                        KeptChange::None => {}
                        KeptChange::Remove(path) => {
                            self.kept.remove(&path);
                        }
                        KeptChange::Rename(from, to) => self.rename_kept(&from, &to),
                    }
                }
            }
            Then::MadeDir(path) => {
                self.forget(&path);
                // A folder just made is empty: what is made in it can be kept without asking.
                if is_ok(&packet) {
                    self.cache.listings.insert(path, Fresh::new(vec![]));
                }
            }
            Then::Behind(handle) => {
                if !is_ok(&packet) {
                    let code = status_of(&packet);
                    if let Some(Local::Kept { failed, .. }) = self.local.get_mut(&handle) {
                        failed.get_or_insert(code);
                    }
                    self.unsaved(code);
                }
            }
            Then::OpenedBehind { handle, path } => {
                if !is_ok(&packet) {
                    // The laptop has no such transcript, so this end keeps none: what claude asks of
                    // it next goes to the laptop, and the handle answers with the laptop's refusal.
                    let code = status_of(&packet);
                    self.kept.remove(&path);
                    self.forget(&path);
                    if let Some(Local::Kept { failed, .. }) = self.local.get_mut(&handle) {
                        failed.get_or_insert(code);
                    }
                    self.unsaved(code);
                }
            }
            Then::FetchOpen(handle) => {
                if !is_ok(&packet) && self.fetching(&handle) {
                    self.end_fetch(false);
                }
            }
            Then::FetchRead(handle, offset) => {
                if self.fetching(&handle) {
                    self.fetched(offset, packet);
                }
            }
        }
        if let Some(client) = pending.client {
            let _ = self.to_client.send(with_id(&raw, client));
        }
    }
}

/// Reads length-prefixed packets (without the length) until the stream ends.
async fn read_packets(mut input: impl AsyncRead + Unpin, packets: mpsc::UnboundedSender<Bytes>) {
    loop {
        let Ok(len) = input.read_u32().await else { return };
        if len == 0 || len > MAX_PACKET {
            return;
        }
        let mut packet = vec![0u8; len as usize];
        if input.read_exact(&mut packet).await.is_err() || packets.send(Bytes::from(packet)).is_err() {
            return;
        }
    }
}

/// Writes packets as they come; what was counted in `queue` is taken off once written.
async fn write_packets(mut output: impl AsyncWrite + Unpin, mut packets: mpsc::UnboundedReceiver<Bytes>, queue: Option<Arc<Queue>>) {
    while let Some(first) = packets.recv().await {
        let mut written = first.len();
        let mut ok = output.write_all(&first).await.is_ok();
        while ok && let Ok(more) = packets.try_recv() {
            written += more.len();
            ok = output.write_all(&more).await.is_ok();
        }
        if !ok || output.flush().await.is_err() {
            return;
        }
        if let Some(queue) = &queue {
            queue.bytes.fetch_sub(written, Ordering::Relaxed);
            queue.drained.notify_one();
        }
    }
}

pub async fn serve(args: &[String]) -> i32 {
    let (Some(socket), Some(warm_file)) = (args.first(), args.get(1)) else {
        eprintln!("claudette: __helper sftp <socket> <warm file> [--writeback]");
        return 2;
    };
    let writeback = args[2..].iter().any(|a| a == "--writeback");
    // A warm start that is missing or damaged only costs round trips.
    let warm = std::fs::read(warm_file).ok().and_then(|bytes| Warm::decode(&bytes)).unwrap_or_default();
    // While this end runs, its warm start is `<file>.held`; the session's end waits for it to go,
    // when nothing more will be sent.
    let held = format!("{warm_file}.held");
    let _ = std::fs::rename(warm_file, &held);
    let code = relay(socket, warm, writeback).await;
    let _ = std::fs::remove_file(held);
    // At once: the runtime would wait for its read of sshfs's stdin, which a stopped session may
    // never close, and sshfs would wait with it.
    std::process::exit(code)
}

async fn relay(socket: &str, warm: Warm, writeback: bool) -> i32 {
    let conn = match UnixStream::connect(socket).await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("claudette: cannot reach the local machine for a mount: {e}");
            return 1;
        }
    };
    let (server_read, server_write) = conn.into_split();
    let (to_client, client_out) = mpsc::unbounded_channel();
    let (to_server, server_out) = mpsc::unbounded_channel();
    let queue = Arc::new(Queue::default());
    let client_writer = tokio::spawn(write_packets(tokio::io::stdout(), client_out, None));
    let server_writer = tokio::spawn(write_packets(server_write, server_out, Some(queue.clone())));
    let (packets, mut from_client) = mpsc::unbounded_channel();
    tokio::spawn(read_packets(tokio::io::stdin(), packets));
    let (packets, mut from_server) = mpsc::unbounded_channel();
    tokio::spawn(read_packets(server_read, packets));

    let mut near = Near::new(warm, writeback, to_client, to_server, queue.clone());
    // The session's process group is signalled when it is stopped. On a TERM (the holder ending a
    // session nobody came back for, or a host stopping claude) this end goes at once, as the plain
    // relay did, once the socket has taken what claude was told is written: the mount breaks, so
    // claude's last writes fail instead of waiting on a laptop that may not be there. An interrupt
    // or a hangup is for claude, which still writes its transcript's last lines: this end carries
    // on until sshfs goes.
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt()), signal(SignalKind::hangup())) else { return 1 };
    let mut until: Option<tokio::time::Instant> = None;
    let terminated = loop {
        tokio::select! {
            // While too much waits for the laptop (a link that is down), sshfs waits too.
            packet = from_client.recv(), if until.is_none() && !near.congested() => match packet {
                Some(packet) => near.from_client(packet),
                // sshfs has gone: what was sent behind is waited for.
                None => until = Some(tokio::time::Instant::now() + DRAIN),
            },
            () = queue.drained.notified(), if near.congested() => {}
            packet = from_server.recv() => match packet {
                Some(packet) => near.from_server(packet),
                None => break false,
            },
            () = async { match until { Some(t) => tokio::time::sleep_until(t).await, None => std::future::pending().await } } => break false,
            _ = term.recv() => break true,
            _ = int.recv() => {}
            _ = hup.recv() => {}
        }
        if until.is_some() && near.idle() {
            break false;
        }
    };
    if !terminated && near.behind() {
        eprintln!("claudette: the end of the session's transcript may not have reached the local machine");
    }
    if std::io::stderr().is_terminal()
        && let Some(warning) = &near.warning
    {
        eprintln!("{warning}");
    }
    drop(near);
    if terminated {
        let _ = tokio::time::timeout(Duration::from_secs(1), server_writer).await;
        return 143;
    }
    let _ = client_writer.await;
    let _ = server_writer.await;
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::warm::{Plan, snapshot};
    use russh_sftp::protocol::{Lstat, Open, SetStat, Write};
    use tokio::io::DuplexStream;

    const APPEND: OpenFlags = OpenFlags::WRITE.union(OpenFlags::CREATE).union(OpenFlags::APPEND);

    /// A near end in front of a real server, on an in-memory connection.
    struct Rig {
        near: Near,
        client: mpsc::UnboundedReceiver<Bytes>,
        server: mpsc::UnboundedReceiver<Bytes>,
        conn: DuplexStream,
        next: u32,
    }

    impl Rig {
        fn new(warm: Warm, writeback: bool, server: impl russh_sftp::server::Handler + Send + 'static) -> Rig {
            let (conn, end) = tokio::io::duplex(1 << 22);
            tokio::spawn(russh_sftp::server::run(end, server));
            let (to_client, client) = mpsc::unbounded_channel();
            let (to_server, server) = mpsc::unbounded_channel();
            Rig { near: Near::new(warm, writeback, to_client, to_server, Arc::default()), client, server, conn, next: 100 }
        }

        /// Sends sshfs's request; returns its id and how many requests it sent to the laptop.
        fn ask(&mut self, packet: impl FnOnce(u32) -> Packet) -> (u32, usize) {
            self.next += 1;
            let mut raw = Bytes::try_from(packet(self.next)).unwrap();
            let _ = raw.split_to(4);
            let before = self.server.len();
            self.near.from_client(raw);
            (self.next, self.server.len() - before)
        }

        /// Carries requests to the server and its answers back until nothing is waiting.
        async fn settle(&mut self) {
            loop {
                while let Ok(bytes) = self.server.try_recv() {
                    self.conn.write_all(&bytes).await.unwrap();
                }
                if self.near.idle() {
                    return;
                }
                let len = self.conn.read_u32().await.unwrap();
                let mut packet = vec![0u8; len as usize];
                self.conn.read_exact(&mut packet).await.unwrap();
                // The greeting's answer, then everything else.
                self.near.from_server(Bytes::from(packet));
            }
        }

        /// The answers sshfs has now, decoded.
        fn answers(&mut self) -> Vec<Packet> {
            let mut out = vec![];
            while let Ok(mut bytes) = self.client.try_recv() {
                let _ = bytes.split_to(4);
                out.push(Packet::try_from(&mut bytes).unwrap());
            }
            out
        }

        /// The one answer sshfs has now.
        fn answer(&mut self) -> Packet {
            let mut answers = self.answers();
            assert_eq!(answers.len(), 1, "{answers:?}");
            answers.remove(0)
        }
    }

    fn handle(packet: Packet) -> String {
        match packet {
            Packet::Handle(h) => h.handle,
            other => panic!("not a handle: {other:?}"),
        }
    }

    fn code(packet: Packet) -> StatusCode {
        match packet {
            Packet::Status(s) => s.status_code,
            other => panic!("not a status: {other:?}"),
        }
    }

    fn sessions(name: &str) -> (std::path::PathBuf, std::path::PathBuf, crate::mount::sftp::Sessions) {
        let base = std::env::temp_dir().join(format!("ct-near-{name}-{}", std::process::id()));
        let root = base.join("key");
        std::fs::create_dir_all(root.join("memory")).unwrap();
        std::fs::write(root.join("old.jsonl"), "{\"a\":1}\n").unwrap();
        let server = crate::mount::sftp::Sessions::new(&root, std::sync::Arc::new(PathMap::new(std::path::Path::new("/Users/me/app")))).unwrap();
        (base, root, server)
    }

    fn warm_of(server: &mut crate::mount::sftp::Sessions) -> Warm {
        let parts = PathMap::parts(std::path::Path::new("/Users/me/app"));
        Warm::decode(&snapshot(server, &Plan { list: &["/"], tree: &["/memory"], files: vec![] }, Some(&parts))).unwrap()
    }

    #[tokio::test]
    async fn transcripts_are_written_behind_and_end_up_as_the_laptop_writes_them() {
        let (base, root, mut server) = sessions("behind");
        let warm = warm_of(&mut server);
        let mut rig = Rig::new(warm, true, server);
        rig.settle().await;
        let remote = format!("/home/remote/{}", crate::mount::workdir::remote_workdir(std::path::Path::new("/Users/me/app")));
        let line = format!("{{\"cwd\":\"{remote}\",\"file\":\"{remote}/src/a.rs\"}}\n");
        let mapped = "{\"cwd\":\"/Users/me/app\",\"file\":\"/Users/me/app/src/a.rs\"}\n";

        // The greeting, the lookup of a new transcript: answered here, nothing asked.
        rig.ask(|_| Packet::Init(Init { version: 3, extensions: HashMap::new() }));
        assert!(matches!(rig.answer(), Packet::Version(_)));
        let (_, sent) = rig.ask(|id| Packet::Lstat(Lstat { id, path: "/s.jsonl".into() }));
        assert_eq!((code(rig.answer()), sent), (StatusCode::NoSuchFile, 0));

        // Its creation, writes and close are answered at once and sent behind.
        let mut behind = 0;
        let (_, sent) = rig.ask(|id| Packet::Open(Open { id, filename: "/s.jsonl".into(), pflags: APPEND, attrs: FileAttributes::empty() }));
        behind += sent;
        let h = handle(rig.answer());
        let (first, second) = line.split_at(20);
        for part in [first, second] {
            let (_, sent) = rig.ask(|id| Packet::Write(Write { id, handle: h.clone(), offset: 0, data: part.as_bytes().to_vec() }));
            behind += sent;
            assert_eq!(code(rig.answer()), StatusCode::Ok);
        }
        rig.ask(|id| Packet::Lstat(Lstat { id, path: "/s.jsonl".into() }));
        assert!(matches!(rig.answer(), Packet::Attrs(a) if a.attrs.size == Some(mapped.len() as u64)));
        let (_, sent) = rig.ask(|id| Packet::Close(Close { id, handle: h }));
        behind += sent;
        assert_eq!(code(rig.answer()), StatusCode::Ok);
        assert_eq!(behind, 4, "the open, two writes and the close");

        // Read back here, as the laptop will have it.
        rig.ask(|id| Packet::Open(Open { id, filename: "/s.jsonl".into(), pflags: OpenFlags::READ, attrs: FileAttributes::empty() }));
        let r = handle(rig.answer());
        let (_, sent) = rig.ask(|id| Packet::Read(Read { id, handle: r, offset: 0, len: 4096 }));
        assert!(matches!(rig.answer(), Packet::Data(d) if d.data == mapped.as_bytes()));
        assert_eq!(sent, 0);

        rig.settle().await;
        assert!(rig.answers().is_empty(), "the laptop's answers to writes behind are not passed on");
        assert_eq!(std::fs::read_to_string(root.join("s.jsonl")).unwrap(), mapped);

        // A transcript the laptop has (being resumed): fetched first, then kept.
        rig.ask(|id| Packet::Open(Open { id, filename: "/old.jsonl".into(), pflags: APPEND, attrs: FileAttributes::empty() }));
        assert!(rig.answers().is_empty(), "the open waits for the fetch");
        rig.settle().await;
        let h = handle(rig.answer());
        rig.ask(|id| Packet::Write(Write { id, handle: h.clone(), offset: 8, data: line.as_bytes().to_vec() }));
        assert_eq!(code(rig.answer()), StatusCode::Ok);
        rig.ask(|id| Packet::Close(Close { id, handle: h }));
        assert_eq!(code(rig.answer()), StatusCode::Ok);
        rig.settle().await;
        let laptop = std::fs::read_to_string(root.join("old.jsonl")).unwrap();
        assert_eq!(laptop, format!("{{\"a\":1}}\n{mapped}"));
        assert_eq!(rig.near.kept["/old.jsonl"].data, laptop.as_bytes());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn what_is_learned_answers_until_a_change_through_the_mount_drops_it() {
        let (base, root, mut server) = sessions("cache");
        std::fs::write(root.join("memory/MEMORY.md"), "remember").unwrap();
        let warm = warm_of(&mut server);
        let mut rig = Rig::new(warm, false, server);
        rig.settle().await;

        // Warm: the memory file's contents, and names absent from a listing.
        let (_, sent) = rig.ask(|id| Packet::Open(Open { id, filename: "/memory/MEMORY.md".into(), pflags: OpenFlags::READ, attrs: FileAttributes::empty() }));
        let h = handle(rig.answer());
        rig.ask(|id| Packet::Read(Read { id, handle: h, offset: 0, len: 100 }));
        assert!(matches!(rig.answer(), Packet::Data(d) if d.data == b"remember"));
        let (_, sent2) = rig.ask(|id| Packet::Lstat(Lstat { id, path: "/memory/other.md".into() }));
        assert_eq!((code(rig.answer()), sent + sent2), (StatusCode::NoSuchFile, 0));

        // Not warm: asked once, then known.
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/x"), "x").unwrap();
        let (_, sent) = rig.ask(|id| Packet::Lstat(Lstat { id, path: "/sub/x".into() }));
        assert_eq!(sent, 1);
        rig.settle().await;
        rig.answers();
        let (_, sent) = rig.ask(|id| Packet::Lstat(Lstat { id, path: "/sub/x".into() }));
        assert_eq!(sent, 0);
        rig.answers();

        // A change through the mount: what it touches is asked again.
        rig.ask(|id| Packet::SetStat(SetStat { id, path: "/sub/x".into(), attrs: FileAttributes { size: Some(0), ..FileAttributes::default() } }));
        rig.settle().await;
        assert_eq!(code(rig.answer()), StatusCode::Ok);
        let (_, sent) = rig.ask(|id| Packet::Lstat(Lstat { id, path: "/sub/x".into() }));
        assert_eq!(sent, 1);
        rig.settle().await;
        assert!(matches!(rig.answer(), Packet::Attrs(a) if a.attrs.size == Some(0)));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn a_transcript_the_laptop_refuses_to_open_is_not_kept() {
        let (base, root, mut server) = sessions("refused");
        let warm = warm_of(&mut server);
        let mut rig = Rig::new(warm, true, server);
        rig.settle().await;
        // The folder goes on the laptop after the warm start said it was there.
        std::fs::remove_dir_all(root.join("memory")).unwrap();
        rig.ask(|id| Packet::Open(Open { id, filename: "/memory/s.jsonl".into(), pflags: APPEND, attrs: FileAttributes::empty() }));
        let h = handle(rig.answer());
        rig.settle().await;

        // The handle answers with the laptop's refusal, and the transcript is the laptop's to show.
        rig.ask(|id| Packet::Write(Write { id, handle: h, offset: 0, data: b"{}\n".to_vec() }));
        assert_ne!(code(rig.answer()), StatusCode::Ok);
        rig.ask(|id| Packet::Lstat(Lstat { id, path: "/memory/s.jsonl".into() }));
        rig.settle().await;
        assert_eq!(code(rig.answer()), StatusCode::NoSuchFile);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn a_transcript_larger_than_one_read_is_fetched_whole_even_if_it_grew() {
        let (base, root, mut server) = sessions("fetch");
        let lines = |from: usize, to: usize| (from..to).map(|i| format!("{{\"n\":{i}}}\n")).collect::<String>();
        std::fs::write(root.join("old.jsonl"), lines(0, 300_000)).unwrap();
        let warm = warm_of(&mut server);
        // Longer than the warm start says, by more than a read.
        let mut file = std::fs::OpenOptions::new().append(true).open(root.join("old.jsonl")).unwrap();
        std::io::Write::write_all(&mut file, lines(300_000, 400_000).as_bytes()).unwrap();
        let mut rig = Rig::new(warm, true, server);
        rig.settle().await;

        rig.ask(|id| Packet::Open(Open { id, filename: "/old.jsonl".into(), pflags: APPEND, attrs: FileAttributes::empty() }));
        assert!(rig.answers().is_empty(), "the open waits for the fetch");
        rig.settle().await;
        handle(rig.answer());
        assert!(rig.near.kept["/old.jsonl"].data == std::fs::read(root.join("old.jsonl")).unwrap(), "kept as the laptop has it");
        std::fs::remove_dir_all(base).unwrap();
    }
}
