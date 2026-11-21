use std::cell::Cell;
use std::collections::HashMap;
use std::fs::{File as FsFile, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode};
use serde::{Deserialize, Serialize};

use super::visible::Visibility;
use crate::plugins::{self, Plugin};
use crate::settings::{Callbacks, SETTINGS_FILES};

const HIDDEN: [&str; 2] = [".git", "node_modules"];

fn is_writable(relative: &Path) -> bool {
    let all: Vec<&str> = relative
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let Some(name) = all.last() else { return false };
    all.contains(&".claude") || (all.len() == 1 && name.starts_with(".mcp.json")) || (name.starts_with("CLAUDE") && name.contains(".md"))
}

pub(crate) fn io_status(e: &std::io::Error) -> StatusCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

pub(crate) fn ok(id: u32) -> Status {
    Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() }
}

thread_local! {
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn note(op: &str, path: &str) {
    if crate::log::enabled() && !QUIET.get() {
        crate::log::trace("sftp", &format!("{op} {path}"));
    }
}

pub(crate) fn quietly<T>(f: impl FnOnce() -> T) -> T {
    QUIET.set(true);
    let out = f();
    QUIET.set(false);
    out
}

const READDIR_BATCH: usize = 100;
const READDIR_BYTES: usize = 64 * 1024;

pub(crate) fn next_batch(listing: &mut Vec<File>) -> Option<Vec<File>> {
    if listing.is_empty() {
        return None;
    }
    let mut bytes = 0;
    let count = listing
        .iter()
        .take(READDIR_BATCH)
        .take_while(|f| {
            bytes += 8 + f.filename.len() + f.longname.len() + 64;
            bytes <= READDIR_BYTES
        })
        .count()
        .max(1);
    let rest = listing.split_off(count);
    Some(std::mem::replace(listing, rest))
}

enum Open {
    File(FsFile),
    Dir(Vec<File>),
    Settings { local: PathBuf, data: Vec<u8>, written: bool },
}

pub struct Project {
    root: PathBuf,
    settings: Option<Arc<Callbacks>>,
    visible: Visibility,
    handles: HashMap<String, Open>,
    next: u64,
}

impl Project {
    pub fn new(root: &Path, settings: Option<Arc<Callbacks>>, respect_gitignore: bool) -> std::io::Result<Project> {
        let root = root.canonicalize()?;
        let visible = Visibility::new(&root, respect_gitignore);
        Ok(Project { root, settings, visible, handles: HashMap::new(), next: 0 })
    }

    fn shows(&self, relative: &Path, is_dir: bool) -> bool {
        is_writable(relative) || self.visible.shows(relative, is_dir)
    }

    fn is_settings(&self, local: &Path) -> bool {
        self.settings.is_some() && local.strip_prefix(&self.root).is_ok_and(|r| SETTINGS_FILES.iter().any(|f| r == Path::new(f)))
    }

    fn relative(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let mut out = PathBuf::new();
        for component in Path::new(path).components() {
            match component {
                Component::Normal(name) => {
                    if HIDDEN.iter().any(|h| name == *h) {
                        return Err(StatusCode::NoSuchFile);
                    }
                    out.push(name);
                }
                Component::ParentDir => {
                    out.pop();
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let relative = self.relative(path)?;
        let local = self.root.join(&relative);
        let real = local.canonicalize().map_err(|e| io_status(&e))?;
        let inside = real.strip_prefix(&self.root).map_err(|_| StatusCode::NoSuchFile)?;
        if inside.components().any(|c| HIDDEN.iter().any(|h| c.as_os_str() == *h)) {
            return Err(StatusCode::NoSuchFile);
        }
        let is_dir = real.is_dir();
        if !self.shows(&relative, is_dir) || !self.shows(inside, is_dir) {
            return Err(StatusCode::NoSuchFile);
        }
        Ok(real)
    }

    fn resolve_entry(&self, path: &str) -> Result<(PathBuf, PathBuf), StatusCode> {
        let relative = self.relative(path)?;
        let Some(name) = relative.file_name().map(ToOwned::to_owned) else { return Ok((self.root.clone(), relative)) };
        let parent = self.resolve(&relative.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default())?;
        let local = parent.join(name);
        let is_dir = std::fs::symlink_metadata(&local).is_ok_and(|m| m.is_dir());
        if !self.shows(&relative, is_dir) {
            return Err(StatusCode::NoSuchFile);
        }
        Ok((local, relative))
    }

    fn writable(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let (local, relative) = self.resolve_entry(path)?;
        let inside = local.strip_prefix(&self.root).map(Path::to_path_buf).unwrap_or(relative);
        if is_writable(&inside) { Ok(local) } else { Err(StatusCode::PermissionDenied) }
    }

    fn attrs(&self, local: &Path, meta: &Metadata) -> FileAttributes {
        let mut attrs = FileAttributes::from(meta);
        attrs.uid = None;
        attrs.gid = None;
        attrs.user = None;
        attrs.group = None;
        let relative = local.strip_prefix(&self.root).unwrap_or(local);
        let allow_write = is_writable(relative) || (meta.is_dir() && relative.as_os_str().is_empty());
        if !allow_write && let Some(mode) = attrs.permissions {
            attrs.permissions = Some(mode & !0o222);
        }
        if meta.is_file()
            && self.is_settings(local)
            && let (Some(settings), Ok(bytes)) = (&self.settings, std::fs::read(local))
        {
            attrs.size = Some(settings.view(&bytes).len() as u64);
        }
        attrs
    }

    fn handle(&mut self, open: Open) -> String {
        self.next += 1;
        let handle = self.next.to_string();
        self.handles.insert(handle.clone(), open);
        handle
    }

    fn file(&mut self, handle: &str) -> Result<&mut FsFile, StatusCode> {
        match self.handles.get_mut(handle) {
            Some(Open::File(file)) => Ok(file),
            _ => Err(StatusCode::Failure),
        }
    }

    fn set_attrs(local: &Path, file: Option<&FsFile>, attrs: &FileAttributes) -> Result<(), StatusCode> {
        let owned;
        let file = match file {
            Some(f) => f,
            None => {
                owned = OpenOptions::new().write(attrs.size.is_some()).read(true).open(local).map_err(|e| io_status(&e))?;
                &owned
            }
        };
        if let Some(size) = attrs.size {
            file.set_len(size).map_err(|e| io_status(&e))?;
        }
        if let Some(mtime) = attrs.mtime {
            let _ = file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime.into()));
        }
        Ok(())
    }
}

impl russh_sftp::server::Handler for Project {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        note("realpath", &path);
        let relative = self.relative(&path)?;
        Ok(Name { id, files: vec![File::dummy(format!("/{}", relative.to_string_lossy()))] })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("stat", &path);
        let local = self.resolve(&path)?;
        let meta = std::fs::metadata(&local).map_err(|e| io_status(&e))?;
        Ok(Attrs { id, attrs: self.attrs(&local, &meta) })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("lstat", &path);
        let (local, _) = self.resolve_entry(&path)?;
        let meta = std::fs::symlink_metadata(&local).map_err(|e| io_status(&e))?;
        Ok(Attrs { id, attrs: self.attrs(&local, &meta) })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        if let Some(Open::Settings { local, data, .. }) = self.handles.get(&handle) {
            let meta = std::fs::metadata(local).map_err(|e| io_status(&e))?;
            let mut attrs = FileAttributes::from(&meta);
            attrs.size = Some(data.len() as u64);
            return Ok(Attrs { id, attrs });
        }
        let meta = self.file(&handle)?.metadata().map_err(|e| io_status(&e))?;
        Ok(Attrs { id, attrs: FileAttributes::from(&meta) })
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        note("readlink", &path);
        let (local, _) = self.resolve_entry(&path)?;
        let target = std::fs::read_link(&local).map_err(|e| io_status(&e))?;
        Ok(Name { id, files: vec![File::dummy(target.to_string_lossy())] })
    }

    async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, attrs: FileAttributes) -> Result<Handle, Self::Error> {
        note("open", &filename);
        if let Some(settings) = self.settings.clone()
            && let Ok((local, _)) = self.resolve_entry(&filename)
            && self.is_settings(&local)
        {
            let writes = pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE);
            let existing = std::fs::read(&local);
            if existing.is_err() && !pflags.contains(OpenFlags::CREATE) {
                return Err(StatusCode::NoSuchFile);
            }
            if existing.is_ok() && pflags.contains(OpenFlags::EXCLUDE) {
                return Err(StatusCode::Failure);
            }
            let data = match existing {
                Ok(bytes) if !pflags.contains(OpenFlags::TRUNCATE) => settings.view(&bytes),
                _ => Vec::new(),
            };
            if writes && !local.exists() {
                std::fs::write(&local, b"").map_err(|e| io_status(&e))?;
            }
            return Ok(Handle { id, handle: self.handle(Open::Settings { local, data, written: pflags.contains(OpenFlags::TRUNCATE) }) });
        }
        let writes = pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE);
        let file = if writes {
            let local = self.writable(&filename)?;
            let file = OpenOptions::from(pflags).open(&local).map_err(|e| io_status(&e))?;
            if let Some(mode) = attrs.permissions.filter(|_| pflags.contains(OpenFlags::CREATE)) {
                use std::os::unix::fs::PermissionsExt;
                let _ = file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777));
            }
            file
        } else {
            let local = self.resolve(&filename)?;
            FsFile::open(&local).map_err(|e| io_status(&e))?
        };
        Ok(Handle { id, handle: self.handle(Open::File(file)) })
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        note("read", &handle);
        if let Some(Open::Settings { data, .. }) = self.handles.get(&handle) {
            let start = (offset as usize).min(data.len());
            let end = (start + len as usize).min(data.len());
            if start == end && len > 0 {
                return Err(StatusCode::Eof);
            }
            return Ok(Data { id, data: data[start..end].to_vec() });
        }
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset)).map_err(|e| io_status(&e))?;
        let mut data = vec![0u8; len as usize];
        let mut filled = 0;
        while filled < data.len() {
            match file.read(&mut data[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => return Err(io_status(&e)),
            }
        }
        if filled == 0 && len > 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(filled);
        Ok(Data { id, data })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        if let Some(Open::Settings { data: buffer, written, .. }) = self.handles.get_mut(&handle) {
            let start = offset as usize;
            if buffer.len() < start + data.len() {
                buffer.resize(start + data.len(), 0);
            }
            buffer[start..start + data.len()].copy_from_slice(&data);
            *written = true;
            return Ok(ok(id));
        }
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset)).map_err(|e| io_status(&e))?;
        file.write_all(&data).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if let Some(Open::Settings { local, data, written: true }) = self.handles.remove(&handle)
            && let Some(settings) = &self.settings
        {
            std::fs::write(&local, settings.restore_file(&data, &local)).map_err(|e| io_status(&e))?;
        }
        Ok(ok(id))
    }

    async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        let local = self.writable(&path)?;
        if self.is_settings(&local) && attrs.size.is_some_and(|s| s != 0) {
            return Ok(ok(id));
        }
        Self::set_attrs(&local, None, &attrs)?;
        Ok(ok(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        if let Some(Open::Settings { data, written, .. }) = self.handles.get_mut(&handle) {
            if let Some(size) = attrs.size {
                data.resize(size as usize, 0);
                *written = true;
            }
            return Ok(ok(id));
        }
        let file = self.file(&handle)?;
        let file = file.try_clone().map_err(|e| io_status(&e))?;
        Self::set_attrs(Path::new(""), Some(&file), &attrs)?;
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        note("opendir", &path);
        let local = self.resolve(&path)?;
        let inside = local.strip_prefix(&self.root).unwrap_or(Path::new("")).to_path_buf();
        let mut files = vec![];
        for entry in std::fs::read_dir(&local).map_err(|e| io_status(&e))?.flatten() {
            let name = entry.file_name();
            if HIDDEN.iter().any(|h| name == *h) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if !self.shows(&inside.join(&name), meta.is_dir()) {
                continue;
            }
            files.push(File::new(name.to_string_lossy(), self.attrs(&entry.path(), &meta)));
        }
        Ok(Handle { id, handle: self.handle(Open::Dir(files)) })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        note("readdir", &handle);
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(listing)) => match next_batch(listing) {
                Some(files) => Ok(Name { id, files }),
                None => Err(StatusCode::Eof),
            },
            _ => Err(StatusCode::Failure),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.writable(&filename)?).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        std::fs::create_dir(self.writable(&path)?).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.writable(&path)?).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, Self::Error> {
        let (from, to) = (self.writable(&oldpath)?, self.writable(&newpath)?);
        if let Some(settings) = self.settings.as_ref().filter(|_| self.is_settings(&to)) {
            let written = std::fs::read(&from).map_err(|e| io_status(&e))?;
            std::fs::write(&to, settings.restore_file(&written, &to)).map_err(|e| io_status(&e))?;
            std::fs::remove_file(&from).map_err(|e| io_status(&e))?;
            return Ok(ok(id));
        }
        std::fs::rename(from, to).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn symlink(&mut self, _id: u32, _linkpath: String, _targetpath: String) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied)
    }
}

