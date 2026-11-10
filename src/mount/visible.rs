//! Which project paths the workspace mount shows: what a local claude's `@` file picker offers.
//!
//! The remote Claude Code indexes the mounted project for `@` mentions and autocomplete. It sees no
//! `.git` there, so it cannot tell what git ignores, and without this it walks build output
//! (`target/`, `dist/`) entry by entry, each a round trip. So the server leaves out what git
//! ignores: `.gitignore` files in the project and in its repository above it, the repository's
//! `.git/info/exclude`, and the global excludes file, with git's precedence (a deeper `.gitignore`
//! over a shallower one, both over `info/exclude`, all over the global file). `!` patterns
//! re-include, except below an ignored folder, as in git. A `.gitignore` is read again when it
//! changes. Claude Code's `respectGitignore` setting turns this off.
//!
//! The caller decides on Claude Code's own files, which must show whether ignored or not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// A folder's `.gitignore` matcher, with the file's mtime when it was read (`None`: no file).
type DirIgnore = (Option<SystemTime>, Arc<Gitignore>);

pub struct Visibility {
    /// The project root, canonical.
    root: PathBuf,
    /// Off when Claude Code's `respectGitignore` is false: everything shows.
    respect: bool,
    /// The repository's top folder when the project is inside one, at or above `root`.
    top: Option<PathBuf>,
    /// `.git/info/exclude`, then the global excludes file: below every `.gitignore`.
    exclude: Gitignore,
    global: Gitignore,
    /// Each folder's `.gitignore`, by absolute folder.
    dirs: Mutex<HashMap<PathBuf, DirIgnore>>,
}

impl Visibility {
    pub fn new(root: &Path, respect: bool) -> Visibility {
        let top = root.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf);
        let exclude = top.as_ref().map_or_else(Gitignore::empty, |top| {
            let mut builder = GitignoreBuilder::new(top);
            builder.add(top.join(".git/info/exclude"));
            builder.build().unwrap_or_else(|_| Gitignore::empty())
        });
        let global = GitignoreBuilder::new(top.as_deref().unwrap_or(root)).build_global().0;
        Visibility { root: root.to_path_buf(), respect, top, exclude, global, dirs: Mutex::new(HashMap::new()) }
    }

    /// Whether `relative`, a path below the root, shows: neither it nor a folder above it is ignored.
    pub fn shows(&self, relative: &Path, is_dir: bool) -> bool {
        if !self.respect {
            return true;
        }
        let mut prefix = self.root.clone();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            prefix.push(component);
            let last = components.peek().is_none();
            if self.ignored(&prefix, !last || is_dir) {
                return false;
            }
        }
        true
    }

    /// Whether git ignores `path` itself (absolute, below the root), ignoring the folders above it.
    fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        let base = self.top.as_deref().unwrap_or(&self.root);
        for dir in path.ancestors().skip(1).take_while(|d| d.starts_with(base)) {
            match self.dir(dir).matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        for matcher in [&self.exclude, &self.global] {
            match matcher.matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }

    /// The `.gitignore` in `dir`, read again when it changed (or appeared, or went).
    fn dir(&self, dir: &Path) -> Arc<Gitignore> {
        let file = dir.join(".gitignore");
        let mtime = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
        let mut dirs = self.dirs.lock().unwrap();
        if let Some((seen, matcher)) = dirs.get(dir)
            && *seen == mtime
        {
            return matcher.clone();
        }
        let matcher = Arc::new(if mtime.is_some() { Gitignore::new(&file).0 } else { Gitignore::empty() });
        dirs.insert(dir.to_path_buf(), (mtime, matcher.clone()));
        matcher
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ct-visible-{}-{n}", std::process::id()));
        for (path, content) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    #[test]
    fn ignored_paths_and_everything_below_them_are_hidden_with_gits_precedence() {
        let root = tree(&[
            (".git/info/exclude", "local-only.txt\n"),
            (".gitignore", "target/\n*.log\n!keep.log\nsub/generated\n"),
            ("sub/.gitignore", "!again.log\nnested/\n"),
            ("src/main.rs", ""),
        ]);
        let v = Visibility::new(&root, true);
        let shows = |p: &str, dir| v.shows(Path::new(p), dir);
        assert!(shows("src/main.rs", false));
        assert!(!shows("target", true));
        assert!(!shows("target/debug/deps/x.rlib", false));
        assert!(!shows("app.log", false));
        assert!(shows("keep.log", false));
        // A deeper .gitignore wins over a shallower one, and can only re-include what is not below an ignored folder.
        assert!(shows("sub/again.log", false));
        assert!(!shows("sub/nested/a.txt", false));
        assert!(!shows("sub/generated", true));
        assert!(!shows("local-only.txt", false));
        assert!(Visibility::new(&root, false).shows(Path::new("target/debug"), true));
    }

    #[test]
    fn a_changed_gitignore_takes_effect_and_a_project_below_the_repository_top_gets_its_rules() {
        let root = tree(&[(".git/HEAD", ""), (".gitignore", "build/\n"), ("app/.gitignore", "")]);
        let app = Visibility::new(&root.join("app"), true);
        assert!(!app.shows(Path::new("build/out"), false), "the repository's own .gitignore holds in a subfolder project");
        // mtime resolution can be coarse: make sure the change is seen as one.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(root.join("app/.gitignore"), "*.tmp\n").unwrap();
        assert!(!app.shows(Path::new("x.tmp"), false));
        assert!(app.shows(Path::new("x.txt"), false));
    }
}
