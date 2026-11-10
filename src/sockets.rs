//! Unix sockets on this machine that the run's holder mirrors on the remote machine: it listens at
//! each one's remote path and carries every connection back here over the link (see
//! [`crate::link`]). Also this run's own socket for the other sessions here. They sit in the run's
//! folder, `claudette-<pid>-XXXXXX` in this user's runtime folder (see
//! [`crate::config::runtime_dir`]). The folder is private to this user (mkdtemp makes it 0700), so
//! only this user's processes can connect; on the remote end the holder makes its sockets under
//! umask 077.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::json::{Object, parse_object, str_field};
use crate::log::trace;
use crate::mcp::bridge::Notify;
use crate::mcp::{Endpoint, id_key, rpc_id};
use crate::mount::{sftp, transcripts, warm, workdir};
use crate::settings::Callbacks;
use crate::tools::Workspace;

pub type HookAnswer = Arc<dyn Fn(&Object) -> Value + Send + Sync>;

/// A file delivered in the first frames of a run.
pub struct RunFile {
    pub name: String,
    pub remote: String,
    pub data: Vec<u8>,
}

/// The sockets one run forwards to the remote machine, and the remote paths they get there.
pub struct Forwards {
    /// Unique to this run; names its files on the remote machine.
    pub run: String,
    pub list: Vec<crate::ssh::SocketForward>,
    /// Files delivered before the session starts; contents never appear on a command line.
    pub files: Vec<RunFile>,
}

impl Forwards {
    pub fn new() -> Forwards {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.write_u32(std::process::id());
        Forwards { run: format!("{:016x}", h.finish()), list: vec![], files: vec![] }
    }

    /// Forwards a local socket; returns its remote path.
    pub fn add(&mut self, local: &std::path::Path) -> String {
        let remote = format!("/tmp/claudette-{}-{}.sock", self.run, self.list.len());
        self.list.push(crate::ssh::SocketForward { remote: remote.clone(), local: local.to_string_lossy().into_owned() });
        remote
    }

    /// Delivers a private file; its contents never appear on a command line.
    pub fn file(&mut self, name: &str, data: Vec<u8>) -> String {
        let remote = format!("/tmp/claudette-{}.{}", self.run, name);
        self.files.push(RunFile { name: name.to_owned(), remote: remote.clone(), data });
        remote
    }
}

pub struct SocketHub {
    dir: PathBuf,
    count: AtomicUsize,
    /// Stops every listener and connection.
    stop: CancellationToken,
}

/// The run folder of this process, once a hub made it; the local Bash tool keeps its files there.
static RUN_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn run_dir() -> Option<&'static PathBuf> {
    RUN_DIR.get()
}

impl SocketHub {
    pub fn new() -> anyhow::Result<SocketHub> {
        let root = crate::config::runtime_dir()?;
        let dir = nix::unistd::mkdtemp(&root.join(format!("claudette-{}-XXXXXX", std::process::id())))?;
        let _ = RUN_DIR.set(dir.clone());
        Ok(SocketHub { dir, count: AtomicUsize::new(0), stop: CancellationToken::new() })
    }

