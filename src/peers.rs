//! Sessions on this machine reaching each other: what ListAgents and SendMessage do among a local
//! claude's sessions. The built-ins are off: on the remote machine they would reach every session
//! there, other machines' included.
//!
//! There is no registry. Each run's folder in this user's runtime folder holds `agent.sock`; a
//! listing asks every one that answers, so it is always current, and a folder whose process is gone
//! is removed. A message goes to the session's own claudette, which puts it into its claude's
//! inbox on the remote machine: a private folder only that run knows (`CLAUDE_CODE_TMPDIR`), so the
//! remote machine's own session list plays no part. claude takes it as a message from another
//! session, never as the user's prompt.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::json::{Object, str_field};

/// The session's socket in its run folder.
pub const SOCKET: &str = "agent.sock";
/// Run folders: `claudette-<pid>-XXXXXX`.
const RUN_PREFIX: &str = "claudette-";
/// How long a listing waits for one session to answer.
const ASK_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a message may take to reach the remote inbox.
const DELIVER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Working,
    Idle,
    Waiting,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Working => "working",
            Status::Idle => "idle",
            Status::Waiting => "waiting for input",
        }
    }
}

/// Puts one inbox line (a JSON object) into this session's claude; an error is shown to the sender.
pub type Deliver = Arc<dyn Fn(Value) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// This session, as the other sessions here see it.
pub struct Agent {
    /// Short and unique: tells sessions with the same name apart.
    pub id: String,
    pub name: String,
    pub project: PathBuf,
    started: u64,
    status: Mutex<Status>,
    /// Sockets of sessions waiting for this one to go idle or end.
    watchers: Mutex<Vec<PathBuf>>,
    socket: OnceLock<PathBuf>,
    deliver: Deliver,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A path under the home folder as `~/…`.
pub fn shown(path: &Path) -> String {
    match path.strip_prefix(crate::config::home()) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

impl Agent {
    /// `run` is the run's unique id; `name` defaults to the project folder's name.
    pub fn new(run: &str, name: Option<String>, project: &Path, deliver: Deliver) -> Arc<Agent> {
        let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
            project.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| "session".into())
        });
        Arc::new(Agent {
            id: run.chars().take(6).collect(),
            name,
            project: project.to_path_buf(),
            started: now(),
            status: Mutex::new(Status::Idle),
            watchers: Mutex::new(vec![]),
            socket: OnceLock::new(),
            deliver,
        })
    }

    /// `name [id]`: how sessions address each other.
    pub fn label(&self) -> String {
        format!("{} [{}]", self.name, self.id)
    }

    pub fn set_socket(&self, path: PathBuf) {
        let _ = self.socket.set(path);
    }

    pub fn socket(&self) -> Option<&PathBuf> {
        self.socket.get()
    }

    pub fn status(&self) -> Status {
        *self.status.lock().unwrap()
    }

    /// A new status; going idle tells the sessions waiting for it.
    pub fn set_status(self: &Arc<Self>, status: Status) {
        let was = std::mem::replace(&mut *self.status.lock().unwrap(), status);
        if status == Status::Idle && was != Status::Idle {
            let watchers = std::mem::take(&mut *self.watchers.lock().unwrap());
            let this = self.clone();
            tokio::spawn(async move { this.tell(watchers, "is idle: it finished its turn").await });
        }
    }

    /// The session is ending: the sessions waiting for it hear so.
    pub async fn end(&self) {
        let watchers = std::mem::take(&mut *self.watchers.lock().unwrap());
        self.tell(watchers, "has ended").await;
    }

    async fn tell(&self, watchers: Vec<PathBuf>, what: &str) {
        let text = format!("Session {} ({}) {what}.", self.label(), shown(&self.project));
        for socket in watchers {
            let _ = ask(&socket, &json!({ "op": "send", "from": self.label(), "project": shown(&self.project), "text": text })).await;
        }
    }

    fn info(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "project": self.project, "status": self.status().as_str(), "started": self.started })
    }

    /// A message from another session, into this session's claude.
    async fn receive(&self, from: &str, project: &str, text: &str) -> Result<(), String> {
        let content = format!("{text}\n\n(From session {from}, working in {project}. To reply, use SendMessage with to: \"{from}\".)");
        let line = json!({ "type": "user", "message": { "role": "user", "content": content }, "priority": "next", "from": from });
        tokio::time::timeout(DELIVER_TIMEOUT, (self.deliver)(line)).await.unwrap_or_else(|_| Err("timed out reaching that session".into()))
    }

    async fn handle(self: &Arc<Self>, request: &Object) -> Value {
        match str_field(request, "op") {
            Some("info") => self.info(),
            Some("send") => {
                let from = str_field(request, "from").unwrap_or("another session");
                let project = str_field(request, "project").unwrap_or("an unknown folder");
                if let Some(text) = str_field(request, "text").filter(|t| !t.is_empty())
                    && let Err(e) = self.receive(from, project, text).await
                {
                    return json!({ "ok": false, "error": e });
                }
                if request.get("notify").and_then(Value::as_bool) == Some(true)
                    && let Some(reply_to) = str_field(request, "reply_to").map(PathBuf::from)
                {
                    if self.status() == Status::Idle {
                        self.tell(vec![reply_to], "is idle").await;
                    } else {
                        self.watchers.lock().unwrap().push(reply_to);
                    }
                }
                json!({ "ok": true, "status": self.status().as_str() })
            }
            _ => json!({ "ok": false, "error": "unknown request" }),
        }
    }
}

