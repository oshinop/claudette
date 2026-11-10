//! A mount's warm start: what the remote end of the mount (the near cache, see
//! [`crate::helper::near`]) knows before claude asks for anything. Made here, at the start of a run,
//! by asking the mount's own SFTP server, so hidden names, settings views and plugin manifests are
//! exactly what the mount serves; sent to the remote machine with the run's first frames.
//!
//! It holds the server's greeting, complete folder listings (which also say what is not there),
//! attributes, and the contents of small files: what claude reads first from each mount, and what
//! would otherwise cost it a round trip each, one after another.

use std::collections::{HashMap, VecDeque};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures_util::FutureExt;
use russh_sftp::protocol::{File, FileAttributes, OpenFlags, StatusCode};
use russh_sftp::server::Handler;

/// Files up to this size have their contents included, when they are of the kinds Claude Code reads
/// at start: markdown (skills, agents, commands, memory) and JSON (settings, manifests, hooks).
const FILE_MAX: u64 = 64 * 1024;
/// Contents of small files, in all, per mount: every byte of a warm start rides the session's start.
const CONTENT_BUDGET: usize = 256 * 1024;
/// Listings and small files together, per mount: no more folders are listed past this.
const WARM_MAX: usize = 1 << 20;
/// A folder with more entries than this is left to be listed on demand.
const LISTING_MAX: usize = 2000;
/// Folders listed, per mount.
const FOLDERS_MAX: usize = 500;
/// The files named in [`Plan::files`] (a transcript being resumed) may be this large together. The
/// whole warm start travels in one frame of at most 16 MiB.
const NAMED_FILE_MAX: u64 = 8 << 20;

const VERSION: u8 = 1;
const ATTRS: u8 = 2;
const MISSING: u8 = 3;
const LISTING: u8 = 4;
const CONTENT: u8 = 5;
const PATHS: u8 = 6;

/// What a mount's warm start covers.
#[derive(Default)]
pub struct Plan<'a> {
    /// Folders listed (every entry's attributes), not descended into.
    pub list: &'a [&'a str],
    /// Folders listed with everything below them, small files' contents included.
    pub tree: &'a [&'a str],
    /// Files whose attributes and contents are included, up to [`NAMED_FILE_MAX`].
    pub files: Vec<String>,
}

/// The parts of a [`crate::mount::workdir::PathMap`], for a remote end that writes transcripts the
/// way this machine's sessions server does.
#[derive(Debug, Clone, PartialEq)]
pub struct PathParts {
    pub workdir: String,
    pub local: String,
    pub sessions_local: String,
    pub user_local: String,
}

/// A decoded warm start.
#[derive(Default)]
pub struct Warm {
    /// The server's answer to INIT: its version and extensions.
    pub version: Option<(u32, HashMap<String, String>)>,
    /// lstat results; `None`: no such file.
    pub attrs: Vec<(String, Option<FileAttributes>)>,
    pub listings: Vec<(String, Vec<File>)>,
    pub contents: Vec<(String, Vec<u8>)>,
    pub paths: Option<PathParts>,
}

/// Runs a handler call to its end. The servers answer from the file system without waiting on
/// anything, so their futures are ready at once; one that is not is left out of the warm start.
fn now<T, E>(call: impl Future<Output = Result<T, E>>) -> Option<T> {
    call.now_or_never().and_then(Result::ok)
}

fn child(dir: &str, name: &str) -> String {
    if dir == "/" { format!("/{name}") } else { format!("{dir}/{name}") }
}

fn put_str(out: &mut BytesMut, s: &[u8]) {
    out.put_u32(s.len() as u32);
    out.put_slice(s);
}

fn put_attrs(out: &mut BytesMut, attrs: &FileAttributes) {
    let bytes = russh_sftp::ser::to_bytes(attrs).unwrap_or_default();
    put_str(out, &bytes);
}

/// Reads a whole file through the server, up to `max` bytes; `None` if it is larger or unreadable.
fn read_all<H: Handler>(handler: &mut H, path: &str, max: u64) -> Option<Vec<u8>> {
    let handle = now(handler.open(0, path.to_owned(), OpenFlags::READ, FileAttributes::empty()))?.handle;
    let mut data = Vec::new();
    let complete = loop {
        match handler.read(0, handle.clone(), data.len() as u64, 64 * 1024).now_or_never() {
            Some(Ok(chunk)) if !chunk.data.is_empty() => {
                data.extend_from_slice(&chunk.data);
                if data.len() as u64 > max {
                    break false;
                }
            }
            // An empty read, or the server's EOF: the end. Any other error leaves it incomplete.
            Some(Ok(_)) => break true,
            Some(Err(e)) => break e.into().status_code == StatusCode::Eof,
            None => break false,
        }
    };
    let _ = now(handler.close(0, handle));
    complete.then_some(data)
}