    /// Serves this session to the other sessions here, at `agent.sock` in the run folder.
    pub fn serve_agent(&self, agent: Arc<crate::peers::Agent>) -> anyhow::Result<PathBuf> {
        let path = self.dir.join(crate::peers::SOCKET);
        let listener = UnixListener::bind(&path)?;
        agent.set_socket(path.clone());
        self.spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                tokio::spawn(crate::peers::serve_connection(agent.clone(), conn));
            }
        });
        Ok(path)
    }


    fn listen(&self) -> anyhow::Result<(PathBuf, UnixListener)> {
        let path = self.dir.join(format!("{}.sock", self.count.fetch_add(1, Ordering::Relaxed)));
        let listener = UnixListener::bind(&path)?;
        Ok((path, listener))
    }

    fn spawn(&self, work: impl Future<Output = ()> + Send + 'static) {
        let stop = self.stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = stop.cancelled() => {}
                _ = work => {}
            }
        });
    }

    /// Serves an MCP endpoint with MCP's stdio framing: one JSON-RPC message per line, both ways.
    /// `name` labels its traffic in the frame log. The notifier sends the endpoint's own
    /// notifications to every connected client.
    pub fn serve_mcp(&self, name: &str, endpoint: Endpoint) -> anyhow::Result<(PathBuf, Notify)> {
        let (path, listener) = self.listen()?;
        let clients: Arc<Mutex<Vec<mpsc::UnboundedSender<String>>>> = Arc::default();
        let (name, stop, accepted) = (name.to_owned(), self.stop.clone(), clients.clone());
        self.spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                let (tx, rx) = mpsc::unbounded_channel();
                accepted.lock().unwrap().push(tx.clone());
                let (name, endpoint, stop) = (name.clone(), endpoint.clone(), stop.clone());
                tokio::spawn(async move {
                    tokio::select! {
                        _ = stop.cancelled() => {}
                        _ = serve_connection(conn, name, endpoint, tx, rx) => {}
                    }
                });
            }
        });
        let notify: Notify = Arc::new(move |message| {
            let line = message.to_string();
            clients.lock().unwrap().retain(|c| c.send(line.clone()).is_ok());
        });
        Ok((path, notify))
    }

    /// Serves the project for the remote mount: the SFTP server and the callbacks of the hooks in
    /// the project's and the user's settings, which the workspace's Bash tool gets too, and watches
    /// the files FileChanged hooks name. Returns the SFTP socket's remote path, and the callbacks,
    /// through which the user's settings go too.
    pub fn serve_workspace(&self, forwards: &mut Forwards, ws: &Arc<Workspace>, nc: &str) -> anyhow::Result<(String, Arc<Callbacks>)> {
        let callbacks = Arc::new(Callbacks::new(&ws.root, nc));
        callbacks.set_socket(forwards.add(&self.serve_callbacks(callbacks.clone())?));
        let sftp = forwards.add(&self.serve_sftp(&ws.root, callbacks.clone())?);
        // What claude reads of the project at start: the root's names (`.git`, `.mcp.json`, …) and
        // Claude Code's own folder.
        if let Ok(mut project) = sftp::Project::new(&ws.root, Some(callbacks.clone()), crate::settings::respects_gitignore(&ws.root)) {
            let plan = warm::Plan { list: &["/"], tree: &["/.claude"], files: vec!["/.mcp.json".into()] };
            forwards.file("project.warm", warm::snapshot(&mut project, &plan, None));
        }
        ws.set_hooks(callbacks.clone());
        let (watcher, ws) = (callbacks.clone(), ws.clone());
        self.spawn(async move { watcher.watch(&ws).await });
        Ok((sftp, callbacks))
    }

    /// Serves the user's skills, agents, commands and output styles over SFTP, read-only, for the
    /// remote machine to mount; returns the socket's remote path, or None when there are none.
    pub fn serve_user_content(&self, forwards: &mut Forwards) -> anyhow::Result<Option<String>> {
        let dir = crate::settings::user_dir();
        let Some(mut content) = sftp::UserContent::new(&dir) else { return Ok(None) };
        forwards.file("user.warm", warm::snapshot(&mut content, &warm::Plan { tree: &["/"], ..Default::default() }, None));
        let (path, listener) = self.listen()?;
        self.spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                if let Some(content) = sftp::UserContent::new(&dir) {
                    russh_sftp::server::run(conn, content).await;
                }
            }
        });
        Ok(Some(forwards.add(&path)))
    }

    /// Serves the session's plugins over SFTP, read-only, for the remote machine to mount; returns
    /// the socket's remote path, or None when there are none.
    pub fn serve_plugins(&self, forwards: &mut Forwards, plugins: Vec<crate::plugins::Plugin>, callbacks: Arc<Callbacks>) -> anyhow::Result<Option<String>> {
        if plugins.is_empty() {
            return Ok(None);
        }
        let plugins = Arc::new(plugins);
        let mut view = sftp::Plugins::new(plugins.clone(), callbacks.clone());
        forwards.file("plugins.warm", warm::snapshot(&mut view, &warm::Plan { tree: &["/"], ..Default::default() }, None));
        let (path, listener) = self.listen()?;
        self.spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                russh_sftp::server::run(conn, sftp::Plugins::new(plugins.clone(), callbacks.clone())).await;
            }
        });
        Ok(Some(forwards.add(&path)))
    }

    /// Serves this machine's plugin store for the remote machine's `claude plugin …` (see
    /// [`crate::mount::store`]); returns the socket's remote path.
    pub fn serve_store(&self, forwards: &mut Forwards, stage: Arc<crate::mount::store::Stage>) -> anyhow::Result<String> {
        let (path, listener) = self.listen()?;
        self.spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                russh_sftp::server::run(conn, crate::mount::store::Store::new(stage.clone())).await;
            }
        });
        Ok(forwards.add(&path))
    }

    /// Serves this machine's user configuration for the remote claude's Claude Code folder (see
    /// [`crate::userconfig`]): `settings.json` and `keybindings.json`, sent with the run's first
    /// frames when this machine has them, and a socket the session's saves of them come back on, one
    /// per connection: `<name> <seq>` on the first line, then the file.
    pub fn serve_user_config(&self, forwards: &mut Forwards, config: Arc<crate::userconfig::UserConfig>) -> anyhow::Result<crate::userconfig::Delivery> {
        let settings = config.settings().map(|bytes| {
            trace("user-settings", &String::from_utf8_lossy(&bytes));
            forwards.file("settings.json", bytes)
        });
        let keybindings = config.keybindings().map(|bytes| forwards.file("keybindings.json", bytes));
        let (path, listener) = self.listen()?;
        let saves = config.clone();
        self.spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let saving = config.saving();
                tokio::spawn(async move {
                    let _saving = saving;
                    let mut buffer = Vec::new();
                    if conn.read_to_end(&mut buffer).await.is_err() {
                        return;
                    }
                    let Some(newline) = buffer.iter().position(|&b| b == b'\n') else { return };
                    let header = String::from_utf8_lossy(&buffer[..newline]).into_owned();
                    let Some((name, seq)) = header.split_once(' ').and_then(|(n, s)| Some((n.to_owned(), s.trim().parse::<u64>().ok()?))) else { return };
                    match _saving.0.save(&name, seq, &buffer[newline + 1..]) {
                        Err(e) => crate::log::warn(&format!("could not save the session's {name} to your ~/.claude: {e}")),
                        Ok(()) => trace("user-config", &header),
                    }
                });
            }
        });
        Ok(crate::userconfig::Delivery { settings, keybindings, sink: forwards.add(&path), saves })
    }

    /// Serves this project's sessions folder over SFTP for the remote machine to mount as its own;
    /// returns the socket's remote path. `args`: the run's arguments, which name the transcript it
    /// resumes, if any.
    pub fn serve_sessions(&self, forwards: &mut Forwards, cwd: &std::path::Path, args: &[String]) -> anyhow::Result<String> {
        let root = transcripts::projects_dir().join(transcripts::project_key(cwd));
        let paths = Arc::new(workdir::PathMap::new(cwd));
        let mut sessions = sftp::Sessions::new(&root, paths.clone())?;
        // The folder's names (a new transcript is known not to be there yet), the auto memory, and a
        // transcript being resumed, whole, which the remote end then keeps (see [`crate::helper::near`]).
        let plan = warm::Plan { list: &["/"], tree: &["/memory"], files: transcripts::resumed(args, &root) };
        forwards.file("sessions.warm", warm::snapshot(&mut sessions, &plan, Some(&workdir::PathMap::parts(cwd))));
        let (path, listener) = self.listen()?;
        self.spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                match sftp::Sessions::new(&root, paths.clone()) {
                    Ok(sessions) => russh_sftp::server::run(conn, sessions).await,
                    Err(e) => crate::log::warn(&format!("cannot serve {}: {e}", root.display())),
                }
            }
        });
        Ok(forwards.add(&path))
    }

    /// Serves the project over SFTP, for the remote machine to mount; each connection gets its own
    /// server. `settings` adapts the project's settings files to the remote claude. What git ignores
    /// is left out, as Claude Code's file picker leaves it out, unless `respectGitignore` is off.
    pub fn serve_sftp(&self, root: &std::path::Path, settings: Arc<Callbacks>) -> anyhow::Result<PathBuf> {
        let (path, listener) = self.listen()?;
        let root = root.to_path_buf();
        let respect_gitignore = crate::settings::respects_gitignore(&root);
        self.spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                match sftp::Project::new(&root, Some(settings.clone()), respect_gitignore) {
                    Ok(project) => russh_sftp::server::run(conn, project).await,
                    Err(e) => crate::log::warn(&format!("cannot serve {}: {e}", root.display())),
                }
            }
        });
        Ok(path)
    }

    /// Serves the callbacks of the project's hooks and status line: the callback's index on the
    /// first line, then the hook's JSON input; the answer goes back before the connection closes.
    pub fn serve_callbacks(&self, callbacks: Arc<Callbacks>) -> anyhow::Result<PathBuf> {
        let (path, listener) = self.listen()?;
        self.spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let callbacks = callbacks.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (index, input) = loop {
                        let complete = match conn.read(&mut chunk).await {
                            Ok(0) | Err(_) => true,
                            Ok(n) => {
                                buffer.extend_from_slice(&chunk[..n]);
                                false
                            }
                        };
                        let Some(newline) = buffer.iter().position(|&b| b == b'\n') else {
                            if complete {
                                return;
                            }
                            continue;
                        };
                        let input = &buffer[newline + 1..];
                        // The input is one JSON document; it may come without a newline or EOF.
                        if complete || serde_json::from_slice::<Value>(input).is_ok() {
                            let index = String::from_utf8_lossy(&buffer[..newline]).trim().parse::<usize>();
                            break (index, input.to_vec());
                        }
                    };
                    let Ok(index) = index else { return };
                    let output = callbacks.run(index, &input).await;
                    let _ = conn.write_all(&output).await;
                    let _ = conn.shutdown().await;
                });
            }
        });
        Ok(path)
    }

    /// Serves a Claude Code command hook: the hook's JSON input arrives on the connection, possibly
    /// without a newline, and the answer goes back before the connection closes.
    pub fn serve_hook(&self, answer: HookAnswer) -> anyhow::Result<PathBuf> {
        let (path, listener) = self.listen()?;
        self.spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let answer = answer.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let output = loop {
                        match conn.read(&mut chunk).await {
                            Ok(0) | Err(_) => {
                                break match serde_json::from_slice::<Value>(&buffer) {
                                    Ok(Value::Object(input)) => answer(&input),
                                    _ => Value::Object(Object::new()),
                                };
                            }
                            Ok(n) => {
                                buffer.extend_from_slice(&chunk[..n]);
                                // Not all of it yet, unless it parses.
                                match serde_json::from_slice::<Value>(&buffer) {
                                    Ok(Value::Object(input)) => break answer(&input),
                                    Ok(_) => break Value::Object(Object::new()),
                                    Err(_) => {}
                                }
                            }
                        }
                    };
                    let _ = conn.write_all(output.to_string().as_bytes()).await;
                    let _ = conn.shutdown().await;
                });
            }
        });
        Ok(path)
    }

    pub fn close(&self) {
        self.stop.cancel();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn serve_connection(conn: UnixStream, name: String, endpoint: Endpoint, tx: mpsc::UnboundedSender<String>, mut rx: mpsc::UnboundedReceiver<String>) {
    let (read, mut write) = conn.into_split();
    let writer = async move {
        while let Some(line) = rx.recv().await {
            if write.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
        }
    };
    let calls: Arc<Mutex<HashMap<String, CancellationToken>>> = Arc::default();
    let reader = {
        let calls = calls.clone();
        async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                trace(&format!("mcp-in:{name}"), &line);
                let Some(message) = parse_object(&line) else { continue };
                if str_field(&message, "method") == Some("notifications/cancelled")
                    && let Some(target) = message.get("params").and_then(|p| p.get("requestId"))
                    && let Some(call) = calls.lock().unwrap().get(&id_key(target))
                {
                    call.cancel();
                }
                let key = rpc_id(&message).map(|id| id_key(&id));
                let cancel = CancellationToken::new();
                if let Some(key) = &key {
                    calls.lock().unwrap().insert(key.clone(), cancel.clone());
                }
                let (endpoint, tx, calls, name) = (endpoint.clone(), tx.clone(), calls.clone(), name.clone());
                tokio::spawn(async move {
                    let response = endpoint.handle(message, cancel).await;
                    if let Some(key) = key {
                        calls.lock().unwrap().remove(&key);
                    }
                    if let Some(response) = response {
                        let out = response.to_string();
                        trace(&format!("mcp-out:{name}"), &out);
                        let _ = tx.send(out);
                    }
                });
            }
        }
    };
    tokio::select! {
        _ = writer => {}
        _ = reader => {}
    }
    for call in calls.lock().unwrap().values() {
        call.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Writes `input`, half-closes like `nc -U` at the end of its stdin when asked, and reads the reply.
    async fn exchange(path: &std::path::Path, input: &[u8], half_close: bool) -> String {
        let mut conn = UnixStream::connect(path).await.unwrap();
        conn.write_all(input).await.unwrap();
        if half_close {
            conn.shutdown().await.unwrap();
        }
        let mut out = String::new();
        conn.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn a_hook_gets_its_answer_whether_or_not_its_input_ends_the_stream() {
        let hub = SocketHub::new().unwrap();
        let path = hub.serve_hook(Arc::new(|input: &Object| json!({ "seen": input.get("tool_name") }))).unwrap();
        assert_eq!(exchange(&path, br#"{"tool_name":"mcp__cc__Edit"}"#, false).await, r#"{"seen":"mcp__cc__Edit"}"#);
        assert_eq!(exchange(&path, br#"{"tool_name":"#, true).await, "{}");
        hub.close();
    }

    #[tokio::test]
    async fn a_settings_callback_runs_with_the_input_that_follows_its_index() {
        let dir = std::env::temp_dir().join(format!("ct-cb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let callbacks = Arc::new(Callbacks::new(&dir, "nc"));
        callbacks.view(json!({ "statusLine": { "type": "command", "command": "cat" } }).to_string().as_bytes());
        let hub = SocketHub::new().unwrap();
        let path = hub.serve_callbacks(callbacks).unwrap();
        assert_eq!(exchange(&path, b"0\n{\"model\":\"x\"}", false).await, r#"{"model":"x"}"#);
        hub.close();
    }

    #[tokio::test]
    async fn an_mcp_endpoint_answers_json_rpc_lines_and_notifications_get_no_reply() {
        let hub = SocketHub::new().unwrap();
        let ws = Arc::new(crate::tools::Workspace::new(std::env::temp_dir(), vec![], Default::default()));
        let (path, _) = hub.serve_mcp("cc", Endpoint::Local(Arc::new(crate::mcp::local::LocalTools::new(ws, &["Read"])))).unwrap();
        let conn = UnixStream::connect(&path).await.unwrap();
        let (read, mut write) = conn.into_split();
        write.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n").await.unwrap();
        let mut lines = BufReader::new(read).lines();
        let ping: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ping, json!({ "jsonrpc": "2.0", "id": 1, "result": {} }));
        write.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n").await.unwrap();
        let list: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect::<Vec<_>>(), ["Read"]);
        hub.close();
    }
}