pub const USER_CONTENT: [&str; 4] = ["skills", "agents", "commands", "output-styles"];

pub struct UserContent {
    dir: PathBuf,
    handles: HashMap<String, Open>,
    next: u64,
}

enum Entry {
    Virtual(Vec<String>),
    Real(PathBuf),
}

impl UserContent {
    pub fn new(dir: &Path) -> Option<UserContent> {
        let content = UserContent { dir: dir.to_path_buf(), handles: HashMap::new(), next: 0 };
        (!content.present().is_empty()).then_some(content)
    }

    fn present(&self) -> Vec<String> {
        USER_CONTENT.iter().filter(|n| self.dir.join(n).is_dir()).map(|n| (*n).to_owned()).collect()
    }

    fn entry(&self, path: &str, follow: bool) -> Result<Entry, StatusCode> {
        let mut parts: Vec<String> = vec![];
        for component in Path::new(path).components() {
            match component {
                Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
                Component::ParentDir => {
                    parts.pop();
                }
                _ => {}
            }
        }
        match parts.as_slice() {
            [] => Ok(Entry::Virtual(vec![".claude".into()])),
            [claude] if claude == ".claude" => Ok(Entry::Virtual(self.present())),
            [claude, folder, rest @ ..] if claude == ".claude" && USER_CONTENT.contains(&folder.as_str()) => {
                let top = self.dir.join(folder).canonicalize().map_err(|e| io_status(&e))?;
                if rest.is_empty() {
                    return Ok(Entry::Real(top));
                }
                let local = rest.iter().fold(top.clone(), |p, part| p.join(part));
                let check = if follow { local.canonicalize() } else { local.parent().map_or(Ok(top.clone()), Path::canonicalize) };
                let check = check.map_err(|e| io_status(&e))?;
                if !check.starts_with(&top) {
                    return Err(StatusCode::NoSuchFile);
                }
                Ok(Entry::Real(local))
            }
            _ => Err(StatusCode::NoSuchFile),
        }
    }

