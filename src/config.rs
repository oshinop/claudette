//! Settings: `~/.config/claudette/config.toml`, each overridden by its `CLAUDETTE_<KEY>` variable.

use std::path::PathBuf;
use std::sync::LazyLock;

#[derive(Clone)]
pub struct Config {
    /// Anything `ssh` accepts as a destination; ports, users and keys go in ~/.ssh/config.
    pub host: String,
    /// The ssh command and its options, without the destination.
    pub ssh: Vec<String>,
    /// The claude command on the remote machine.
    pub remote_claude: String,
    /// A command on the remote machine that relays stdio to a Unix socket (`<cmd> -U <path>`).
    pub remote_nc: String,
    /// Environment variables to pass to the remote claude by name (`forward_env`), always.
    pub forward_env: Vec<String>,
    /// Whether Claude Code's own variables go to the remote claude as well, but for the ones bound
    /// to this machine (`forward_claude_env`, on by default).
    pub forward_claude_env: bool,
    /// Append every frame crossing the relay to this file (`log`). Holds prompts and file contents.
    pub log_file: Option<String>,
    /// How long the remote machine keeps a session whose local machine went away, in seconds.
    pub detach_timeout: u64,
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// claudette's own configuration folder: `~/.config/claudette`.
pub fn claudette_dir() -> PathBuf {
    home().join(".config").join("claudette")
}

/// The folder this user's runs keep their sockets in, the same for every process of the user however
/// it was started (a host's background service may set its own `TMPDIR`): the per-user temporary
/// folder on macOS, `XDG_RUNTIME_DIR` or a private `/tmp/claudette-<uid>` elsewhere. A folder
/// another user owns or can write to is refused.
pub fn runtime_dir() -> anyhow::Result<PathBuf> {
    let dir = platform_runtime_dir()?;
    check_private(&dir)?;
    Ok(dir)
}

#[cfg(target_os = "macos")]
fn platform_runtime_dir() -> anyhow::Result<PathBuf> {
    let mut buf = vec![0u8; 1024];
    // SAFETY: confstr writes at most buf.len() bytes, NUL-terminated, and returns the length needed.
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    anyhow::ensure!(n > 0 && n <= buf.len(), "cannot find this user's temporary folder");
    buf.truncate(n - 1);
    Ok(PathBuf::from(String::from_utf8(buf)?))
}

#[cfg(not(target_os = "macos"))]
fn platform_runtime_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(PathBuf::from)
        && dir.is_dir()
    {
        return Ok(dir);
    }
    private_tmp_dir()
}

/// `/tmp/claudette-<uid>`, created private if missing and refused when another user owns it or
/// can write to it. Short on purpose: it holds the shared ssh connection's socket, whose path (with
/// `%C`'s 40 characters and the suffix ssh adds while creating it) must fit in 104 bytes, which the
/// per-user temporary folder on macOS leaves no room for.
pub fn private_tmp_dir() -> anyhow::Result<PathBuf> {
    let dir = PathBuf::from(format!("/tmp/claudette-{}", nix::unistd::getuid()));
    if let Err(e) = nix::unistd::mkdir(&dir, nix::sys::stat::Mode::from_bits_truncate(0o700))
        && e != nix::errno::Errno::EEXIST
    {
        anyhow::bail!("cannot create {}: {e}", dir.display());
    }
    check_private(&dir)?;
    Ok(dir)
}

fn check_private(dir: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir)?;
    let uid = nix::unistd::getuid().as_raw();
    anyhow::ensure!(meta.is_dir() && meta.uid() == uid, "{} is not a folder this user owns", dir.display());
    anyhow::ensure!(meta.mode() & 0o022 == 0, "{} can be written by other users", dir.display());
    Ok(())
}

/// The settings file. A key's variable is `CLAUDETTE_` and the key in capitals, and wins over it.
pub fn config_file() -> PathBuf {
    claudette_dir().join("config.toml")
}

/// Every setting, as `config.toml` names it.
const KEYS: [&str; 9] = ["host", "ssh", "remote_claude", "remote_nc", "forward_claude_env", "forward_env", "detach_timeout", "log", "prefix"];

type Vars<'a> = &'a (dyn Fn(&str) -> Option<String> + Sync);

/// Where settings come from: the variables, then the file.
pub struct Settings<'a> {
    file: toml::Table,
    var: Vars<'a>,
}

/// A setting as given, and where, for errors.
enum Given {
    Var(String, String),
    File(String, toml::Value),
}

