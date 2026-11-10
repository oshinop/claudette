//! This machine's Claude Code plugin store, served for the remote machine's Claude Code to manage:
//! `claude plugin …` runs there, installing into this machine's `~/.claude/plugins` and recording
//! its choices in this machine's settings (see [`crate::run::plugin_command`]). Nothing of Claude
//! Code's installer is copied here; it is the remote machine's own, working on this machine's files.
//!
//! The root is a private stage folder made for the command:
//!
//! - `config/` is the remote claude's Claude Code folder: `plugins` and `settings.json` lead to this
//!   machine's own, and anything else it writes there (`.claude.json`, backups) stays in the stage,
//!   which goes at the end.
//! - `project/` is where it runs: `.claude/settings.json` and `.claude/settings.local.json` lead to
//!   this project's, for `--scope project` and `--scope local`.
//! - `paths/<n>`, read-only, are folders of this machine a command names or the store records (a
//!   marketplace in a local folder).
//!
//! The files that record paths (`installed_plugins.json`, `known_marketplaces.json` and the settings
//! files) read with the mount's paths and are written back with this machine's, so the store stays
//! as a local claude would have left it. Nothing outside those is reachable.

use std::collections::HashMap;
use std::fs::{File as FsFile, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode};
use serde_json::Value;

use super::sftp::{io_status, next_batch, note, ok};
use crate::settings::{SETTINGS_FILES, user_dir};

/// The plugin store's files that record paths.
const RECORDS: [&str; 2] = ["installed_plugins.json", "known_marketplaces.json"];

#[derive(Clone, Copy, PartialEq)]
enum Access {
    ReadWrite,
    ReadOnly,
}

/// The stage and what it leads to; the stage folder goes when this does.
pub struct Stage {
    /// The stage folder, canonical.
    dir: PathBuf,
    /// Real paths reachable through it: (path, access, whether only that path and not below it).
    roots: Vec<(PathBuf, Access, bool)>,
    /// Real paths of the files that record paths.
    records: Vec<PathBuf>,
    /// (path on the remote machine, path here), for the files that record paths.
    pairs: Vec<(String, String)>,
}

impl Drop for Stage {
    fn drop(&mut self) {
        // The links go, not what they lead to.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A path whose last part may not exist yet: its folder's real path, then its name.
fn real_entry(path: &Path) -> std::io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(real) => Ok(real),
        Err(_) => {
            let name = path.file_name().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
            Ok(path.parent().unwrap_or(Path::new("/")).canonicalize()?.join(name))
        }
    }
}

