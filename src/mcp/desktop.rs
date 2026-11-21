use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::config::{self as mcp, Place, Scope};
use crate::config::home;
use crate::json::Object;

const CONFIG: &str = "Claude/claude_desktop_config.json";

fn config_path() -> Result<Option<PathBuf>, &'static str> {
    if cfg!(target_os = "macos") {
        return Ok(Some(home().join("Library/Application Support").join(CONFIG)));
    }
    let wsl = std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|r| r.to_ascii_lowercase().contains("microsoft"));
    if !wsl {
        return Err("Claude Desktop is only supported on macOS and WSL.");
    }
    let found: Vec<PathBuf> = std::fs::read_dir("/mnt/c/Users")
        .map(|users| users.flatten().map(|u| u.path().join("AppData/Roaming").join(CONFIG)).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    Ok(found.into_iter().next())
}

fn stdio_servers(path: &Path) -> Vec<(String, Object)> {
    let Some(Value::Object(root)) = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()) else { return vec![] };
    let Some(Value::Object(servers)) = root.get("mcpServers") else { return vec![] };
    servers.iter().filter_map(|(name, config)| Some((name.clone(), config.as_object().filter(|c| c.get("command").is_some_and(Value::is_string))?.clone()))).collect()
}

struct Item {
    name: String,
    config: Object,
    taken: bool,
    checked: bool,
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn frame(items: &[Item], cursor: usize, done: bool) -> Vec<String> {
    let width = crate::tunnel::terminal_size().map_or(80, |(_, cols)| usize::from(cols));
    let mut lines = vec!["─".repeat(width), "  Import MCP servers from Claude Desktop".into(), format!("  Found {} MCP server{} in Claude Desktop.", items.len(), plural(items.len()))];
    if items.iter().any(|i| i.taken) {
        lines.extend(["".into(), "  ⚠ Some of these names are taken. If selected, those servers get a number".into(), "    added to the name.".into()]);
    }
    lines.extend(["".into(), "  Which servers do you want to import?".into(), "".into()]);
    let label_width = items.iter().filter(|i| i.taken).map(|i| i.name.chars().count()).max().unwrap_or(0);
    for (index, item) in items.iter().enumerate() {
        let pointer = if index == cursor { "  ❯ " } else { "    " };
        let mark = if item.checked { "✔" } else { "◯" };
        let label = if item.taken { format!("{}   already exists", " ".repeat(label_width - item.name.chars().count())) } else { String::new() };
        lines.push(format!("{pointer}{mark} {}{label}", item.name));
    }
    if !done {
        lines.push(format!("{}  Import selected", if cursor == items.len() { "  ❯ " } else { "    " }));
        lines.push("  Space to toggle · Enter to select · Esc to cancel".into());
    }
    lines
}

fn draw(out: &mut impl Write, lines: &[String], previous: usize) -> std::io::Result<()> {
    if previous > 0 {
        write!(out, "\x1b[{previous}F\x1b[J")?;
    }
    write!(out, "{}\r\n", lines.join("\r\n"))?;
    out.flush()
}

enum Key {
    Up,
    Down,
    Toggle,
    Enter,
    Cancel,
    Other,
}

fn read_key(input: &mut impl Read) -> std::io::Result<Key> {
    let mut buf = [0u8; 16];
    let n = input.read(&mut buf)?;
    Ok(match &buf[..n] {
        [] | [0x03] | [0x1b] => Key::Cancel,
        [0x1b, b'[', b'A'] | [0x1b, b'O', b'A'] | [b'k'] => Key::Up,
        [0x1b, b'[', b'B'] | [0x1b, b'O', b'B'] | [b'j'] => Key::Down,
        [b' '] => Key::Toggle,
        [b'\r'] | [b'\n'] => Key::Enter,
        _ => Key::Other,
    })
}

fn choose(items: &mut [Item]) -> anyhow::Result<bool> {
    let _raw = crate::tunnel::RawMode::enter();
    let mut out = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    let mut cursor = 0;
    let mut shown = 0;
    loop {
        let lines = frame(items, cursor, false);
        draw(&mut out, &lines, shown)?;
        shown = lines.len();
        match read_key(&mut input)? {
            Key::Up => cursor = cursor.saturating_sub(1),
            Key::Down => cursor = (cursor + 1).min(items.len()),
            Key::Toggle | Key::Enter if cursor < items.len() => items[cursor].checked = !items[cursor].checked,
            Key::Enter => {
                draw(&mut out, &frame(items, cursor, true), shown)?;
                return Ok(true);
            }
            Key::Cancel => {
                draw(&mut out, &frame(items, cursor, true), shown)?;
                return Ok(false);
            }
            Key::Toggle | Key::Other => {}
        }
    }
}

fn free_name(cwd: &Path, scope: Scope, name: &str) -> String {
    (1..).map(|n| format!("{name}_{n}")).find(|candidate| mcp::files_with(cwd, scope, candidate).is_empty()).expect("an unused name")
}

pub fn import(scope: Scope, cwd: &Path) -> anyhow::Result<Result<String, String>> {
    let path = match config_path() {
        Ok(path) => path,
        Err(message) => return Ok(Err(message.into())),
    };
    let servers = path.map(|p| stdio_servers(&p)).unwrap_or_default();
    if servers.is_empty() {
        return Ok(Ok("No MCP servers found in the Claude Desktop config, or the file doesn't exist.".into()));
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(Err("claude mcp add-from-claude-desktop needs a terminal to choose the servers to import".into()));
    }
    let mut items: Vec<Item> = servers
        .into_iter()
        .map(|(name, config)| {
            let taken = !mcp::files_with(cwd, scope, &name).is_empty();
            Item { name, config, taken, checked: !taken }
        })
        .collect();
    if !choose(&mut items)? {
        return Ok(Ok("No servers were imported.".into()));
    }
    let chosen: Vec<&Item> = items.iter().filter(|i| i.checked).collect();
    if chosen.is_empty() {
        return Ok(Ok("No servers were imported.".into()));
    }
    for item in &chosen {
        let name = if item.taken { free_name(cwd, scope, &item.name) } else { item.name.clone() };
        mcp::add(cwd, scope, &name, item.config.clone(), Place::Here)?;
    }
    Ok(Ok(format!("Imported {} MCP server{} to {} config.", chosen.len(), plural(chosen.len()), scope.name())))
}