impl<'a> Settings<'a> {
    /// `text`: the file's content, if any. Unknown keys are refused, so a typo is not silently
    /// ignored.
    pub fn new(text: Option<&str>, var: Vars<'a>) -> anyhow::Result<Settings<'a>> {
        let path = config_file();
        let file: toml::Table = match text {
            Some(text) => text.parse().map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?,
            None => toml::Table::new(),
        };
        if let Some(key) = file.keys().find(|k| !KEYS.contains(&k.as_str())) {
            anyhow::bail!("{}: unknown setting `{key}`; the settings are {}", path.display(), KEYS.join(", "));
        }
        Ok(Settings { file, var })
    }

    fn given(&self, key: &str) -> Option<Given> {
        let name = format!("CLAUDETTE_{}", key.to_ascii_uppercase());
        match (self.var)(&name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) {
            Some(value) => Some(Given::Var(name, value)),
            None => self.file.get(key).map(|v| Given::File(format!("`{key}` in {}", config_file().display()), v.clone())),
        }
    }

    pub fn text(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(match self.given(key) {
            None => None,
            Some(Given::Var(_, v)) => Some(v),
            Some(Given::File(_, toml::Value::String(s))) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
            Some(Given::File(w, _)) => anyhow::bail!("{w}: use a string"),
        })
    }

    /// Words: from a variable or a string, split by `split`, or a TOML array of strings.
    pub fn list(&self, key: &str, split: fn(&str) -> Vec<String>) -> anyhow::Result<Option<Vec<String>>> {
        Ok(match self.given(key) {
            None => None,
            Some(Given::Var(_, v) | Given::File(_, toml::Value::String(v))) => Some(split(&v)),
            Some(Given::File(w, toml::Value::Array(items))) => {
                Some(items.into_iter().map(|i| i.as_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("{w}: use strings"))).collect::<anyhow::Result<_>>()?)
            }
            Some(Given::File(w, _)) => anyhow::bail!("{w}: use a list of strings"),
        })
    }

    pub fn switch(&self, key: &str) -> anyhow::Result<Option<bool>> {
        Ok(match self.given(key) {
            None => None,
            Some(Given::Var(name, v)) => Some(parse_switch(&v).ok_or_else(|| anyhow::anyhow!("{name}={v}: use 1 or 0 (true/false, yes/no, on/off)"))?),
            Some(Given::File(_, toml::Value::Boolean(b))) => Some(b),
            Some(Given::File(w, _)) => anyhow::bail!("{w}: use true or false"),
        })
    }

    /// Seconds: `90`, or a string with a unit, `90s`, `30m`, `12h`.
    pub fn duration(&self, key: &str) -> anyhow::Result<Option<u64>> {
        let bad = |w: &str| anyhow::anyhow!("{w}: use seconds, or a number with s, m or h, such as \"12h\"");
        Ok(match self.given(key) {
            None => None,
            Some(Given::Var(name, v)) => Some(parse_duration(&v).ok_or_else(|| bad(&format!("{name}={v}")))?),
            Some(Given::File(w, toml::Value::String(v))) => Some(parse_duration(&v).ok_or_else(|| bad(&w))?),
            Some(Given::File(w, toml::Value::Integer(n))) => Some(u64::try_from(n).map_err(|_| bad(&w))?),
            Some(Given::File(w, _)) => return Err(bad(&w)),
        })
    }
}

/// This process's settings: the variables, then `config.toml`, read once.
static SETTINGS: LazyLock<Result<Settings<'static>, String>> = LazyLock::new(|| {
    static VAR: fn(&str) -> Option<String> = |name| std::env::var(name).ok();
    let text = std::fs::read_to_string(config_file()).ok();
    Settings::new(text.as_deref(), &VAR).map_err(|e| e.to_string())
});

