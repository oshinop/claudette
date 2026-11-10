//! `Read(…)` and `Edit(…)` permission rules, as Claude Code applies them to its own file tools.
//!
//! claude cannot apply them to the local tools: to it those are MCP tools, and Claude Code matches
//! only bare names against MCP tools. So claudette applies them itself, with Claude Code's
//! semantics (code.claude.com/docs/en/permissions, "Read and Edit"):
//!
//! - gitignore patterns, anchored by their prefix: `//path` at the filesystem root, `~/path` at the
//!   home folder, `/path` at the source's own directory (the project for project and local settings
//!   and the command line, `~/.claude` for user settings), `path` and `./path` at the working
//!   directory. A relative single-folder pattern (`src/**`) matches at any depth in deny and ask
//!   rules, only at the top in allow rules. `!` carves out of the earlier relative rules of the same
//!   source and kind.
//! - deny beats ask beats allow. A deny matches when the requested path or the file it resolves to
//!   matches; an allow only when both do.
//! - `Read` covers Read, Glob and Grep; `Edit` covers Edit, Write and NotebookEdit; a `Read` deny also
//!   blocks Edit and Write (not NotebookEdit).

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Edit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Behavior {
    Allow,
    Ask,
    Deny,
}

/// Where a rule comes from, which decides what a `/path` pattern is anchored at.
pub struct Source<'a> {
    /// The directory `/path` patterns anchor at.
    pub anchor: &'a Path,
    /// `(behavior, rule)` pairs in the order the source lists them, such as `(Deny, "Read(./.env)")`.
    pub rules: Vec<(Behavior, String)>,
}

struct Group {
    kind: Kind,
    behavior: Behavior,
    matcher: Gitignore,
}

#[derive(Default)]
pub struct PathRules {
    groups: Vec<Group>,
}

/// The tool family a path rule kind applies to.
fn kind_of(tool: &str) -> Option<Kind> {
    match tool {
        "Read" => Some(Kind::Read),
        "Edit" => Some(Kind::Edit),
        _ => None,
    }
}

/// A rule such as `Read(./.env)` as (kind, pattern); None for bare names and other tools.
pub fn parse(rule: &str) -> Option<(Kind, &str)> {
    let (tool, rest) = rule.split_once('(')?;
    let pattern = rest.strip_suffix(')')?;
    (!pattern.is_empty() && pattern != "*").then_some(())?;
    Some((kind_of(tool)?, pattern))
}

impl PathRules {
    /// Builds the rules of each source; `cwd` is the session's working directory, `home` the home folder.
    pub fn new(sources: &[Source], cwd: &Path, home: &Path) -> PathRules {
        let mut groups = vec![];
        for source in sources {
            // (kind, behavior, anchor) → patterns, in order, so a `!` reaches the rules before it.
            let mut builders: Vec<(Kind, Behavior, PathBuf, GitignoreBuilder)> = vec![];
            for (behavior, rule) in &source.rules {
                let Some((kind, pattern)) = parse(rule) else { continue };
                let (anchor, line) = anchor(pattern, *behavior, source.anchor, cwd, home);
                let index = match builders.iter().position(|(k, b, a, _)| *k == kind && b == behavior && *a == anchor) {
                    Some(i) => i,
                    None => {
                        builders.push((kind, *behavior, anchor.clone(), GitignoreBuilder::new(&anchor)));
                        builders.len() - 1
                    }
                };
                let _ = builders[index].3.add_line(None, &line);
            }
            for (kind, behavior, _, builder) in builders {
                if let Ok(matcher) = builder.build() {
                    groups.push(Group { kind, behavior, matcher });
                }
            }
        }
        PathRules { groups }
    }

    fn matches(&self, kind: Kind, behavior: Behavior, path: &Path) -> bool {
        let is_dir = path.is_dir();
        self.groups.iter().filter(|g| g.kind == kind && g.behavior == behavior).any(|g| {
            path.starts_with(g.matcher.path()) && g.matcher.matched_path_or_any_parents(path, is_dir).is_ignore()
        })
    }

    /// The decision for a tool touching `path` (already resolved against the working directory):
    /// None when no rule says anything.
    pub fn decide(&self, tool: &str, path: &Path) -> Option<Behavior> {
        let real = real_path(path);
        let paths: Vec<&Path> = if real == path { vec![path] } else { vec![path, real.as_path()] };
        let (kind, read_deny_too) = match tool {
            "Read" | "Glob" | "Grep" => (Kind::Read, false),
            "Edit" | "Write" => (Kind::Edit, true),
            "NotebookEdit" => (Kind::Edit, false),
            _ => return None,
        };
        let any = |k: Kind, b: Behavior| paths.iter().any(|p| self.matches(k, b, p));
        if any(kind, Behavior::Deny) || (read_deny_too && any(Kind::Read, Behavior::Deny)) {
            return Some(Behavior::Deny);
        }
        if any(kind, Behavior::Ask) {
            return Some(Behavior::Ask);
        }
        paths.iter().all(|p| self.matches(kind, Behavior::Allow, p)).then_some(Behavior::Allow)
    }

    /// A path no read may reach, for filtering search results.
    pub fn read_denied(&self, path: &Path) -> bool {
        self.decide("Read", path) == Some(Behavior::Deny)
    }
}

