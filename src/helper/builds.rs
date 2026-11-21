use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nix::fcntl::{Flock, FlockArg};

const KEEP: usize = 2;
const LOCK: &str = ".lock";

fn lock(dir: &Path, how: FlockArg) -> Option<Flock<File>> {
    let file = File::options().create(true).truncate(false).write(true).open(dir.join(LOCK)).ok()?;
    Flock::lock(file, how).ok()
}

pub fn hold() -> Option<Flock<File>> {
    lock(std::env::current_exe().ok()?.parent()?, FlockArg::LockShared)
}

fn bin() -> PathBuf {
    crate::config::home().join(".claudette").join("bin")
}

pub fn prune() -> i32 {
    let Ok(entries) = std::fs::read_dir(bin()) else { return 0 };
    let mut builds: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| (std::fs::metadata(p.join("claudette")).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), p))
        .collect();
    builds.sort_by_key(|b| std::cmp::Reverse(b.0));
    for (_, dir) in builds.into_iter().skip(KEEP) {
        if let Some(_held) = lock(&dir, FlockArg::LockExclusiveNonblock) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    0
}