impl Stage {
    /// Makes the stage for a command run in `cwd`, with `paths` as `paths/<n>`, mounted at `remote`
    /// on the remote machine.
    pub fn new(cwd: &Path, paths: &[PathBuf], remote: &str) -> std::io::Result<Stage> {
        use std::os::unix::fs::{DirBuilderExt, symlink};
        let base = std::env::temp_dir().join(format!("claudette-store-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()));
        std::fs::DirBuilder::new().mode(0o700).create(&base)?;
        let dir = base.canonicalize()?;
        let user = user_dir();
        std::fs::create_dir_all(user.join("plugins"))?;
        for sub in ["config", "project/.claude", "paths"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        symlink(user.join("plugins"), dir.join("config/plugins"))?;
        symlink(user.join("settings.json"), dir.join("config/settings.json"))?;
        for file in SETTINGS_FILES {
            symlink(cwd.join(file), dir.join("project").join(file))?;
        }
        for (n, path) in paths.iter().enumerate() {
            symlink(path, dir.join("paths").join(n.to_string()))?;
        }

        let plugins = user.join("plugins").canonicalize()?;
        let mut roots = vec![(dir.clone(), Access::ReadWrite, false), (plugins.clone(), Access::ReadWrite, false)];
        let mut records: Vec<PathBuf> = RECORDS.iter().map(|r| plugins.join(r)).collect();
        // The settings files, which may not exist yet (nor their folder, for the project's).
        let settings = std::iter::once(user.join("settings.json")).chain(SETTINGS_FILES.iter().map(|f| cwd.join(f)));
        for file in settings {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let real = real_entry(&file)?;
            roots.push((real.clone(), Access::ReadWrite, true));
            records.push(real);
        }
        roots.extend(paths.iter().filter_map(|p| p.canonicalize().ok()).map(|p| (p, Access::ReadOnly, false)));

        let project = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        // Going out, the longest local path that holds a string wins, and a named folder before the
        // project (`claude plugin marketplace add .` names the project itself).
        let mut locals: Vec<(String, PathBuf)> = paths.iter().enumerate().map(|(n, p)| (format!("paths/{n}"), p.clone())).collect();
        locals.extend([("config".to_owned(), user.canonicalize().unwrap_or(user)), ("project".to_owned(), project)]);
        locals.sort_by_key(|(_, local)| std::cmp::Reverse(local.as_os_str().len()));
        // macOS reports /tmp by its real path, /private/tmp; the first spelling is the one read back.
        let mut pairs = vec![];
        for root in [remote.to_owned(), format!("/private{remote}")] {
            pairs.extend(locals.iter().map(|(sub, local)| (format!("{root}/{sub}"), local.to_string_lossy().into_owned())));
        }
        Ok(Stage { dir, roots, records, pairs })
    }

    fn access(&self, real: &Path) -> Option<Access> {
        self.roots.iter().filter(|(root, _, exact)| if *exact { real == root } else { real.starts_with(root) }).map(|(_, a, _)| *a).max_by_key(|a| *a == Access::ReadWrite)
    }

    /// A JSON file that records paths, with them turned from one side's into the other's.
    fn translate(&self, bytes: &[u8], to_local: bool) -> Vec<u8> {
        let Ok(mut value) = serde_json::from_slice::<Value>(bytes) else { return bytes.to_vec() };
        let pairs: Vec<(&str, &str)> = if to_local {
            self.pairs.iter().map(|(r, l)| (r.as_str(), l.as_str())).collect()
        } else {
            // One spelling of the mount's paths is enough going out.
            self.pairs.iter().take(self.pairs.len() / 2).map(|(r, l)| (l.as_str(), r.as_str())).collect()
        };
        rewrite(&mut value, &pairs);
        let mut out = serde_json::to_vec_pretty(&value).unwrap_or_else(|_| bytes.to_vec());
        if bytes.ends_with(b"\n") {
            out.push(b'\n');
        }
        out
    }
}

/// Every string that is one of the `from` paths, or below one, becomes the `to` path.
fn rewrite(value: &mut Value, pairs: &[(&str, &str)]) {
    match value {
        Value::String(s) => {
            for (from, to) in pairs {
                if let Some(rest) = s.strip_prefix(from)
                    && (rest.is_empty() || rest.starts_with('/'))
                {
                    *s = format!("{to}{rest}");
                    break;
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rewrite(v, pairs)),
        Value::Object(map) => map.values_mut().for_each(|v| rewrite(v, pairs)),
        _ => {}
    }
}

enum Open {
    File(FsFile),
    Dir(Vec<File>),
    /// A file that records paths, read and written whole: `real` gets it back on close.
    Record { real: PathBuf, data: Vec<u8>, written: bool },
}

/// The SFTP server over a [`Stage`].
pub struct Store {
    stage: Arc<Stage>,
    handles: HashMap<String, Open>,
    next: u64,
}

impl Store {
    pub fn new(stage: Arc<Stage>) -> Store {
        Store { stage, handles: HashMap::new(), next: 0 }
    }

    /// A client path in the stage; `..` stops at its root.
    fn local(&self, path: &str) -> PathBuf {
        let mut out = self.stage.dir.clone();
        for component in Path::new(path).components() {
            match component {
                Component::Normal(part) => out.push(part),
                Component::ParentDir if out != self.stage.dir => {
                    out.pop();
                }
                _ => {}
            }
        }
        out
    }

    /// The real path an operation touches, with what it may do there. `follow`: through a final
    /// symlink (a link the stage plants is always followed: it stands for what it leads to).
    fn resolve(&self, path: &str, follow: bool) -> Result<(PathBuf, Access), StatusCode> {
        let local = self.local(path);
        let planted = local.parent().is_some_and(|p| p.starts_with(&self.stage.dir) && p.canonicalize().is_ok_and(|p| p.starts_with(&self.stage.dir)));
        let real = if follow || (planted && local.is_symlink()) {
            match local.canonicalize() {
                Ok(real) => real,
                // Missing: what a link leads to, or the path itself, in its folder.
                Err(_) => real_entry(&std::fs::read_link(&local).unwrap_or(local)).map_err(|e| io_status(&e))?,
            }
        } else {
            real_entry(&local.parent().map(Path::to_path_buf).unwrap_or_default()).map_err(|e| io_status(&e))?.join(local.file_name().unwrap_or_default())
        };
        let access = self.stage.access(&real).ok_or(StatusCode::NoSuchFile)?;
        Ok((real, access))
    }

    fn writable(&self, path: &str, follow: bool) -> Result<PathBuf, StatusCode> {
        match self.resolve(path, follow)? {
            (real, Access::ReadWrite) => Ok(real),
            _ => Err(StatusCode::PermissionDenied),
        }
    }

    fn is_record(&self, real: &Path) -> bool {
        self.stage.records.iter().any(|r| r == real)
    }

    fn attrs(&self, real: &Path, meta: &Metadata, access: Access) -> FileAttributes {
        let mut attrs = FileAttributes::from(meta);
        attrs.uid = None;
        attrs.gid = None;
        attrs.user = None;
        attrs.group = None;
        if access == Access::ReadOnly {
            attrs.permissions = attrs.permissions.map(|m| m & !0o222);
        }
        if meta.is_file() && self.is_record(real) {
            attrs.size = std::fs::read(real).ok().map(|b| self.stage.translate(&b, false).len() as u64);
        }
        attrs
    }

    fn stat_path(&self, path: &str, follow: bool) -> Result<FileAttributes, StatusCode> {
        let (real, access) = self.resolve(path, follow)?;
        let meta = if follow { std::fs::metadata(&real) } else { std::fs::symlink_metadata(&real) };
        Ok(self.attrs(&real, &meta.map_err(|e| io_status(&e))?, access))
    }

    fn handle(&mut self, open: Open) -> String {
        self.next += 1;
        let handle = self.next.to_string();
        self.handles.insert(handle.clone(), open);
        handle
    }

    fn set_attrs(real: &Path, attrs: &FileAttributes) -> Result<(), StatusCode> {
        use std::os::unix::fs::PermissionsExt;
        if let Some(size) = attrs.size {
            OpenOptions::new().write(true).open(real).and_then(|f| f.set_len(size)).map_err(|e| io_status(&e))?;
        }
        if let Some(mode) = attrs.permissions {
            std::fs::set_permissions(real, std::fs::Permissions::from_mode(mode & 0o7777)).map_err(|e| io_status(&e))?;
        }
        if let Some(mtime) = attrs.mtime {
            let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime.into());
            let _ = FsFile::open(real).and_then(|f| f.set_modified(time));
        }
        Ok(())
    }
}

impl russh_sftp::server::Handler for Store {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        note("store realpath", &path);
        let relative = self.local(&path).strip_prefix(&self.stage.dir).map(Path::to_path_buf).unwrap_or_default();
        Ok(Name { id, files: vec![File::dummy(format!("/{}", relative.to_string_lossy()))] })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("store stat", &path);
        Ok(Attrs { id, attrs: self.stat_path(&path, true)? })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        note("store lstat", &path);
        Ok(Attrs { id, attrs: self.stat_path(&path, false)? })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.handles.get(&handle) {
            Some(Open::File(file)) => file.metadata().map(|m| Attrs { id, attrs: FileAttributes::from(&m) }).map_err(|e| io_status(&e)),
            Some(Open::Record { real, data, .. }) => {
                let mut attrs = std::fs::metadata(real).map(|m| FileAttributes::from(&m)).unwrap_or_default();
                attrs.size = Some(data.len() as u64);
                Ok(Attrs { id, attrs })
            }
            _ => Err(StatusCode::Failure),
        }
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        note("store readlink", &path);
        let (real, _) = self.resolve(&path, false)?;
        let target = std::fs::read_link(&real).map_err(|e| io_status(&e))?;
        Ok(Name { id, files: vec![File::dummy(target.to_string_lossy())] })
    }

    async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, attrs: FileAttributes) -> Result<Handle, Self::Error> {
        note("store open", &filename);
        let writes = pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE);
        let real = if writes { self.writable(&filename, true)? } else { self.resolve(&filename, true)?.0 };
        if self.is_record(&real) {
            let existing = std::fs::read(&real);
            if existing.is_err() && !pflags.contains(OpenFlags::CREATE) {
                return Err(StatusCode::NoSuchFile);
            }
            if existing.is_ok() && pflags.contains(OpenFlags::EXCLUDE) {
                return Err(StatusCode::Failure);
            }
            let data = match existing {
                Ok(bytes) if !pflags.contains(OpenFlags::TRUNCATE) => self.stage.translate(&bytes, false),
                _ => Vec::new(),
            };
            let written = pflags.contains(OpenFlags::TRUNCATE) || (pflags.contains(OpenFlags::CREATE) && !real.exists());
            return Ok(Handle { id, handle: self.handle(Open::Record { real, data, written }) });
        }
        let file = OpenOptions::from(pflags).open(&real).map_err(|e| io_status(&e))?;
        if let Some(mode) = attrs.permissions.filter(|_| pflags.contains(OpenFlags::CREATE)) {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777));
        }
        Ok(Handle { id, handle: self.handle(Open::File(file)) })
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        let data = match self.handles.get_mut(&handle) {
            Some(Open::Record { data, .. }) => {
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

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Record { data: buffer, written, .. }) => {
                let start = offset as usize;
                if buffer.len() < start + data.len() {
                    buffer.resize(start + data.len(), 0);
                }
                buffer[start..start + data.len()].copy_from_slice(&data);
                *written = true;
            }
            Some(Open::File(file)) => {
                file.seek(SeekFrom::Start(offset)).map_err(|e| io_status(&e))?;
                file.write_all(&data).map_err(|e| io_status(&e))?;
            }
            _ => return Err(StatusCode::Failure),
        }
        Ok(ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if let Some(Open::Record { real, data, written: true }) = self.handles.remove(&handle) {
            std::fs::write(&real, self.stage.translate(&data, true)).map_err(|e| io_status(&e))?;
        }
        Ok(ok(id))
    }

