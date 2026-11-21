use std::collections::BTreeMap;

use crate::config::Config;

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

const FAMILIES: [&str; 6] = ["ANTHROPIC_", "CLAUDE_", "DISABLE_", "ENABLE_", "MCP_", "VERTEX_REGION_"];
const OUTSIDE_FAMILIES: [&str; 10] = [
    "API_FORCE_IDLE_TIMEOUT",
    "API_TIMEOUT_MS",
    "DO_NOT_TRACK",
    "FORCE_PROMPT_CACHING_5M",
    "MAX_MCP_OUTPUT_TOKENS",
    "MAX_STRUCTURED_OUTPUT_RETRIES",
    "MAX_THINKING_TOKENS",
    "SLASH_COMMAND_TOOL_CHAR_BUDGET",
    "TASK_MAX_OUTPUT_LENGTH",
    "USE_BUILTIN_RIPGREP",
];

const CLAUDE_SECRETS: [&str; 10] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_AWS_API_KEY",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "MCP_CLIENT_SECRET",
];

const LOCAL_PATHS: [&str; 16] = [
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_TMPDIR",
    "CLAUDE_CODE_SHELL",
    "CLAUDE_CODE_GIT_BASH_PATH",
    "CLAUDE_CODE_PROJECT_DIR_NAME",
    "CLAUDE_CODE_CLIENT_CERT",
    "CLAUDE_CODE_CLIENT_KEY",
    "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
    "CLAUDE_CODE_DEBUG_LOGS_DIR",
    "CLAUDE_CODE_PLUGIN_CACHE_DIR",
    "CLAUDE_CODE_PLUGIN_DIRS",
    "CLAUDE_CODE_PLUGIN_SEED_DIR",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CLIENT_PRESENCE_FILE",
    "CLAUDE_ENV_FILE",
    "CLAUDE_JOB_DIR",
];

const SET_BY_CLAUDE: [&str; 8] = [
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_REMOTE",
    "CLAUDE_CODE_REMOTE_SESSION_ID",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

pub fn is_credential(name: &str) -> bool {
    ["_KEY", "_TOKEN", "_SECRET", "_PASSPHRASE", "_PASSWORD"].iter().any(|s| name.ends_with(s)) || CLAUDE_SECRETS.contains(&name)
}

pub fn machine_bound(name: &str) -> bool {
    LOCAL_PATHS.contains(&name) || SET_BY_CLAUDE.contains(&name) || (is_credential(name) && !CLAUDE_SECRETS.contains(&name))
}

pub fn forwarded_env<'a>(env: impl IntoIterator<Item = (String, String)>, extra: &'a [String], claude_env: bool) -> BTreeMap<String, String> {
    let claude_owned = |key: &str| (FAMILIES.iter().any(|f| key.starts_with(f)) || OUTSIDE_FAMILIES.contains(&key) || CLAUDE_SECRETS.contains(&key)) && !machine_bound(key);
    env.into_iter().filter(|(key, _)| extra.iter().any(|e| e == key) || (claude_env && claude_owned(key))).collect()
}

const REMOTE_PATH: &str = r#"PATH="$HOME/.local/bin:$PATH""#;

#[derive(Default)]
pub struct Script {
    pub setup: String,
    pub cleanup: String,
}

pub fn claude_command(config: &Config, script: &Script, args: &[String]) -> String {
    let mut run = vec![shell_quote(&config.remote_claude)];
    run.extend(args.iter().map(|a| shell_quote(a)));
    let run = run.join(" ");
    let setup = &script.setup;
    if script.cleanup.is_empty() {
        return format!("{REMOTE_PATH}\nset --\n{setup}\nexec \"$@\" {run}");
    }
    let traps = format!("trap {} EXIT\ntrap 'exit 129' HUP\ntrap 'exit 130' INT\ntrap 'exit 143' TERM", shell_quote(&script.cleanup));
    format!("{REMOTE_PATH}\n{traps}\nset --\n{setup}\n\"$@\" {run}")
}

pub fn ssh_argv(config: &Config, command: &str, tty: bool) -> Vec<String> {
    let mut argv = config.ssh.clone();
    argv.push(if tty { "-t" } else { "-T" }.into());
    argv.push(config.host.clone());
    argv.push(command.into());
    argv
}

pub fn failure_hint(config: &Config, status: std::process::ExitStatus) -> String {
    if status.code() != Some(255) {
        return String::new();
    }
    format!("\nssh could not connect without a prompt: run `ssh {}` once in a terminal (to accept its host key), and keep the key in an agent", config.host)
}