static THIS: OnceLock<Arc<Agent>> = OnceLock::new();

/// This process's session; one per process.
pub fn register(agent: Arc<Agent>) {
    let _ = THIS.set(agent);
}

pub fn this() -> Option<&'static Arc<Agent>> {
    THIS.get()
}

pub fn set_status(status: Status) {
    if let Some(agent) = this() {
        agent.set_status(status);
    }
}

/// One request on a session's socket: a JSON line, answered by one.
pub async fn serve_connection(agent: Arc<Agent>, conn: UnixStream) {
    let (read, mut write) = conn.into_split();
    let mut line = String::new();
    let read = tokio::time::timeout(ASK_TIMEOUT, BufReader::new(read).read_line(&mut line)).await;
    let answer = match (read, serde_json::from_str::<Value>(&line)) {
        (Ok(Ok(_)), Ok(Value::Object(request))) => agent.handle(&request).await,
        _ => json!({ "ok": false, "error": "bad request" }),
    };
    let _ = write.write_all(format!("{answer}\n").as_bytes()).await;
    let _ = write.shutdown().await;
}

async fn ask(socket: &Path, request: &Value) -> Result<Value, String> {
    let exchange = async {
        let conn = UnixStream::connect(socket).await.map_err(|e| e.to_string())?;
        let (read, mut write) = conn.into_split();
        write.write_all(format!("{request}\n").as_bytes()).await.map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.map_err(|e| e.to_string())?;
        serde_json::from_str::<Value>(&line).map_err(|e| e.to_string())
    };
    let limit = if request.get("op").and_then(Value::as_str) == Some("info") { ASK_TIMEOUT } else { DELIVER_TIMEOUT + ASK_TIMEOUT };
    tokio::time::timeout(limit, exchange).await.unwrap_or_else(|_| Err("no answer".into()))
}

/// Another session here, as it described itself.
#[derive(Debug, Clone)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub project: PathBuf,
    pub status: String,
    pub started: u64,
    pub socket: PathBuf,
}

impl Peer {
    pub fn label(&self) -> String {
        format!("{} [{}]", self.name, self.id)
    }
}

fn process_alive(pid: i32) -> bool {
    !matches!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None), Err(nix::errno::Errno::ESRCH))
}