fn settings() -> anyhow::Result<&'static Settings<'static>> {
    SETTINGS.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Keepalives make a dead link fail in about a minute instead of hanging the protocol. OpenSSH keeps
/// the first value it sees, so an earlier `-o` in CLAUDETTE_SSH still wins.
const KEEPALIVE: [&str; 4] = ["-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"];

/// Fail instead of prompting for a password, passphrase or host key.
pub const BATCH: [&str; 2] = ["-o", "BatchMode=yes"];


pub fn is_ssh_binary(command: &str) -> bool {
    command == "ssh" || command.ends_with("/ssh")
}

/// What marks claudette's own commands (`+doctor`): `prefix`, `+` by default. A prefix starting with
/// `-` is refused: it would take claude's options for claudette's commands.
pub fn prefix() -> anyhow::Result<String> {
    let prefix = settings()?.text("prefix")?.unwrap_or_else(|| "+".into());
    anyhow::ensure!(!prefix.starts_with('-'), "prefix {prefix}: a prefix starting with `-` would take claude's options");
    Ok(prefix)
}

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        let s = settings()?;
        let host = s.text("host")?.ok_or_else(|| anyhow::anyhow!("no remote host: set `host` in {} (or CLAUDETTE_HOST)", config_file().display()))?;
        // ssh reads a destination starting with `-` as an option (`-oProxyCommand=…` runs a command here).
        anyhow::ensure!(!host.starts_with('-'), "the remote host {host:?} starts with `-`, which ssh would read as an option");
        let custom = s.list("ssh", |v| v.split_whitespace().map(str::to_owned).collect())?.unwrap_or_default();
        let mut ssh = if let Some(first) = custom.first() {
            let mut ssh = custom.clone();
            if is_ssh_binary(first) {
                ssh.extend(KEEPALIVE.iter().map(|s| (*s).to_owned()));
            }
            ssh
        } else {
            // One shared connection: a host opens sessions and probes in quick succession. Its socket
            // lives in a private folder, so no other user can put one there first.
            let mut ssh: Vec<String> = ["ssh", "-o", "ControlMaster=auto"].iter().map(|s| (*s).to_owned()).collect();
            ssh.push("-o".into());
            ssh.push(format!("ControlPath={}/%C", private_tmp_dir()?.display()));
            ssh.extend(["-o", "ControlPersist=10m"].iter().map(|s| (*s).to_owned()));
            ssh.extend(KEEPALIVE.iter().map(|s| (*s).to_owned()));
            ssh
        };
        if is_ssh_binary(&ssh[0]) && !crate::run::is_tty() {
            // Nobody can answer a prompt: a host's server, or a daemon still holding a shell's terminal,
            // where ssh would ask on a terminal no one watches. After the `ssh` setting, so its own -o wins.
            ssh.extend(BATCH.iter().map(|s| (*s).to_owned()));
        }
        Ok(Config {
            host,
            ssh,
            remote_claude: s.text("remote_claude")?.unwrap_or_else(|| "claude".into()),
            remote_nc: s.text("remote_nc")?.unwrap_or_else(|| "nc".into()),
            forward_env: s.list("forward_env", |v| v.split(',').map(|w| w.trim().to_owned()).filter(|w| !w.is_empty()).collect())?.unwrap_or_default(),
            forward_claude_env: s.switch("forward_claude_env")?.unwrap_or(true),
            log_file: s.text("log")?,
            detach_timeout: s.duration("detach_timeout")?.unwrap_or(12 * 3600),
        })
    }
}

/// An on/off variable: `1`, `true`, `yes`, `on` or `0`, `false`, `no`, `off`, in any case.
fn parse_switch(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `90`, `90s`, `30m`, `12h`: seconds.
fn parse_duration(text: &str) -> Option<u64> {
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let n: u64 = number.parse().ok()?;
    match unit {
        "" | "s" => Some(n),
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> + Sync {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn a_variable_wins_over_the_file_and_each_setting_takes_its_natural_toml_type() {
        let file = r#"
            host = "mini"
            ssh = ["ssh", "-o", "ProxyJump=bastion host"]
            forward_env = ["AWS_REGION", "AWS_PROFILE"]
            forward_claude_env = false
            detach_timeout = 90
            prefix = "::"
        "#;
        let env = vars(&[("CLAUDETTE_HOST", "studio"), ("CLAUDETTE_DETACH_TIMEOUT", "30m")]);
        let s = Settings::new(Some(file), &env).unwrap();
        assert_eq!(s.text("host").unwrap().as_deref(), Some("studio"));
        // An array keeps a word with a space in it, which a variable split on spaces can't.
        assert_eq!(s.list("ssh", |v| v.split_whitespace().map(str::to_owned).collect()).unwrap().unwrap(), ["ssh", "-o", "ProxyJump=bastion host"]);
        assert_eq!(s.list("forward_env", |v| v.split(',').map(str::to_owned).collect()).unwrap().unwrap(), ["AWS_REGION", "AWS_PROFILE"]);
        assert_eq!(s.switch("forward_claude_env").unwrap(), Some(false));
        assert_eq!(s.duration("detach_timeout").unwrap(), Some(1800));
        assert_eq!(s.text("prefix").unwrap().as_deref(), Some("::"));
        assert_eq!(s.text("log").unwrap(), None);
    }

    #[test]
    fn strings_work_where_a_variable_would_and_wrong_types_and_unknown_keys_are_refused() {
        let none = vars(&[]);
        let s = Settings::new(Some("forward_env = \"A, B\"\ndetach_timeout = \"12h\""), &none).unwrap();
        assert_eq!(s.list("forward_env", |v| v.split(',').map(|w| w.trim().to_owned()).collect()).unwrap().unwrap(), ["A", "B"]);
        assert_eq!(s.duration("detach_timeout").unwrap(), Some(43200));
        let wrong = Settings::new(Some("forward_claude_env = \"maybe\""), &none).unwrap();
        assert!(wrong.switch("forward_claude_env").unwrap_err().to_string().contains("use true or false"));
        let typo = Settings::new(Some("hots = \"mini\""), &none).err().unwrap().to_string();
        assert!(typo.contains("unknown setting `hots`"), "{typo}");
        let bad_var = vars(&[("CLAUDETTE_FORWARD_CLAUDE_ENV", "maybe")]);
        assert!(Settings::new(None, &bad_var).unwrap().switch("forward_claude_env").unwrap_err().to_string().contains("CLAUDETTE_FORWARD_CLAUDE_ENV=maybe"));
    }
}
