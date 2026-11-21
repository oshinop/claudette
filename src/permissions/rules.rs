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

pub struct Source<'a> {
    pub anchor: &'a Path,
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

fn kind_of(tool: &str) -> Option<Kind> {
    match tool {
        "Read" => Some(Kind::Read),
        "Edit" => Some(Kind::Edit),
        _ => None,
    }
}

pub fn parse(rule: &str) -> Option<(Kind, &str)> {
    let (tool, rest) = rule.split_once('(')?;
    let pattern = rest.strip_suffix(')')?;
    (!pattern.is_empty() && pattern != "*").then_some(())?;
    Some((kind_of(tool)?, pattern))
}

impl PathRules {
    pub fn new(sources: &[Source], cwd: &Path, home: &Path) -> PathRules {
        let mut groups = vec![];
        for source in sources {
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

    pub fn read_denied(&self, path: &Path) -> bool {
        self.decide("Read", path) == Some(Behavior::Deny)
    }
}

fn anchor(pattern: &str, behavior: Behavior, source: &Path, cwd: &Path, home: &Path) -> (PathBuf, String) {
    if let Some(negated) = pattern.strip_prefix('!') {
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
    let single_folder = rel.strip_suffix("/**").is_some_and(|folder| !folder.is_empty() && !folder.contains('/'));
    let line = if single_folder && behavior != Behavior::Allow { format!("**/{rel}") } else { rel.to_owned() };
    (cwd.to_path_buf(), line)
}

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