/// The other sessions with a run folder in `root`, except the one at `own`. A folder this user owns
/// whose process is gone is removed; anything else that does not answer is left alone.
pub async fn list(root: &Path, own: Option<&Path>) -> Vec<Peer> {
    use std::os::unix::fs::MetadataExt;
    let uid = nix::unistd::getuid().as_raw();
    let Ok(entries) = std::fs::read_dir(root) else { return vec![] };
    let mut asks = vec![];
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(RUN_PREFIX).and_then(|rest| rest.split('-').next()).and_then(|p| p.parse::<i32>().ok()) else { continue };
        let dir = entry.path();
        if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && m.uid() == uid) {
            continue;
        }
        let socket = dir.join(SOCKET);
        if own == Some(socket.as_path()) {
            continue;
        }
        asks.push(async move {
            match ask(&socket, &json!({ "op": "info" })).await {
                Ok(Value::Object(info)) => Some(Peer {
                    id: str_field(&info, "id").unwrap_or_default().to_owned(),
                    name: str_field(&info, "name").unwrap_or_default().to_owned(),
                    project: PathBuf::from(str_field(&info, "project").unwrap_or_default()),
                    status: str_field(&info, "status").unwrap_or_default().to_owned(),
                    started: info.get("started").and_then(Value::as_u64).unwrap_or(0),
                    socket,
                }),
                _ => {
                    if !process_alive(pid) {
                        let _ = std::fs::remove_dir_all(&dir);
                    }
                    None
                }
            }
        });
    }
    let mut peers: Vec<Peer> = futures_util::future::join_all(asks).await.into_iter().flatten().collect();
    peers.sort_by(|a, b| a.name.cmp(&b.name).then(a.started.cmp(&b.started)).then(a.id.cmp(&b.id)));
    peers
}

/// The peer `to` names: `name [id]`, the id, or a name only one session has.
pub fn resolve<'a>(peers: &'a [Peer], to: &str) -> Result<&'a Peer, String> {
    let to = to.trim().trim_start_matches('@');
    if let Some(peer) = peers.iter().find(|p| p.label() == to || p.id == to) {
        return Ok(peer);
    }
    let named: Vec<&Peer> = peers.iter().filter(|p| p.name == to).collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("No session named {to} on this machine.{}", listing_hint(peers))),
        many => Err(format!("More than one session is named {to}: {}. Use the one you mean, with its id.", many.iter().map(|p| format!("\"{}\"", p.label())).collect::<Vec<_>>().join(", "))),
    }
}

fn listing_hint(peers: &[Peer]) -> String {
    if peers.is_empty() {
        " There are no other sessions.".into()
    } else {
        format!(" Sessions: {}.", peers.iter().map(Peer::label).collect::<Vec<_>>().join(", "))
    }
}

/// Sends `text` from `from` to the session `to` names; with `notify`, also asks it for one notice
/// when it is next idle or ends. Returns the target's label and status.
pub async fn send(root: &Path, from: &Agent, to: &str, text: Option<&str>, notify: bool) -> Result<(String, String), String> {
    let peers = list(root, from.socket().map(PathBuf::as_path)).await;
    if to.trim().trim_start_matches('@') == from.label() || to.trim() == from.id {
        return Err("That is this session.".into());
    }
    let target = resolve(&peers, to)?;
    let mut request = json!({ "op": "send", "from": from.label(), "project": shown(&from.project), "notify": notify });
    if let Some(text) = text {
        request["text"] = json!(text);
    }
    if let Some(socket) = from.socket() {
        request["reply_to"] = json!(socket);
    }
    match ask(&target.socket, &request).await {
        Ok(answer) if answer.get("ok").and_then(Value::as_bool) == Some(true) => {
            Ok((target.label(), answer.get("status").and_then(Value::as_str).unwrap_or("").to_owned()))
        }
        Ok(answer) => Err(answer.get("error").and_then(Value::as_str).unwrap_or("not delivered").to_owned()),
        Err(e) => Err(format!("{} did not answer: {e}", target.label())),
    }
}