/// The directory a pattern anchors at, and the gitignore line for it there.
fn anchor(pattern: &str, behavior: Behavior, source: &Path, cwd: &Path, home: &Path) -> (PathBuf, String) {
    if let Some(negated) = pattern.strip_prefix('!') {
        // A carve-out reads relative to the working directory, whatever follows the `!`.
        return (cwd.to_path_buf(), format!("!{}", negated.trim_start_matches("./")));
    }
    if let Some(abs) = pattern.strip_prefix("//") {
        return (PathBuf::from("/"), format!("/{abs}"));
    }
    if let Some(rel) = pattern.strip_prefix("~/") {
        return (home.to_path_buf(), format!("/{rel}"));
    }
    if pattern.starts_with('/') {
        return (source.to_path_buf(), pattern.to_owned());
    }
    let rel = pattern.strip_prefix("./").unwrap_or(pattern);
    // A single-folder pattern (`src/**`) reaches nested copies in deny and ask rules only.
    let single_folder = rel.strip_suffix("/**").is_some_and(|folder| !folder.is_empty() && !folder.contains('/'));
    let line = if single_folder && behavior != Behavior::Allow { format!("**/{rel}") } else { rel.to_owned() };
    (cwd.to_path_buf(), line)
}

/// The file a path resolves to: its real path, or for a file not created yet, its real folder's.
fn real_path(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent().and_then(|p| p.canonicalize().ok()), path.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(project: &[(Behavior, &str)], user: &[(Behavior, &str)], root: &Path, home: &Path) -> PathRules {
        let s = |list: &[(Behavior, &str)]| list.iter().map(|(b, r)| (*b, (*r).to_owned())).collect();
        let user_anchor = home.join(".claude");
        PathRules::new(&[Source { anchor: root, rules: s(project) }, Source { anchor: &user_anchor, rules: s(user) }], root, home)
    }

    fn setup() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("ct-rules-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let (root, home) = (base.join("proj"), base.join("home"));
        for d in ["src", "vendor/pkg/src", "secrets/public", "docs"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_rsa"), "key").unwrap();
        (root.canonicalize().unwrap(), home.canonicalize().unwrap())
    }

    use Behavior::*;

    #[test]
    fn bare_names_match_at_any_depth_and_deny_wins() {
        let (root, home) = setup();
        let r = rules(&[(Deny, "Read(./.env)"), (Allow, "Read(src/**)")], &[], &root, &home);
        assert_eq!(r.decide("Read", &root.join(".env")), Some(Deny));
        assert_eq!(r.decide("Grep", &root.join("src/.env")), Some(Deny));
        assert_eq!(r.decide("Read", &root.join("src/app.ts")), Some(Allow));
        assert_eq!(r.decide("Read", &root.join("docs/a.md")), None);
        // A Read deny also blocks writing the file, but not a notebook edit.
        assert_eq!(r.decide("Write", &root.join(".env")), Some(Deny));
        assert_eq!(r.decide("NotebookEdit", &root.join(".env")), None);
    }

    #[test]
    fn single_folder_patterns_reach_nested_copies_only_when_denying() {
        let (root, home) = setup();
        let allow = rules(&[(Allow, "Edit(src/**)")], &[], &root, &home);
        assert_eq!(allow.decide("Edit", &root.join("src/app.ts")), Some(Allow));
        assert_eq!(allow.decide("Edit", &root.join("vendor/pkg/src/lib.js")), None);
        let deny = rules(&[(Deny, "Edit(src/**)")], &[], &root, &home);
        assert_eq!(deny.decide("Edit", &root.join("vendor/pkg/src/lib.js")), Some(Deny));
        let anchored = rules(&[(Deny, "Edit(/src/**)")], &[], &root, &home);
        assert_eq!(anchored.decide("Edit", &root.join("vendor/pkg/src/lib.js")), None);
        assert_eq!(anchored.decide("Edit", &root.join("src/app.ts")), Some(Deny));
    }

    #[test]
    fn anchors_follow_the_prefix_and_the_source() {
        let (root, home) = setup();
        let r = rules(&[(Deny, "Read(~/.ssh/**)"), (Ask, "Edit(//etc/hosts)")], &[(Deny, "Read(/secrets/**)")], &root, &home);
        assert_eq!(r.decide("Read", &home.join(".ssh/id_rsa")), Some(Deny));
        assert_eq!(r.decide("Edit", Path::new("/etc/hosts")), Some(Ask));
        // In user settings, `/secrets` is ~/.claude/secrets, not the project's.
        assert_eq!(r.decide("Read", &root.join("secrets/a")), None);
        assert_eq!(r.decide("Read", &home.join(".claude/secrets/a")), Some(Deny));
    }

    #[test]
    fn a_symlink_to_a_denied_file_is_denied_and_carve_outs_reach_earlier_relative_rules() {
        let (root, home) = setup();
        std::os::unix::fs::symlink(home.join(".ssh/id_rsa"), root.join("src/key")).unwrap();
        let r = rules(&[(Deny, "Read(~/.ssh/**)"), (Allow, "Read(src/**)")], &[], &root, &home);
        assert_eq!(r.decide("Read", &root.join("src/key")), Some(Deny));
        let carved = rules(&[(Deny, "Read(*.env)"), (Deny, "Read(!sample.env)")], &[], &root, &home);
        assert_eq!(carved.decide("Read", &root.join("prod.env")), Some(Deny));
        assert_eq!(carved.decide("Read", &root.join("sample.env")), None);
    }
}