    fn attrs(&self, meta: &Metadata) -> FileAttributes {
        read_only(meta)
    }

    fn stat_entry(&self, path: &str, follow: bool) -> Result<FileAttributes, StatusCode> {
        match self.entry(path, follow)? {
            Entry::Virtual(_) => std::fs::metadata(&self.dir).map(|m| self.attrs(&m)).map_err(|e| io_status(&e)),
            Entry::Real(local) => {
                let meta = if follow { std::fs::metadata(&local) } else { std::fs::symlink_metadata(&local) };
                meta.map(|m| self.attrs(&m)).map_err(|e| io_status(&e))
            }
        }
    }

    fn handle(&mut self, open: Open) -> String {
        self.next += 1;
        let handle = self.next.to_string();
        self.handles.insert(handle.clone(), open);
        handle
    }
}

impl russh_sftp::server::Handler for UserContent {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::PermissionDenied
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        note("user realpath", &path);
        let normalized = Path::new(&path).components().fold(PathBuf::from("/"), |mut p, c| {
            match c {
                Component::Normal(n) => p.push(n),
                Component::ParentDir => {
                    p.pop();
                }
                _ => {}
            }
            p
        });
        Ok(Name { id, files: vec![File::dummy(normalized.to_string_lossy())] })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("user stat", &path);
        Ok(Attrs { id, attrs: self.stat_entry(&path, true)? })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("user lstat", &path);
        Ok(Attrs { id, attrs: self.stat_entry(&path, false)? })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.handles.get(&handle) {
            Some(Open::File(file)) => file.metadata().map(|m| Attrs { id, attrs: self.attrs(&m) }).map_err(|e| io_status(&e)),
            _ => Err(StatusCode::Failure),
        }
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        note("user readlink", &path);
        let Entry::Real(local) = self.entry(&path, false)? else { return Err(StatusCode::Failure) };
        let target = std::fs::read_link(&local).map_err(|e| io_status(&e))?;
        Ok(Name { id, files: vec![File::dummy(target.to_string_lossy())] })
    }

    async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, _attrs: FileAttributes) -> Result<Handle, Self::Error> {
        note("user open", &filename);
        if pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE) {
            return Err(StatusCode::PermissionDenied);
        }
        let Entry::Real(local) = self.entry(&filename, true)? else { return Err(StatusCode::Failure) };
        let file = FsFile::open(&local).map_err(|e| io_status(&e))?;
        Ok(Handle { id, handle: self.handle(Open::File(file)) })
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        note("user read", &handle);
        let Some(Open::File(file)) = self.handles.get_mut(&handle) else { return Err(StatusCode::Failure) };
        file.seek(SeekFrom::Start(offset)).map_err(|e| io_status(&e))?;
        let mut data = Vec::with_capacity(len as usize);
        Read::by_ref(file).take(u64::from(len)).read_to_end(&mut data).map_err(|e| io_status(&e))?;
        if data.is_empty() && len > 0 {
            return Err(StatusCode::Eof);
        }
        Ok(Data { id, data })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        note("user opendir", &path);
        let files = match self.entry(&path, true)? {
            Entry::Virtual(names) => {
                let attrs = self.stat_entry("/", true)?;
                names.into_iter().map(|n| File::new(n, attrs.clone())).collect()
            }
            Entry::Real(local) => std::fs::read_dir(&local)
                .map_err(|e| io_status(&e))?
                .flatten()
                .filter_map(|e| std::fs::symlink_metadata(e.path()).ok().map(|m| File::new(e.file_name().to_string_lossy(), self.attrs(&m))))
                .collect(),
        };
        Ok(Handle { id, handle: self.handle(Open::Dir(files)) })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        note("user readdir", &handle);
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(listing)) => match next_batch(listing) {
                Some(files) => Ok(Name { id, files }),
                None => Err(StatusCode::Eof),
            },
            _ => Err(StatusCode::Failure),
        }
    }
}