/// Delivery into the run's claude: the line goes to the run's holder on the remote machine through
/// the session's connection, queued while that connection is away, and the holder puts it into
/// claude's inbox there (see [`crate::helper`]).
pub fn queued(to_holder: tokio::sync::mpsc::UnboundedSender<Value>) -> Deliver {
    Arc::new(move |line: Value| {
        let sent = to_holder.send(line).map_err(|_| "that session has ended".to_owned());
        Box::pin(async move { sent })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        // Short: a socket path has room for about 100 bytes.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = PathBuf::from(format!("/tmp/cta-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A session serving its socket from `root/claudette-<pid>-<tag>`, recording what it is given.
    fn session(root: &Path, tag: &str, name: &str, project: &str) -> (Arc<Agent>, Arc<Mutex<Vec<Value>>>) {
        let got: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sink = got.clone();
        let deliver: Deliver = Arc::new(move |line| {
            sink.lock().unwrap().push(line);
            Box::pin(async { Ok(()) })
        });
        let agent = Agent::new(&format!("{tag}0000000000"), Some(name.into()), Path::new(project), deliver);
        let dir = root.join(format!("{RUN_PREFIX}{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SOCKET);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        agent.set_socket(path);
        let served = agent.clone();
        tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                tokio::spawn(serve_connection(served.clone(), conn));
            }
        });
        (agent, got)
    }

    fn content(line: &Value) -> String {
        line["message"]["content"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn sessions_find_each_other_by_asking_and_messages_arrive_marked_with_the_sender() {
        let root = root();
        let (api, _) = session(&root, "aaaaaa", "api", "/w/api");
        let (web, web_got) = session(&root, "bbbbbb", "web", "/w/web");
        let peers = list(&root, api.socket().map(PathBuf::as_path)).await;
        assert_eq!(peers.iter().map(Peer::label).collect::<Vec<_>>(), ["web [bbbbbb]"]);
        assert_eq!((peers[0].project.as_path(), peers[0].status.as_str()), (Path::new("/w/web"), "idle"));

        let (to, _) = send(&root, &api, "web", Some("schema changed"), false).await.unwrap();
        assert_eq!(to, "web [bbbbbb]");
        let line = web_got.lock().unwrap()[0].clone();
        assert_eq!((line["type"].as_str(), line["from"].as_str()), (Some("user"), Some("api [aaaaaa]")));
        assert!(content(&line).starts_with("schema changed\n\n(From session api [aaaaaa], working in /w/api."));
        assert_eq!(send(&root, &api, "api [aaaaaa]", Some("x"), false).await.unwrap_err(), "That is this session.");
        let _ = web;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn same_names_need_the_id_and_unknown_names_list_what_there_is() {
        let root = root();
        let (me, _) = session(&root, "aaaaaa", "me", "/w/me");
        let (_one, _) = session(&root, "bbbbbb", "app", "/w/app");
        let (_two, two_got) = session(&root, "cccccc", "app", "/w/app2");
        let err = send(&root, &me, "app", Some("hi"), false).await.unwrap_err();
        assert!(err.contains("\"app [bbbbbb]\", \"app [cccccc]\""), "{err}");
        send(&root, &me, "cccccc", Some("hi"), false).await.unwrap();
        assert_eq!(two_got.lock().unwrap().len(), 1);
        let err = send(&root, &me, "nope", Some("hi"), false).await.unwrap_err();
        assert!(err.contains("Sessions: app [bbbbbb], app [cccccc]."), "{err}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_watcher_hears_once_when_the_session_goes_idle() {
        let root = root();
        let (me, me_got) = session(&root, "aaaaaa", "me", "/w/me");
        let (busy, _) = session(&root, "bbbbbb", "busy", "/w/busy");
        busy.set_status(Status::Working);
        send(&root, &me, "busy", None, true).await.unwrap();
        assert!(me_got.lock().unwrap().is_empty());
        busy.set_status(Status::Idle);
        for _ in 0..100 {
            if !me_got.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(content(&me_got.lock().unwrap()[0]).starts_with("Session busy [bbbbbb] (/w/busy) is idle"));
        busy.set_status(Status::Working);
        busy.set_status(Status::Idle);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(me_got.lock().unwrap().len(), 1, "one notice per request");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_folder_whose_process_is_gone_is_removed_and_a_live_silent_one_is_kept() {
        let root = root();
        // A pid that no longer runs: a child that has exited and been reaped.
        let dead = std::process::Command::new("true").spawn().unwrap();
        let pid = dead.id();
        let _ = dead.wait_with_output();
        let gone = root.join(format!("{RUN_PREFIX}{pid}-XXXXXX"));
        let silent = root.join(format!("{RUN_PREFIX}{}-YYYYYY", std::process::id()));
        std::fs::create_dir_all(&gone).unwrap();
        std::fs::create_dir_all(&silent).unwrap();
        assert!(list(&root, None).await.is_empty());
        assert!(!gone.exists());
        assert!(silent.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