    async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        let real = self.writable(&path, true)?;
        // A record's size is the translated one; only emptying it means the same on the real file.
        if self.is_record(&real) && attrs.size.is_some_and(|s| s != 0) {
            return Ok(ok(id));
        }
        Store::set_attrs(&real, &attrs)?;
        Ok(ok(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Record { data, written, .. }) => {
                if let Some(size) = attrs.size {
                    data.resize(size as usize, 0);
                    *written = true;
                }
            }
            Some(Open::File(file)) => {
                use std::os::unix::fs::PermissionsExt;
                if let Some(size) = attrs.size {
                    file.set_len(size).map_err(|e| io_status(&e))?;
                }
                if let Some(mode) = attrs.permissions {
                    file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777)).map_err(|e| io_status(&e))?;
                }
                if let Some(mtime) = attrs.mtime {
                    let _ = file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime.into()));
                }
            }
            _ => return Err(StatusCode::Failure),
        }
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        note("store opendir", &path);
        let (real, access) = self.resolve(&path, true)?;
        let in_stage = real.starts_with(&self.stage.dir);
        let files = std::fs::read_dir(&real)
            .map_err(|e| io_status(&e))?
            .flatten()
            .filter_map(|e| {
                // The stage's links show as what they lead to.
                let meta = if in_stage { std::fs::metadata(e.path()).or_else(|_| std::fs::symlink_metadata(e.path())) } else { std::fs::symlink_metadata(e.path()) };
                meta.ok().map(|m| File::new(e.file_name().to_string_lossy(), self.attrs(&e.path(), &m, access)))
            })
            .collect();
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
        std::fs::remove_file(self.writable(&filename, false)?).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        use std::os::unix::fs::DirBuilderExt;
        let real = self.writable(&path, false)?;
        std::fs::DirBuilder::new().mode(attrs.permissions.map_or(0o755, |m| m & 0o7777)).create(&real).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.writable(&path, false)?).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, Self::Error> {
        let from = self.writable(&oldpath, false)?;
        // Onto a link of the stage means onto what it leads to.
        let to = self.writable(&newpath, false).and_then(|to| if to.is_symlink() && to.starts_with(&self.stage.dir) { self.writable(&newpath, true) } else { Ok(to) })?;
        // A record written by replacement (a temporary file, renamed over it) is turned back.
        if self.is_record(&to) {
            let written = std::fs::read(&from).map_err(|e| io_status(&e))?;
            std::fs::write(&to, self.stage.translate(&written, true)).map_err(|e| io_status(&e))?;
            std::fs::remove_file(&from).map_err(|e| io_status(&e))?;
            return Ok(ok(id));
        }
        std::fs::rename(from, to).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }

    async fn symlink(&mut self, id: u32, linkpath: String, targetpath: String) -> Result<Status, Self::Error> {
        // A repository's own links, inside the store; reading through one still stays inside.
        let link = self.writable(&linkpath, false)?;
        std::os::unix::fs::symlink(&targetpath, &link).map_err(|e| io_status(&e))?;
        Ok(ok(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_read_with_the_mounts_paths_and_are_written_back_with_this_machines() {
        let mut stage = Stage { dir: PathBuf::from("/s"), roots: vec![], records: vec![], pairs: vec![] };
        for root in ["/tmp/claudette-r.store", "/private/tmp/claudette-r.store"] {
            stage.pairs.push((format!("{root}/config"), "/Users/me/.claude".into()));
            stage.pairs.push((format!("{root}/paths/0"), "/Users/me/market".into()));
        }
        let local = br#"{"plugins":{"a@m":[{"installPath":"/Users/me/.claude/plugins/cache/m/a/1.0.0","note":"/Users/me/.claudette"}]},"m":{"source":{"path":"/Users/me/market"}}}"#;
        let remote: Value = serde_json::from_slice(&stage.translate(local, false)).unwrap();
        assert_eq!(remote["plugins"]["a@m"][0]["installPath"], "/tmp/claudette-r.store/config/plugins/cache/m/a/1.0.0");
        assert_eq!(remote["plugins"]["a@m"][0]["note"], "/Users/me/.claudette");
        assert_eq!(remote["m"]["source"]["path"], "/tmp/claudette-r.store/paths/0");
        // The remote side may write either spelling of the mount.
        let written = br#"{"installPath":"/private/tmp/claudette-r.store/config/plugins/x","path":"/tmp/claudette-r.store/paths/0"}"#;
        let back: Value = serde_json::from_slice(&stage.translate(written, true)).unwrap();
        assert_eq!(back, serde_json::json!({ "installPath": "/Users/me/.claude/plugins/x", "path": "/Users/me/market" }));
    }
}