fn read_only(meta: &Metadata) -> FileAttributes {
    let mut attrs = FileAttributes::from(meta);
    attrs.uid = None;
    attrs.gid = None;
    attrs.user = None;
    attrs.group = None;
    attrs.permissions = attrs.permissions.map(|m| m & !0o222);
    attrs
}

pub struct Plugins {
    plugins: Arc<Vec<Plugin>>,
    callbacks: Arc<Callbacks>,
    handles: HashMap<String, Open>,
    next: u64,
}

enum PluginEntry {
    Root,
    Real { plugin: usize, relative: PathBuf, local: PathBuf },
}

impl Plugins {
    pub fn new(plugins: Arc<Vec<Plugin>>, callbacks: Arc<Callbacks>) -> Plugins {
        Plugins { plugins, callbacks, handles: HashMap::new(), next: 0 }
    }

    fn viewed(&self, plugin: &Plugin, relative: &Path) -> bool {
        relative == Path::new(plugins::MANIFEST) || plugin.hook_files().iter().any(|f| f == relative)
    }

    fn entry(&self, path: &str, follow: bool) -> Result<PluginEntry, StatusCode> {
        let mut parts: Vec<String> = vec![];
        for component in Path::new(path).components() {
            match component {
                Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
                Component::ParentDir => {
                    parts.pop();
                }
                _ => {}
            }
        }
        let Some((dir, rest)) = parts.split_first() else { return Ok(PluginEntry::Root) };
        let index = self.plugins.iter().position(|p| p.dir_name() == *dir).ok_or(StatusCode::NoSuchFile)?;
        let plugin = &self.plugins[index];
        let relative: PathBuf = rest.iter().collect();
        if relative == Path::new(plugins::MCP_FILE) {
            return Err(StatusCode::NoSuchFile);
        }
        let local = plugin.root.join(&relative);
        if !rest.is_empty() {
            let check = if follow { local.canonicalize() } else { local.parent().map_or(Ok(plugin.root.clone()), Path::canonicalize) };
            if !check.map_err(|e| io_status(&e))?.starts_with(&plugin.root) {
                return Err(StatusCode::NoSuchFile);
            }
        }
        Ok(PluginEntry::Real { plugin: index, relative, local })
    }

    fn view(&self, plugin: &Plugin, relative: &Path, local: &Path) -> Result<Vec<u8>, StatusCode> {
        if relative == Path::new(plugins::MANIFEST) {
            return Ok(plugin.manifest_view(&self.callbacks));
        }
        let bytes = std::fs::read(local).map_err(|e| io_status(&e))?;
        Ok(self.callbacks.plugin_view(&bytes, &plugin.env()))
    }

    fn stat_entry(&self, path: &str, follow: bool) -> Result<FileAttributes, StatusCode> {
        match self.entry(path, follow)? {
            PluginEntry::Root => self.plugins.first().ok_or(StatusCode::NoSuchFile).and_then(|p| std::fs::metadata(&p.root).map(|m| read_only(&m)).map_err(|e| io_status(&e))),
            PluginEntry::Real { plugin, relative, local } => {
                let meta = if follow { std::fs::metadata(&local) } else { std::fs::symlink_metadata(&local) };
                let mut attrs = meta.map(|m| read_only(&m)).map_err(|e| io_status(&e))?;
                let plugin = &self.plugins[plugin];
                if attrs.is_regular() && self.viewed(plugin, &relative) {
                    attrs.size = Some(self.view(plugin, &relative, &local)?.len() as u64);
                }
                Ok(attrs)
            }
        }
    }

    fn handle(&mut self, open: Open) -> String {
        self.next += 1;
        let handle = self.next.to_string();
        self.handles.insert(handle.clone(), open);
        handle
    }
}