/// Lists a folder through the server; `None` if it cannot be listed or is too big to send.
fn list<H: Handler>(handler: &mut H, path: &str) -> Option<Vec<File>> {
    let handle = now(handler.opendir(0, path.to_owned()))?.handle;
    let mut files = Vec::new();
    let complete = loop {
        match handler.readdir(0, handle.clone()).now_or_never() {
            Some(Ok(name)) => {
                files.extend(name.files.into_iter().map(|f| File { filename: f.filename, longname: String::new(), attrs: f.attrs }));
                if files.len() > LISTING_MAX {
                    break false;
                }
            }
            // EOF, the only way a listing ends; any other error leaves it incomplete.
            Some(Err(e)) => break e.into().status_code == StatusCode::Eof,
            None => break false,
        }
    };
    let _ = now(handler.close(0, handle));
    complete.then_some(files)
}

/// Asks `handler` for what `plan` covers and encodes the answers.
pub fn snapshot<H: Handler>(handler: &mut H, plan: &Plan, paths: Option<&PathParts>) -> Vec<u8> {
    crate::mount::sftp::quietly(|| encode(handler, plan, paths))
}

fn encode<H: Handler>(handler: &mut H, plan: &Plan, paths: Option<&PathParts>) -> Vec<u8> {
    let mut out = BytesMut::new();
    if let Some(version) = now(handler.init(3, HashMap::new())) {
        out.put_u8(VERSION);
        out.put_u32(version.version);
        out.put_u32(version.extensions.len() as u32);
        for (name, value) in &version.extensions {
            put_str(&mut out, name.as_bytes());
            put_str(&mut out, value.as_bytes());
        }
    }
    if let Some(parts) = paths {
        out.put_u8(PATHS);
        for s in [&parts.workdir, &parts.local, &parts.sessions_local, &parts.user_local] {
            put_str(&mut out, s.as_bytes());
        }
    }
    let lstat = |handler: &mut H, out: &mut BytesMut, path: &str| -> Option<FileAttributes> {
        match handler.lstat(0, path.to_owned()).now_or_never() {
            Some(Ok(reply)) => {
                out.put_u8(ATTRS);
                put_str(out, path.as_bytes());
                put_attrs(out, &reply.attrs);
                Some(reply.attrs)
            }
            Some(Err(e)) => {
                if Into::<russh_sftp::server::StatusReply>::into(e).status_code == StatusCode::NoSuchFile {
                    out.put_u8(MISSING);
                    put_str(out, path.as_bytes());
                }
                None
            }
            None => None,
        }
    };
    lstat(handler, &mut out, "/");
    let mut budget = CONTENT_BUDGET;
    let mut folders: VecDeque<(String, bool)> = plan.list.iter().map(|p| ((*p).to_owned(), false)).chain(plan.tree.iter().map(|p| ((*p).to_owned(), true))).collect();
    let mut listed = 0;
    while let Some((dir, descend)) = folders.pop_front() {
        if listed >= FOLDERS_MAX || out.len() > WARM_MAX {
            break;
        }
        if dir != "/" && lstat(handler, &mut out, &dir).is_none_or(|a| !a.is_dir()) {
            continue;
        }
        let Some(files) = list(handler, &dir) else { continue };
        listed += 1;
        out.put_u8(LISTING);
        put_str(&mut out, dir.as_bytes());
        out.put_u32(files.len() as u32);
        for file in &files {
            put_str(&mut out, file.filename.as_bytes());
            put_attrs(&mut out, &file.attrs);
        }
        if !descend {
            continue;
        }
        for file in files {
            let path = child(&dir, &file.filename);
            if file.attrs.is_dir() {
                folders.push_back((path, true));
            } else if file.attrs.is_regular() && file.attrs.len() <= FILE_MAX && (file.attrs.len() as usize) <= budget
                && (file.filename.ends_with(".md") || file.filename.ends_with(".json"))
                && let Some(data) = read_all(handler, &path, FILE_MAX)
            {
                budget -= data.len().min(budget);
                out.put_u8(CONTENT);
                put_str(&mut out, path.as_bytes());
                put_str(&mut out, &data);
            }
        }
    }
    let mut named = NAMED_FILE_MAX;
    for path in &plan.files {
        if lstat(handler, &mut out, path).is_some_and(|a| a.is_regular() && a.len() <= named)
            && let Some(data) = read_all(handler, path, named)
        {
            named -= data.len() as u64;
            out.put_u8(CONTENT);
            put_str(&mut out, path.as_bytes());
            put_str(&mut out, &data);
        }
    }
    out.to_vec()
}

