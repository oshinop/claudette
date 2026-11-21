use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

type DirIgnore = (Option<SystemTime>, Arc<Gitignore>);

pub struct Visibility {
    root: PathBuf,
    respect: bool,
    top: Option<PathBuf>,
    exclude: Gitignore,
    global: Gitignore,
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
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(root.join("app/.gitignore"), "*.tmp\n").unwrap();
        assert!(!app.shows(Path::new("x.tmp"), false));
        assert!(app.shows(Path::new("x.txt"), false));
    }
}