impl russh_sftp::server::Handler for Plugins {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::PermissionDenied
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        note("plugins realpath", &path);
        let normalized = Path::new(&path).components().fold(PathBuf::from("/"), |mut p, c| {
            match c {
                Component::Normal(n) => p.push(n),
                Component::ParentDir => {
                    p.pop();
                }
                _ => {}
            }
            p
        });
        Ok(Name { id, files: vec![File::dummy(normalized.to_string_lossy())] })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("plugins stat", &path);
        Ok(Attrs { id, attrs: self.stat_entry(&path, true)? })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("plugins lstat", &path);
        Ok(Attrs { id, attrs: self.stat_entry(&path, false)? })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.handles.get(&handle) {
            Some(Open::File(file)) => file.metadata().map(|m| Attrs { id, attrs: read_only(&m) }).map_err(|e| io_status(&e)),
            Some(Open::Settings { local, data, .. }) => {
                let mut attrs = std::fs::metadata(local).map(|m| read_only(&m)).map_err(|e| io_status(&e))?;
                attrs.size = Some(data.len() as u64);
                Ok(Attrs { id, attrs })
            }
            _ => Err(StatusCode::Failure),
        }
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        note("plugins readlink", &path);
        let PluginEntry::Real { local, .. } = self.entry(&path, false)? else { return Err(StatusCode::Failure) };
        let target = std::fs::read_link(&local).map_err(|e| io_status(&e))?;
        Ok(Name { id, files: vec![File::dummy(target.to_string_lossy())] })
    }

    async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, _attrs: FileAttributes) -> Result<Handle, Self::Error> {
        note("plugins open", &filename);
        if pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE) {
            return Err(StatusCode::PermissionDenied);
        }
        let PluginEntry::Real { plugin, relative, local } = self.entry(&filename, true)? else { return Err(StatusCode::Failure) };
        let plugins = self.plugins.clone();
        let plugin = &plugins[plugin];
        let open = if self.viewed(plugin, &relative) {
            let data = self.view(plugin, &relative, &local)?;
            Open::Settings { local, data, written: false }
        } else {
            Open::File(FsFile::open(&local).map_err(|e| io_status(&e))?)
        };
        Ok(Handle { id, handle: self.handle(open) })
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        note("plugins read", &handle);
        let data = match self.handles.get_mut(&handle) {
            Some(Open::Settings { data, .. }) => {
                let start = (offset as usize).min(data.len());
                data[start..(start + len as usize).min(data.len())].to_vec()
            }
            Some(Open::File(file)) => {
                file.seek(SeekFrom::Start(offset)).map_err(|e| io_status(&e))?;
                let mut data = Vec::with_capacity(len as usize);
                Read::by_ref(file).take(u64::from(len)).read_to_end(&mut data).map_err(|e| io_status(&e))?;
                data
            }
            _ => return Err(StatusCode::Failure),
        };
        if data.is_empty() && len > 0 {
            return Err(StatusCode::Eof);
        }
        Ok(Data { id, data })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        note("plugins opendir", &path);
        let files = match self.entry(&path, true)? {
            PluginEntry::Root => self
                .plugins
                .iter()
                .filter_map(|p| std::fs::metadata(&p.root).ok().map(|m| File::new(p.dir_name(), read_only(&m))))
                .collect(),
            PluginEntry::Real { plugin, relative, local } => {
                let hides_mcp = relative.as_os_str().is_empty();
                let all = self.plugins.clone();
                let plugin = &all[plugin];
                std::fs::read_dir(&local)
                    .map_err(|e| io_status(&e))?
                    .flatten()
                    .filter(|e| !(hides_mcp && e.file_name() == plugins::MCP_FILE))
                    .filter_map(|e| {
                        let mut attrs = read_only(&std::fs::symlink_metadata(e.path()).ok()?);
                        let inside = relative.join(e.file_name());
                        if attrs.is_regular() && self.viewed(plugin, &inside) {
                            attrs.size = Some(self.view(plugin, &inside, &e.path()).ok()?.len() as u64);
                        }
                        Some(File::new(e.file_name().to_string_lossy(), attrs))
                    })
                    .collect()
            }
        };
        Ok(Handle { id, handle: self.handle(Open::Dir(files)) })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        note("plugins readdir", &handle);
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(listing)) => match next_batch(listing) {
                Some(files) => Ok(Name { id, files }),
                None => Err(StatusCode::Eof),
            },
            _ => Err(StatusCode::Failure),
        }
    }
}

pub struct Sessions {
    root: PathBuf,
    paths: Arc<crate::mount::workdir::PathMap>,
    handles: HashMap<String, SessionOpen>,
    next: u64,
}

enum SessionOpen {
    Dir(Vec<File>),
    File { file: FsFile, lines: LineWriter },
}

pub const OPEN_AS: &str = "open-as@claudette";

#[derive(Serialize, Deserialize)]
pub struct OpenAs {
    pub handle: String,
    pub filename: String,
    pub pflags: OpenFlags,
    pub attrs: FileAttributes,
}

pub struct LineWriter {
    append: bool,
    next: u64,
    pending: Vec<u8>,
}

impl LineWriter {
    pub fn new(pflags: OpenFlags, size: u64) -> LineWriter {
        LineWriter { append: pflags.contains(OpenFlags::APPEND), next: size, pending: vec![] }
    }

    pub fn write<F: Write + Seek>(&mut self, paths: &crate::mount::workdir::PathMap, file: &mut F, offset: u64, data: &[u8]) -> std::io::Result<()> {
        if self.append || offset == self.next {
            self.pending.extend_from_slice(data);
            self.next = offset.max(self.next) + data.len() as u64;
            self.flush(paths, file, false)
        } else {
            self.flush(paths, file, true)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(data)?;
            self.next = offset + data.len() as u64;
            Ok(())
        }
    }

    pub fn truncate(&mut self, size: u64) {
        self.pending.clear();
        self.next = size;
    }

    pub fn flush<F: Write + Seek>(&mut self, paths: &crate::mount::workdir::PathMap, file: &mut F, all: bool) -> std::io::Result<()> {
        let end = if all { self.pending.len() } else { self.pending.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1) };
        if end == 0 {
            return Ok(());
        }
        let chunk: Vec<u8> = self.pending.drain(..end).collect();
        let mapped = match std::str::from_utf8(&chunk) {
            Ok(text) => paths.map(text).into_owned().into_bytes(),
            Err(_) => chunk,
        };
        file.seek(SeekFrom::End(0))?;
        file.write_all(&mapped)
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        for open in self.handles.values_mut() {
            if let SessionOpen::File { file, lines } = open {
                let _ = lines.flush(&self.paths, file, true);
            }
        }
    }
}

impl Sessions {
    pub fn new(root: &Path, paths: Arc<crate::mount::workdir::PathMap>) -> std::io::Result<Sessions> {
        std::fs::create_dir_all(root)?;
        Ok(Sessions { root: root.canonicalize()?, paths, handles: HashMap::new(), next: 0 })
    }