fn get_str(input: &mut Bytes) -> Option<Bytes> {
    if input.remaining() < 4 {
        return None;
    }
    let len = input.get_u32() as usize;
    (input.remaining() >= len).then(|| input.split_to(len))
}

fn get_string(input: &mut Bytes) -> Option<String> {
    String::from_utf8(get_str(input)?.to_vec()).ok()
}

fn get_attrs(input: &mut Bytes) -> Option<FileAttributes> {
    let mut bytes = get_str(input)?;
    russh_sftp::de::from_bytes(&mut bytes).ok()
}

impl Warm {
    /// Decodes a warm start; `None` if it is damaged.
    pub fn decode(bytes: &[u8]) -> Option<Warm> {
        let mut input = Bytes::copy_from_slice(bytes);
        let mut warm = Warm::default();
        while input.has_remaining() {
            match input.get_u8() {
                VERSION => {
                    if input.remaining() < 8 {
                        return None;
                    }
                    let version = input.get_u32();
                    let count = input.get_u32();
                    let mut extensions = HashMap::new();
                    for _ in 0..count {
                        extensions.insert(get_string(&mut input)?, get_string(&mut input)?);
                    }
                    warm.version = Some((version, extensions));
                }
                ATTRS => {
                    let path = get_string(&mut input)?;
                    warm.attrs.push((path, Some(get_attrs(&mut input)?)));
                }
                MISSING => warm.attrs.push((get_string(&mut input)?, None)),
                LISTING => {
                    let path = get_string(&mut input)?;
                    if input.remaining() < 4 {
                        return None;
                    }
                    let count = input.get_u32();
                    let mut files = Vec::new();
                    for _ in 0..count {
                        let filename = get_string(&mut input)?;
                        files.push(File { filename, longname: String::new(), attrs: get_attrs(&mut input)? });
                    }
                    warm.listings.push((path, files));
                }
                CONTENT => {
                    let path = get_string(&mut input)?;
                    warm.contents.push((path, get_str(&mut input)?.to_vec()));
                }
                PATHS => {
                    warm.paths = Some(PathParts {
                        workdir: get_string(&mut input)?,
                        local: get_string(&mut input)?,
                        sessions_local: get_string(&mut input)?,
                        user_local: get_string(&mut input)?,
                    });
                }
                _ => return None,
            }
        }
        Some(warm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_warm_start_is_what_the_server_answers() {
        let base = std::env::temp_dir().join(format!("ct-warm-{}", std::process::id()));
        let dir = base.join("p");
        std::fs::create_dir_all(dir.join(".claude/skills/s")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".claude/skills/s/SKILL.md"), "skill").unwrap();
        std::fs::write(dir.join(".claude/big.bin"), vec![0u8; (FILE_MAX + 1) as usize]).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn a() {}").unwrap();
        let mut project = crate::mount::sftp::Project::new(&dir, None, false).unwrap();
        let plan = Plan { list: &["/"], tree: &["/.claude"], files: vec!["/src/a.rs".into(), "/nope".into()] };
        let parts = PathParts { workdir: "w".into(), local: "l".into(), sessions_local: "s".into(), user_local: "u".into() };
        let warm = Warm::decode(&snapshot(&mut project, &plan, Some(&parts))).unwrap();

        assert_eq!(warm.paths, Some(parts));
        assert!(warm.version.is_some());
        let names = |path: &str| {
            let mut names: Vec<String> = warm.listings.iter().find(|(p, _)| p == path).unwrap().1.iter().map(|f| f.filename.clone()).collect();
            names.sort();
            names
        };
        // Hidden names stay hidden: the listing is the server's.
        assert_eq!(names("/"), [".claude", "src"]);
        assert_eq!(names("/.claude/skills/s"), ["SKILL.md"]);
        // `/src` is listed only as an entry of `/`; it is not descended into.
        assert!(!warm.listings.iter().any(|(p, _)| p == "/src"));
        let content = |path: &str| warm.contents.iter().find(|(p, _)| p == path).map(|(_, c)| c.clone());
        assert_eq!(content("/.claude/skills/s/SKILL.md").as_deref(), Some(&b"skill"[..]));
        assert_eq!(content("/.claude/big.bin"), None);
        assert_eq!(content("/src/a.rs").as_deref(), Some(&b"fn a() {}"[..]));
        assert!(warm.attrs.iter().any(|(p, a)| p == "/nope" && a.is_none()));
        std::fs::remove_dir_all(base).unwrap();
    }
}