pub struct SocketForward {
    pub remote: String,
    pub local: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(remote_claude: &str, ssh: &[&str]) -> Config {
        Config {
            host: "h".into(),
            ssh: ssh.iter().map(|s| (*s).to_owned()).collect(),
            remote_claude: remote_claude.into(),
            remote_nc: "nc".into(),
            forward_env: vec![],
            forward_claude_env: true,
            log_file: None,
            detach_timeout: 60,
        }
    }

    fn run_sh(command: &str, home: &std::path::Path) -> (i32, String) {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .env_clear()
            .env("HOME", home)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .output()
            .expect("sh runs");
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
    }

    #[test]
    fn arguments_reach_the_remote_claude_intact() {
        let home = tempdir();
        let fake = home.join("fake-claude");
        std::fs::write(&fake, "#!/bin/sh\nfor a in \"$@\"; do printf '<%s>\\n' \"$a\"; done\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let tricky: Vec<String> = [r#"it's "quoted""#, "$HOME", "a b", "`date`", r#"{"json":true}"#].iter().map(|s| (*s).to_owned()).collect();
        let (code, out) = run_sh(&claude_command(&config(fake.to_str().unwrap(), &["ssh"]), &Script::default(), &tricky), &home);
        assert_eq!(code, 0);
        let expected: Vec<String> = tricky.iter().map(|a| format!("<{a}>")).collect();
        assert_eq!(out.trim_end().split('\n').map(str::to_owned).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn cleanup_runs_after_claude_exits_and_its_status_survives() {
        let home = tempdir();
        let fake = home.join("fake-claude");
        std::fs::write(&fake, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let marker = home.join("it's here");
        std::fs::write(&marker, "").unwrap();
        let cleanup = format!("rm -f -- {}", shell_quote(marker.to_str().unwrap()));
        let script = Script { setup: String::new(), cleanup };
        let (code, _) = run_sh(&claude_command(&config(fake.to_str().unwrap(), &["ssh"]), &script, &[]), &home);
        assert_eq!(code, 7);
        assert!(!marker.exists());
    }

    #[test]
    fn claude_codes_variables_credentials_and_provider_go_but_this_machines_paths_and_session_stay() {
        let env = [
            ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
            ("ENABLE_TOOL_SEARCH", "false"),
            ("ANTHROPIC_MODEL", "claude-opus-5-5"),
            ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "50"),
            ("DO_NOT_TRACK", "1"),
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "oauth-secret"),
            ("AWS_BEARER_TOKEN_BEDROCK", "bedrock-secret"),
            ("ANTHROPIC_BASE_URL", "https://proxy"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("VERTEX_REGION_CLAUDE_5_SONNET", "europe-west1"),
            ("CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR", "1"),
            ("MCP_GITHUB_TOKEN", "ghp-for-a-server"),
            ("CLAUDE_CONFIG_DIR", "/Users/me/.claude-work"),
            ("CLAUDE_CODE_TMPDIR", "/Users/me/tmp"),
            ("CLAUDE_CODE_SESSION_ID", "parent-session"),
            ("CLAUDE_CODE_MESSAGING_TOKEN", "parent-token"),
            ("CLAUDE_CODE_CLIENT_KEY_PASSPHRASE", "for-a-key-file-here"),
            ("HOME", "/Users/me"),
            ("GITHUB_TOKEN", "ghp-secret"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let kept = forwarded_env(env.clone(), &[], true);
        assert_eq!(
            kept.keys().collect::<Vec<_>>(),
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_MODEL",
                "AWS_BEARER_TOKEN_BEDROCK",
                "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
                "CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR",
                "CLAUDE_CODE_ENTRYPOINT",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_CODE_USE_BEDROCK",
                "DO_NOT_TRACK",
                "ENABLE_TOOL_SEARCH",
                "VERTEX_REGION_CLAUDE_5_SONNET",
            ]
        );
        assert!(forwarded_env(env.clone(), &["GITHUB_TOKEN".into()], true).contains_key("GITHUB_TOKEN"));
        assert_eq!(forwarded_env(env, &["ANTHROPIC_MODEL".into()], false).keys().collect::<Vec<_>>(), ["ANTHROPIC_MODEL"]);
    }

    pub(crate) fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ct-test-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn rand_suffix() -> u64 {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64);
        h.finish()
    }
}