    fn relative(path: &str) -> PathBuf {
        let mut out = PathBuf::new();
        for component in Path::new(path).components() {
            match component {
                Component::Normal(name) => out.push(name),
                Component::ParentDir => {
                    out.pop();
                }
                _ => {}
            }
        }
        out
    }

    fn entry(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let relative = Self::relative(path);
        let Some(name) = relative.file_name() else { return Ok(self.root.clone()) };
        let parent = self.root.join(relative.parent().unwrap_or(Path::new(""))).canonicalize().map_err(|e| io_status(&e))?;
        if !parent.starts_with(&self.root) {
            return Err(StatusCode::NoSuchFile);
        }
        Ok(parent.join(name))
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let local = self.entry(path)?;
        match local.canonicalize() {
            Ok(real) if real.starts_with(&self.root) => Ok(real),
            Ok(_) => Err(StatusCode::NoSuchFile),
            Err(e) => Err(io_status(&e)),
        }
    }

    fn attrs(meta: &Metadata) -> FileAttributes {
        let mut attrs = FileAttributes::from(meta);
        attrs.uid = None;
        attrs.gid = None;
        attrs.user = None;
        attrs.group = None;
        attrs
    }

    fn handle(&mut self, open: SessionOpen) -> String {
        self.next += 1;
        let handle = self.next.to_string();
        self.handles.insert(handle.clone(), open);
        handle
    }

    fn open_file(&self, filename: &str, pflags: OpenFlags, attrs: &FileAttributes) -> Result<SessionOpen, StatusCode> {
        let local = if pflags.contains(OpenFlags::CREATE) { self.entry(filename)? } else { self.resolve(filename)? };
        if local.is_symlink() {
            return Err(StatusCode::PermissionDenied);
        }
        let file = OpenOptions::from(pflags).open(&local).map_err(|e| io_status(&e))?;
        if let Some(mode) = attrs.permissions.filter(|_| pflags.contains(OpenFlags::CREATE)) {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777));
        }
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(SessionOpen::File { file, lines: LineWriter::new(pflags, size) })
    }
}

impl russh_sftp::server::Handler for Sessions {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        note("sessions realpath", &path);
        Ok(Name { id, files: vec![File::dummy(format!("/{}", Self::relative(&path).to_string_lossy()))] })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("sessions stat", &path);
        let meta = std::fs::metadata(self.resolve(&path)?).map_err(|e| io_status(&e))?;
        Ok(Attrs { id, attrs: Self::attrs(&meta) })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("sessions lstat", &path);
        let meta = std::fs::symlink_metadata(self.entry(&path)?).map_err(|e| io_status(&e))?;
        Ok(Attrs { id, attrs: Self::attrs(&meta) })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.handles.get(&handle) {
            Some(SessionOpen::File { file, .. }) => file.metadata().map(|m| Attrs { id, attrs: Self::attrs(&m) }).map_err(|e| io_status(&e)),
            _ => Err(StatusCode::Failure),
        }
    }

    async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, attrs: FileAttributes) -> Result<Handle, Self::Error> {
        note("sessions open", &filename);
        let open = self.open_file(&filename, pflags, &attrs)?;
        Ok(Handle { id, handle: self.handle(open) })
    }

    async fn extended(&mut self, id: u32, request: String, data: Vec<u8>) -> Result<Packet, Self::Error> {
        if request != OPEN_AS {
            return Err(StatusCode::OpUnsupported);
        }
        let request: OpenAs = russh_sftp::de::from_bytes(&mut bytes::Bytes::from(data)).map_err(|_| StatusCode::BadMessage)?;
        note("sessions open", &request.filename);
        if request.handle.is_empty() || request.handle.starts_with(|c: char| c.is_ascii_digit()) || self.handles.contains_key(&request.handle) {
            return Err(StatusCode::Failure);
        }
        let open = self.open_file(&request.filename, request.pflags, &request.attrs)?;
        self.handles.insert(request.handle, open);
        Ok(Packet::Status(ok(id)))
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        note("sessions read", &handle);
        let Some(SessionOpen::File { file, .. }) = self.handles.get_mut(&handle) else { return Err(StatusCode::Failure) };
        file.seek(SeekFrom::Start(offset)).map_err(|e| io_status(&e))?;
        let mut data = Vec::with_capacity(len as usize);
        Read::by_ref(file).take(u64::from(len)).read_to_end(&mut data).map_err(|e| io_status(&e))?;
        if data.is_empty() && len > 0 {
            return Err(StatusCode::Eof);
        }
        Ok(Data { id, data })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        note("sessions write", &handle);
        let Some(SessionOpen::File { file, lines }) = self.handles.get_mut(&handle) else { return Err(StatusCode::Failure) };
        lines.write(&self.paths, file, offset, &data).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if let Some(SessionOpen::File { mut file, mut lines }) = self.handles.remove(&handle) {
            lines.flush(&self.paths, &mut file, true).map_err(|e| io_status(&e))?;
        }
        Ok(ok(id))
    }

    async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        note("sessions setstat", &path);
        Project::set_attrs(&self.resolve(&path)?, None, &attrs)?;
        Ok(ok(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        note("sessions fsetstat", &handle);
        let Some(SessionOpen::File { file, lines }) = self.handles.get_mut(&handle) else { return Err(StatusCode::Failure) };
        let file = file.try_clone().map_err(|e| io_status(&e))?;
        if let Some(size) = attrs.size {
            lines.truncate(size);
        }
        Project::set_attrs(Path::new(""), Some(&file), &attrs)?;
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        note("sessions opendir", &path);
        let files = std::fs::read_dir(self.resolve(&path)?)
            .map_err(|e| io_status(&e))?
            .flatten()
            .filter_map(|e| std::fs::symlink_metadata(e.path()).ok().map(|m| File::new(e.file_name().to_string_lossy(), Self::attrs(&m))))
            .collect();
        Ok(Handle { id, handle: self.handle(SessionOpen::Dir(files)) })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(SessionOpen::Dir(listing)) => match next_batch(listing) {
                Some(files) => Ok(Name { id, files }),
                None => Err(StatusCode::Eof),
            },
            _ => Err(StatusCode::Failure),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        note("sessions remove", &filename);
        std::fs::remove_file(self.entry(&filename)?).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        note("sessions mkdir", &path);
        std::fs::create_dir(self.entry(&path)?).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let local = self.entry(&path)?;
        if local == self.root {
            return Err(StatusCode::PermissionDenied);
        }
        std::fs::remove_dir(local).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, Self::Error> {
        note("sessions rename", &format!("{oldpath} {newpath}"));
        std::fs::rename(self.entry(&oldpath)?, self.entry(&newpath)?).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn symlink(&mut self, _id: u32, _linkpath: String, _targetpath: String) -> Result<Status, Self::Error> {
        Err(StatusCode::PermissionDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::server::Handler;

    fn project() -> (PathBuf, Project) {
        let dir = std::env::temp_dir().join(format!("ct-sftp-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        for d in ["src", ".git", "node_modules/x", ".claude"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("src/a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref").unwrap();
        let outside = dir.with_extension("secret");
        std::fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("escape")).unwrap();
        let p = Project::new(&dir, None, true).unwrap();
        (dir, p)
    }

    const READ: OpenFlags = OpenFlags::READ;
    fn create() -> OpenFlags {
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
    }

    #[tokio::test]
    async fn serves_the_project_and_nothing_outside_it() {
        let (_, mut p) = project();
        let h = p.open(1, "/src/a.rs".into(), READ, FileAttributes::empty()).await.unwrap().handle;
        assert_eq!(p.read(2, h, 0, 100).await.unwrap().data, b"fn a() {}");
        assert_eq!(p.realpath(3, "/../../src/..".into()).await.unwrap().files[0].filename, "/");
        assert_eq!(p.open(4, "/../../../etc/passwd".into(), READ, FileAttributes::empty()).await.err(), Some(StatusCode::NoSuchFile));
        assert_eq!(p.open(5, "/escape".into(), READ, FileAttributes::empty()).await.err(), Some(StatusCode::NoSuchFile));
        assert_eq!(p.stat(6, "/.git/HEAD".into()).await.err(), Some(StatusCode::NoSuchFile));
        let listing = p.opendir(7, "/".into()).await.unwrap().handle;
        let mut names: Vec<String> = p.readdir(8, listing.clone()).await.unwrap().files.into_iter().map(|f| f.filename).collect();
        names.sort();
        assert_eq!(names, [".claude", "escape", "src"]);
        assert_eq!(p.readdir(9, listing).await.err(), Some(StatusCode::Eof));
    }

    #[tokio::test]
    async fn a_big_folder_is_listed_over_several_replies() {
        let (dir, mut p) = project();
        std::fs::create_dir(dir.join("big")).unwrap();
        for i in 0..250 {
            std::fs::write(dir.join(format!("big/f{i:03}")), "").unwrap();
        }
        let listing = p.opendir(1, "/big".into()).await.unwrap().handle;
        let mut sizes = vec![];
        let mut names = std::collections::BTreeSet::new();
        while let Ok(reply) = p.readdir(2, listing.clone()).await {
            sizes.push(reply.files.len());
            names.extend(reply.files.into_iter().map(|f| f.filename));
        }
        assert_eq!(sizes, [100, 100, 50]);
        assert_eq!(names.len(), 250);
        assert_eq!(p.readdir(3, listing).await.err(), Some(StatusCode::Eof));
    }

    #[test]
    fn a_reply_of_long_names_stays_under_what_sshfs_accepts() {
        let name = "é".repeat(380);
        let mut listing: Vec<File> = (0..200).map(|i| File::new(format!("{name}{i}"), FileAttributes::default())).collect();
        let mut total = 0;
        while let Some(batch) = next_batch(&mut listing) {
            let bytes: usize = batch.iter().map(|f| 8 + f.filename.len() + f.longname.len() + 64).sum();
            assert!(bytes <= READDIR_BYTES, "{bytes}");
            total += batch.len();
        }
        assert_eq!(total, 200);
    }

    #[tokio::test]
    async fn what_git_ignores_is_not_served_but_claude_codes_own_files_are() {
        let (dir, mut p) = project();
        std::fs::write(dir.join(".gitignore"), "target/\n.claude/settings.local.json\nCLAUDE.local.md\n").unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("target/debug/app"), "").unwrap();
        std::fs::write(dir.join(".claude/settings.local.json"), "{}").unwrap();
        std::fs::write(dir.join("CLAUDE.local.md"), "notes").unwrap();
        std::os::unix::fs::symlink(dir.join("target"), dir.join("out")).unwrap();

        let listing = p.opendir(1, "/".into()).await.unwrap().handle;
        let mut names: Vec<String> = p.readdir(2, listing).await.unwrap().files.into_iter().map(|f| f.filename).collect();
        names.sort();
        assert_eq!(names, [".claude", ".gitignore", "CLAUDE.local.md", "escape", "out", "src"]);
        assert_eq!(p.stat(3, "/target".into()).await.err(), Some(StatusCode::NoSuchFile));
        assert_eq!(p.open(4, "/target/debug/app".into(), READ, FileAttributes::empty()).await.err(), Some(StatusCode::NoSuchFile));
        assert_eq!(p.stat(5, "/out".into()).await.err(), Some(StatusCode::NoSuchFile));
        let h = p.open(6, "/CLAUDE.local.md".into(), READ, FileAttributes::empty()).await.unwrap().handle;
        assert_eq!(p.read(7, h, 0, 100).await.unwrap().data, b"notes");
        assert!(p.stat(8, "/.claude/settings.local.json".into()).await.is_ok());
    }

    #[tokio::test]
    async fn only_claude_codes_own_files_can_change() {
        let (dir, mut p) = project();
        assert_eq!(p.open(1, "/src/b.rs".into(), create(), FileAttributes::empty()).await.err(), Some(StatusCode::PermissionDenied));
        assert_eq!(p.remove(2, "/src/a.rs".into()).await.err(), Some(StatusCode::PermissionDenied));
        assert_eq!(p.rename(3, "/.claude/x".into(), "/src/x".into()).await.err(), Some(StatusCode::PermissionDenied));
        let h = p.open(4, "/.claude/settings.local.json".into(), create(), FileAttributes::empty()).await.unwrap().handle;
        p.write(5, h, 0, b"{}".to_vec()).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(".claude/settings.local.json")).unwrap(), "{}");
        p.mkdir(6, "/.claude/skills".into(), FileAttributes::empty()).await.unwrap();
        for writable in ["/CLAUDE.md", "/src/CLAUDE.local.md", "/.mcp.json"] {
            assert!(p.open(7, writable.into(), create(), FileAttributes::empty()).await.is_ok(), "{writable}");
        }
        assert_eq!(p.open(8, "/src/.mcp.json".into(), create(), FileAttributes::empty()).await.err(), Some(StatusCode::PermissionDenied));
        let ro = p.stat(9, "/src/a.rs".into()).await.unwrap().attrs.permissions.unwrap();
        assert_eq!(ro & 0o222, 0);
    }

    #[tokio::test]
    async fn only_the_users_own_skills_agents_commands_and_styles_are_served() {
        let dir = std::env::temp_dir().join(format!("ct-user-{}", std::process::id()));
        for d in ["skills/mine", "agents", "projects/-x", "plugins/cache"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("skills/mine/SKILL.md"), "skill").unwrap();
        std::fs::write(dir.join("settings.json"), "{}").unwrap();
        std::fs::write(dir.join(".credentials.json"), "secret").unwrap();
        std::fs::write(dir.join("history.jsonl"), "typed").unwrap();
        let outside = dir.with_extension("outside");
        std::fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("skills/mine/leak")).unwrap();
        std::os::unix::fs::symlink(dir.join(".credentials.json"), dir.join("agents/creds.md")).unwrap();

        let mut u = UserContent::new(&dir).unwrap();
        let names = |files: Vec<File>| files.into_iter().map(|f| f.filename).collect::<Vec<_>>();
        let root = u.opendir(1, "/".into()).await.unwrap().handle;
        assert_eq!(names(u.readdir(2, root).await.unwrap().files), [".claude"]);
        let claude = u.opendir(3, "/.claude".into()).await.unwrap().handle;
        assert_eq!(names(u.readdir(4, claude).await.unwrap().files), ["skills", "agents"]);
        for path in ["/.claude", "/.claude/skills", "/.claude/skills/mine", "/.claude/skills/mine/SKILL.md"] {
            assert!(u.lstat(9, path.into()).await.is_ok(), "{path}");
        }
        let h = u.open(5, "/.claude/skills/mine/SKILL.md".into(), READ, FileAttributes::empty()).await.unwrap().handle;
        assert_eq!(u.read(6, h, 0, 100).await.unwrap().data, b"skill");
        for hidden in ["/.claude/settings.json", "/.claude/.credentials.json", "/.claude/projects/-x", "/.claude/skills/mine/leak", "/.claude/agents/creds.md", "/.claude/skills/../history.jsonl"] {
            assert_eq!(u.open(7, hidden.into(), READ, FileAttributes::empty()).await.err(), Some(StatusCode::NoSuchFile), "{hidden}");
        }
        assert_eq!(u.open(8, "/.claude/skills/new.md".into(), create(), FileAttributes::empty()).await.err(), Some(StatusCode::PermissionDenied));
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[tokio::test]
    async fn the_sessions_folder_takes_transcripts_with_local_paths_and_nothing_leaves_it() {
        let base = std::env::temp_dir().join(format!("ct-sessions-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let cwd = Path::new("/Users/me/app");
        let remote = format!("/home/remote/{}", crate::mount::workdir::remote_workdir(cwd));
        let mut s = Sessions::new(&base.join("key"), Arc::new(crate::mount::workdir::PathMap::new(cwd))).unwrap();
        let append = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::APPEND;
        let h = s.open(1, "/sess.jsonl".into(), append, FileAttributes::empty()).await.unwrap().handle;
        let line = format!("{{\"cwd\":\"{remote}\",\"file\":\"{remote}/src/a.rs\"}}\n");
        let (first, second) = line.split_at(20);
        s.write(2, h.clone(), 0, first.as_bytes().to_vec()).await.unwrap();
        assert_eq!(std::fs::read_to_string(base.join("key/sess.jsonl")).unwrap(), "");
        s.write(3, h.clone(), 0, second.as_bytes().to_vec()).await.unwrap();
        s.close(4, h).await.unwrap();
        assert_eq!(std::fs::read_to_string(base.join("key/sess.jsonl")).unwrap(), "{\"cwd\":\"/Users/me/app\",\"file\":\"/Users/me/app/src/a.rs\"}\n");

        s.mkdir(5, "/sess".into(), FileAttributes::empty()).await.unwrap();
        std::fs::write(base.join("outside"), "secret").unwrap();
        std::os::unix::fs::symlink(base.join("outside"), base.join("key/leak")).unwrap();
        assert_eq!(s.open(6, "/leak".into(), READ, FileAttributes::empty()).await.err(), Some(StatusCode::NoSuchFile));
        assert_eq!(s.open(7, "/../outside".into(), READ, FileAttributes::empty()).await.err(), Some(StatusCode::NoSuchFile));
        assert!(s.open(8, "/../escaped".into(), create(), FileAttributes::empty()).await.is_ok());
        assert!(!base.join("escaped").exists(), "`..` stops at the folder");
        assert_eq!(s.symlink(9, "/l".into(), "/etc".into()).await.err(), Some(StatusCode::PermissionDenied));
        std::fs::remove_dir_all(base).unwrap();
    }
}
