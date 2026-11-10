//! The remote side of a run: the bridge ssh runs, and the holder that owns the session.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use super::control_path;
use crate::link::{Frame, GONE, Link, PROTOCOL, READY, STDERR, STDIO, Start, read_frame};

/// Data for a channel's writer; `None` = the other side finished writing.
type Queue = mpsc::UnboundedSender<Option<Vec<u8>>>;

fn log_path(run: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/claudette-{run}.holder.log"))
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Bridges ssh's stdio to the run's holder, starting the holder first with `spawn`.
pub async fn connect(run: &str, spawn: bool) -> i32 {
    let path = control_path(run);
    let mut conn = UnixStream::connect(&path).await;
    if conn.is_err() && spawn {
        if let Err(e) = spawn_holder(run) {
            eprintln!("claudette: cannot start the session holder: {e}");
            return 1;
        }
        for _ in 0..200 {
            conn = UnixStream::connect(&path).await;
            if conn.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let Ok(conn) = conn else {
        println!("{GONE}");
        return 3;
    };
    let mut stdout = tokio::io::stdout();
    if stdout.write_all(format!("{READY}\n").as_bytes()).await.is_err() || stdout.flush().await.is_err() {
        return 1;
    }
    let (mut from_holder, mut to_holder) = conn.into_split();
    let mut stdin = tokio::io::stdin();
    // Whichever side ends first ends the bridge; the holder carries on without it.
    tokio::select! {
        _ = tokio::io::copy(&mut stdin, &mut to_holder) => {}
        _ = async {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match from_holder.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout.write_all(&buf[..n]).await.is_err() || stdout.flush().await.is_err() {
                            break;
                        }
                    }
                }
            }
        } => {}
    }
    0
}

/// Starts the holder as its own session, so ssh's hangup never reaches it.
fn spawn_holder(run: &str) -> std::io::Result<()> {
    let log = std::fs::File::options().create(true).append(true).open(log_path(run))?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.args(["__helper", "hold", run]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    // SAFETY: setsid is async-signal-safe and touches nothing in the parent.
    unsafe {
        command.pre_exec(|| nix::unistd::setsid().map(|_| ()).map_err(std::io::Error::from));
    }
    command.spawn().map(|_| ())
}

/// The session being held.
struct Holder {
    run: String,
    link: Link,
    started: AtomicBool,
    /// Writers of open channels, by channel.
    chans: Mutex<HashMap<u32, Queue>>,
    next_chan: AtomicU32,
    /// The session's process group, once started.
    group: AtomicU32,
    /// The pty's controlling side, for resizing.
    pty: Mutex<Option<Arc<OwnedFd>>>,
    /// Sockets and run files made by this holder, removed at the end.
    sockets: Mutex<Vec<PathBuf>>,
    files: Mutex<Vec<PathBuf>>,
    /// Bumped per connection, so only the latest one marks the holder detached.
    generation: AtomicU64,
    connected: AtomicBool,
    /// Set when the session has ended (its exit code sent) or the local machine said goodbye.
    ended: tokio::sync::Notify,
    bye: AtomicBool,
    exited: AtomicBool,
    detach_timeout: AtomicU64,
    /// The inbox relay claude started (`__helper inbox`), once connected.
    relay: tokio::sync::Mutex<Option<tokio::net::unix::OwnedWriteHalf>>,
}

/// How long a holder whose session has ended waits for a detached local machine to collect the exit code.
const EXITED_DETACH_TIMEOUT: u64 = 600;

/// The holder: serves the run until the session ends and the local machine has heard, the local machine says
/// goodbye, or the local machine stays away longer than the detach timeout.
pub async fn hold(run: &str) -> i32 {
    // Everything the holder makes is for this user only.
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let path = control_path(run);
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("claudette: cannot listen on {}: {e}", path.display());
            return 1;
        }
    };
    let holder = Arc::new(Holder {
        run: run.to_owned(),
        link: Link::default(),
        started: AtomicBool::new(false),
        chans: Mutex::new(HashMap::new()),
        next_chan: AtomicU32::new(2),
        group: AtomicU32::new(0),
        pty: Mutex::new(None),
        sockets: Mutex::new(vec![path.clone()]),
        files: Mutex::new(vec![]),
        generation: AtomicU64::new(0),
        connected: AtomicBool::new(false),
        ended: tokio::sync::Notify::new(),
        bye: AtomicBool::new(false),
        exited: AtomicBool::new(false),
        // Until the start says otherwise: a holder nobody starts goes away soon.
        detach_timeout: AtomicU64::new(60),
        relay: tokio::sync::Mutex::new(None),
    });
    let mut current: Option<tokio::task::JoinHandle<()>> = None;
    let mut detached_since = Some(tokio::time::Instant::now());
    loop {
        let timeout = Duration::from_secs(holder.detach_timeout.load(Ordering::SeqCst));
        let deadline = detached_since.map(|since| since + timeout);
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((conn, _)) = accepted else { continue };
                // A reconnect can arrive before the old connection is noticed gone: the new one wins.
                if let Some(old) = current.take() {
                    old.abort();
                    holder.link.detach().await;
                }
                let generation = holder.generation.fetch_add(1, Ordering::SeqCst) + 1;
                holder.connected.store(true, Ordering::SeqCst);
                detached_since = None;
                let h = holder.clone();
                current = Some(tokio::spawn(async move { h.serve(conn, generation).await }));
            }
            _ = holder.ended.notified() => {
                if holder.bye.load(Ordering::SeqCst) {
                    break;
                }
                if !holder.connected.load(Ordering::SeqCst) && detached_since.is_none() {
                    detached_since = Some(tokio::time::Instant::now());
                }
            }
            _ = async { match deadline { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending().await } } => {
                eprintln!("claudette: nobody reconnected within {}s; ending the session", timeout.as_secs());
                break;
            }
        }
    }
    holder.stop().await;
    0
}

impl Holder {
    /// One connection from claudette, until it ends.
    async fn serve(self: Arc<Self>, conn: UnixStream, generation: u64) {
        let (mut read, mut write) = conn.into_split();
        let peer = match read_frame(&mut read).await {
            Ok((_, Frame::Hello(peer))) => peer,
            _ => return self.disconnected(generation),
        };
        if peer.get("protocol").and_then(|p| p.as_u64()) != Some(u64::from(PROTOCOL)) {
            eprintln!("claudette: a different helper protocol connected");
            return self.disconnected(generation);
        }
        let hello = Frame::Hello(json!({ "protocol": PROTOCOL, "received": self.link.received(), "fresh": !self.started.load(Ordering::SeqCst) }));
        if write.write_all(&hello.encode(0)).await.is_err() {
            return self.disconnected(generation);
        }
        let peer_received = peer.get("received").and_then(|r| r.as_u64()).unwrap_or(0);
        if self.link.attach(Box::new(write), peer_received).await.is_err() {
            return self.disconnected(generation);
        }
        let this = self.clone();
        let result = self.link.receive(read, move |frame| {
            let this = this.clone();
            async move { this.handle(frame).await }
        });
        let _ = result.await;
        self.link.detach().await;
        self.disconnected(generation);
    }

    fn disconnected(&self, generation: u64) {
        if self.generation.load(Ordering::SeqCst) == generation {
            self.connected.store(false, Ordering::SeqCst);
            self.ended.notify_one();
        }
    }

    async fn handle(self: &Arc<Self>, frame: Frame) {
        match frame {
            Frame::File { name, data } if !self.started.load(Ordering::SeqCst) => {
                if !valid_file_name(&name) {
                    self.link.send(&Frame::Notice(format!("invalid run file name: {name}"))).await;
                } else {
                    let path = PathBuf::from(format!("/tmp/claudette-{}.{}", self.run, name));
                    if let Err(e) = tokio::fs::write(&path, data).await {
                        self.link.send(&Frame::Notice(format!("could not write run file: {e}"))).await;
                    } else {
                        let mut files = self.files.lock().unwrap();
                        if !files.contains(&path) {
                            files.push(path);
                        }
                    }
                }
            }
            Frame::Start(v) => {
                if !self.started.swap(true, Ordering::SeqCst) {
                    match Start::from_json(&v) {
                        Some(start) => {
                            if let Err(e) = self.clone().start(start).await {
                                self.link.send(&Frame::Notice(format!("could not start the session: {e}"))).await;
                                self.link.send(&Frame::Exit(1)).await;
                            }
                        }
                        None => self.link.send(&Frame::Exit(1)).await,
                    }
                }
            }
            Frame::Data(chan, bytes) => {
                if let Some(queue) = self.chans.lock().unwrap().get(&chan) {
                    let _ = queue.send(Some(bytes));
                }
            }
            Frame::Close(chan) => {
                if let Some(queue) = self.chans.lock().unwrap().get(&chan) {
                    let _ = queue.send(None);
                }
            }
            Frame::Resize { rows, cols } => {
                if let Some(fd) = self.pty.lock().unwrap().as_ref() {
                    set_size(fd, rows, cols);
                }
            }
            Frame::Signal(n) => self.signal(n),
            Frame::Bye => {
                self.bye.store(true, Ordering::SeqCst);
                self.ended.notify_one();
            }
            Frame::Deliver(line) => {
                let this = self.clone();
                tokio::spawn(async move { this.deliver(line).await });
            }
            _ => {}
        }
    }

    fn signal(&self, n: i32) {
        let group = self.group.load(Ordering::SeqCst);
        if group != 0
            && let Ok(signal) = nix::sys::signal::Signal::try_from(n)
        {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group as i32), signal);
        }
    }

    /// Serves the sockets and runs the session.
    async fn start(self: Arc<Self>, start: Start) -> std::io::Result<()> {
        self.detach_timeout.store(start.detach_timeout.max(1), Ordering::SeqCst);
        // The inbox relay connects here once claude has started it.
        let relay = super::relay_path(&self.run);
        let _ = std::fs::remove_file(&relay);
        let relay_listener = UnixListener::bind(&relay)?;
        self.sockets.lock().unwrap().push(relay);
        let this = self.clone();
        tokio::spawn(async move {
            while let Ok((conn, _)) = relay_listener.accept().await {
                let (_, write) = conn.into_split();
                *this.relay.lock().await = Some(write);
            }
        });
        for (index, path) in start.sockets.iter().enumerate() {
            let path = PathBuf::from(path);
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)?;
            self.sockets.lock().unwrap().push(path);
            let this = self.clone();
            tokio::spawn(async move {
                while let Ok((conn, _)) = listener.accept().await {
                    this.clone().open(conn, index as u32).await;
                }
            });
        }
        if let Some((rows, cols)) = start.pty { self.run_pty(&start, rows, cols) } else { self.run_pipes(&start) }
    }

    /// A connection on one of the sockets: a new channel to the local machine.
    async fn open(self: Arc<Self>, conn: UnixStream, index: u32) {
        let chan = self.next_chan.fetch_add(1, Ordering::SeqCst);
        let (mut read, mut write) = conn.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
        self.chans.lock().unwrap().insert(chan, tx);
        self.link.send(&Frame::Open { chan, index }).await;
        tokio::spawn(async move {
            while let Some(Some(bytes)) = rx.recv().await {
                if write.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = write.shutdown().await;
        });
        let this = self.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match read.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => this.link.send(&Frame::Data(chan, buf[..n].to_vec())).await,
                }
            }
            this.link.send(&Frame::Close(chan)).await;
        });
    }

    /// The session on pipes: claude's stdio is channel 0, its stderr channel 1.
    fn run_pipes(self: Arc<Self>, start: &Start) -> std::io::Result<()> {
        let mut child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&start.command)
            .envs(start.env.iter().filter_map(|(k, v)| Some((k, v.as_str()?))))
            .envs(helper_env())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()?;
        self.group.store(child.id().unwrap_or(0), Ordering::SeqCst);
        let mut stdin = child.stdin.take().expect("piped");
        let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
        self.chans.lock().unwrap().insert(STDIO, tx);
        tokio::spawn(async move {
            while let Some(Some(bytes)) = rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
        let out = self.clone().pump(child.stdout.take().expect("piped"), STDIO);
        let err = self.clone().pump(child.stderr.take().expect("piped"), STDERR);
        tokio::spawn(async move {
            let status = child.wait().await;
            let code = status.map(exit_code).unwrap_or(1);
            self.finish(code, vec![out, err]).await;
        });
        Ok(())
    }

    /// Sends what a stream gives on a channel, then closes it.
    fn pump(self: Arc<Self>, mut stream: impl tokio::io::AsyncRead + Unpin + Send + 'static, chan: u32) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => self.link.send(&Frame::Data(chan, buf[..n].to_vec())).await,
                }
            }
            self.link.send(&Frame::Close(chan)).await;
        })
    }

    /// The session in a terminal, for claude's interface: the terminal is channel 0.
    fn run_pty(self: Arc<Self>, start: &Start, rows: u16, cols: u16) -> std::io::Result<()> {
        let size = nix::pty::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        let pty = nix::pty::openpty(Some(&size), None).map_err(std::io::Error::from)?;
        let master = Arc::new(pty.master);
        let slave = pty.slave;
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&start.command)
            .envs(start.env.iter().filter_map(|(k, v)| Some((k, v.as_str()?))))
            .envs(helper_env())
            .stdin(std::process::Stdio::from(slave.try_clone()?))
            .stdout(std::process::Stdio::from(slave.try_clone()?))
            .stderr(std::process::Stdio::from(slave));
        // SAFETY: only async-signal-safe calls: a new session, with the terminal as its controlling one.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::from)?;
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        self.group.store(child.id(), Ordering::SeqCst);
        *self.pty.lock().unwrap() = Some(master.clone());

        // A terminal has no async reads here: a thread each way.
        let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
        self.chans.lock().unwrap().insert(STDIO, tx);
        let mut writer = std::fs::File::from(master.try_clone()?);
        let (wtx, wrx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            use std::io::Write;
            while let Ok(bytes) = wrx.recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(Some(bytes)) = rx.recv().await {
                if wtx.send(bytes).is_err() {
                    break;
                }
            }
        });
        let mut reader = std::fs::File::from(master.try_clone()?);
        let (otx, mut orx) = mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if otx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let this = self.clone();
        let out = tokio::spawn(async move {
            while let Some(bytes) = orx.recv().await {
                this.link.send(&Frame::Data(STDIO, bytes)).await;
            }
        });
        tokio::spawn(async move {
            let status = tokio::task::spawn_blocking(move || child.wait()).await;
            let code = match status {
                Ok(Ok(status)) => exit_code(status),
                _ => 1,
            };
            // The terminal stays open while the holder has it; what the session wrote last is read
            // by now, give or take, so stop reading.
            tokio::time::sleep(Duration::from_millis(200)).await;
            out.abort();
            self.finish(code, vec![]).await;
        });
        Ok(())
    }

    /// The session ended: what it wrote goes first, then its exit code.
    async fn finish(&self, code: i32, readers: Vec<tokio::task::JoinHandle<()>>) {
        // Something the session left running may hold its output open: wait only so long.
        let _ = tokio::time::timeout(Duration::from_secs(3), futures_util::future::join_all(readers)).await;
        self.exited.store(true, Ordering::SeqCst);
        self.link.send(&Frame::Exit(code)).await;
        // Only the exit code and the last output wait for the local machine now: no need to hold them long.
        self.detach_timeout.fetch_min(EXITED_DETACH_TIMEOUT, Ordering::SeqCst);
        self.ended.notify_one();
    }

    /// Ends the session if it still runs, and removes what the holder made.
    async fn stop(&self) {
        if !self.exited.load(Ordering::SeqCst) && self.group.load(Ordering::SeqCst) != 0 {
            self.signal(libc::SIGTERM);
            for _ in 0..50 {
                if self.exited.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !self.exited.load(Ordering::SeqCst) {
                self.signal(libc::SIGKILL);
            }
        }
        for path in self.sockets.lock().unwrap().iter() {
            let _ = std::fs::remove_file(path);
        }
        for path in self.files.lock().unwrap().iter() {
            let _ = std::fs::remove_file(path);
            // A near end's warm start, held while it ran: removed by the session's end, unless the
            // session was killed.
            if path.extension().is_some_and(|e| e == "warm") {
                let mut held = path.clone().into_os_string();
                held.push(".held");
                let _ = std::fs::remove_file(held);
            }
        }
        let _ = std::fs::remove_file(log_path(&self.run));
    }

    /// Puts a line into claude's inbox. Through the inbox relay when claude has started it: a process
    /// of claude's own, whose messages claude takes as from itself in any permission mode. Until then
    /// (early in a session), straight into the inbox socket, which a session that skips permission
    /// prompts holds for approval. The inbox may not be up yet: retried for half a minute.
    async fn deliver(&self, line: Vec<u8>) {
        let dir = inbox_dir(&self.run);
        let mut last_error = String::from("claude's inbox never appeared");
        for attempt in 0..60 {
            {
                let mut relay = self.relay.lock().await;
                if let Some(writer) = relay.as_mut() {
                    let mut payload = line.clone();
                    payload.push(b'\n');
                    if writer.write_all(&payload).await.is_ok() {
                        return;
                    }
                    *relay = None;
                }
            }
            // Give the relay a few seconds to come up before going around it.
            if attempt >= 6
                && let Some(socket) = find_socket(&dir)
            {
                match post(&socket, std::fs::read_to_string(dir.join(crate::settings::INBOX_TOKEN)).ok(), &line).await {
                    Ok(()) => return,
                    Err(e) => last_error = e.to_string(),
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.link.send(&Frame::Notice(format!("a message for this session was not delivered: {last_error}"))).await;
    }
}

/// The helper binary, for the session: claude starts the inbox relay with it.
fn helper_env() -> Vec<(&'static str, String)> {
    std::env::current_exe().map(|exe| vec![(super::HELPER_ENV, exe.to_string_lossy().into_owned())]).unwrap_or_default()
}

/// One line into claude's inbox socket, after claude's token when there is one. The inbox reads lines
/// as they come and never closes the connection itself, so it is closed a moment later.
async fn post(socket: &Path, token: Option<String>, line: &[u8]) -> std::io::Result<()> {
    let mut conn = UnixStream::connect(socket).await?;
    let mut payload = Vec::new();
    if let Some(token) = token.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()) {
        payload.extend_from_slice(json!({ "type": "auth", "token": token }).to_string().as_bytes());
        payload.push(b'\n');
    }
    payload.extend_from_slice(line);
    payload.push(b'\n');
    conn.write_all(&payload).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

/// `__helper mcp`: serves the local-tools handshake from a run file and forwards tool calls.
pub async fn mcp(socket: &str, manifest_file: &str) -> i32 {
    let Some(manifest) = std::fs::read_to_string(manifest_file).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.as_object().cloned()) else { return 1 };
    let Ok(conn) = UnixStream::connect(socket).await else { return 1 };
    let (read, mut write) = conn.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(format!("{line}\n").as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let socket_tx = out_tx.clone();
    let socket_reader = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if socket_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = stdin.next_line().await {
        let Some(Value::Object(message)) = serde_json::from_str::<Value>(&line).ok() else {
            if write.write_all(format!("{line}\n").as_bytes()).await.is_err() { break }
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let id = crate::mcp::rpc_id(&message);
        let response = match method {
            "initialize" => {
                let mut result = manifest.get("initializeResult").cloned().unwrap_or_else(|| json!({}));
                if let Some(result) = result.as_object_mut() {
                    result.insert("protocolVersion".into(), json!(crate::mcp::negotiated_version(message.get("params"))));
                }
                id.map(|id| crate::mcp::rpc_result(id, result))
            }
            "ping" => id.map(|id| crate::mcp::rpc_result(id, json!({}))),
            "tools/list" => id.map(|id| crate::mcp::rpc_result(id, manifest.get("toolsListResult").cloned().unwrap_or_else(|| json!({})))),
            // One way, so nothing waits; the laptop hears that claude has its tools.
            "notifications/initialized" => {
                if write.write_all(format!("{line}\n").as_bytes()).await.is_err() { break }
                None
            }
            "server/discover" => id.map(|id| crate::mcp::rpc_error(id, crate::mcp::METHOD_NOT_FOUND, "claudette's local tools have no method server/discover")),
            _ => {
                if write.write_all(format!("{line}\n").as_bytes()).await.is_err() { break }
                None
            }
        };
        if let Some(response) = response {
            if out_tx.send(response.to_string()).is_err() { break }
        }
    }
    let _ = write.shutdown().await;
    drop(out_tx);
    socket_reader.abort();
    let _ = writer.await;
    0
}

/// The key every hook event from the remote side carries: when it was sent, in nanoseconds of the
/// remote machine's clock. Events whose answer nobody waits for can reach the laptop out of order.
pub const SENT_AT: &str = "claudetteSentAt";

fn sent_now() -> Value {
    json!(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)))
}

/// A hook event with [`SENT_AT`] added; anything but a JSON object goes as it is.
fn stamped(input: Vec<u8>) -> Vec<u8> {
    match serde_json::from_slice::<Value>(&input) {
        Ok(Value::Object(mut event)) => {
            event.insert(SENT_AT.into(), sent_now());
            Value::Object(event).to_string().into_bytes()
        }
        _ => input,
    }
}

/// `__helper notify`: forwards hook input without waiting for a response.
pub async fn notify(socket: &str) -> i32 {
    let mut input = Vec::new();
    if tokio::io::stdin().read_to_end(&mut input).await.is_err() { return 1 }
    let Ok(mut conn) = UnixStream::connect(socket).await else { return 1 };
    if conn.write_all(&stamped(input)).await.is_err() { return 1 }
    if conn.shutdown().await.is_err() { return 1 }
    0
}

/// The key a PreToolUse event the remote side decided carries to the laptop, with the decision.
pub const DECIDED: &str = "claudetteDecision";

/// `__helper hook <rules> <socket>`: claudette's PreToolUse hook. A Bash call is decided here: the
/// decision rests on the session's Bash rules alone (`{"allow": […], "deny": […]}`), which never
/// change in a session without a host, so the laptop only hears of it. Any other call is the
/// laptop's to decide (its path rules resolve its own files): its answer is the hook's.
pub async fn hook(rules_file: &str, socket: &str) -> i32 {
    let mut input = Vec::new();
    if tokio::io::stdin().read_to_end(&mut input).await.is_err() {
        return 1;
    }
    let rules = std::fs::read(rules_file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    if let (Some(rules), Ok(Value::Object(mut event))) = (rules, serde_json::from_slice::<Value>(&input))
        && event.get("tool_name").and_then(Value::as_str).and_then(|n| n.strip_prefix(crate::names::PREFIX)) == Some("Bash")
    {
        let list = |key: &str| -> Vec<String> { rules.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default() };
        let command = event.get("tool_input").and_then(|i| i.get("command")).and_then(Value::as_str).unwrap_or("");
        let decision = crate::permissions::policy::bash_decision(command, &list("allow"), &list("deny"));
        let mut stdout = tokio::io::stdout();
        let _ = stdout.write_all(crate::permissions::policy::hook_output(decision.as_ref()).to_string().as_bytes()).await;
        let _ = stdout.flush().await;
        event.insert(DECIDED.into(), decision.map_or(Value::Null, |d| d.to_json()));
        event.insert(SENT_AT.into(), sent_now());
        if let Ok(mut conn) = UnixStream::connect(socket).await {
            let _ = conn.write_all(Value::Object(event).to_string().as_bytes()).await;
            let _ = conn.shutdown().await;
        }
        return 0;
    }
    let Ok(mut conn) = UnixStream::connect(socket).await else { return 1 };
    if conn.write_all(&stamped(input)).await.is_err() || conn.shutdown().await.is_err() {
        return 1;
    }
    let mut answer = Vec::new();
    if conn.read_to_end(&mut answer).await.is_err() {
        return 1;
    }
    let mut stdout = tokio::io::stdout();
    let _ = stdout.write_all(&answer).await;
    let _ = stdout.flush().await;
    0
}

/// `__helper inbox`: a stdio MCP server claude starts, with no tools. It exists to be claude's own
/// process: it takes lines from the run's holder and posts them into claude's inbox (found through the
/// environment claude gives its servers), so claude delivers them as from itself.
pub async fn inbox(run: &str) -> i32 {
    use tokio::io::AsyncBufReadExt;
    let relay = super::relay_path(run);
    let mut conn = None;
    for _ in 0..200 {
        if let Ok(c) = UnixStream::connect(&relay).await {
            conn = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let tmp = std::env::var_os("CLAUDE_CODE_TMPDIR").map(PathBuf::from);
    let socket = move || std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET").map(PathBuf::from).or_else(|| tmp.as_deref().and_then(find_socket));
    let token_dir = std::env::var_os("CLAUDE_CODE_TMPDIR").map(PathBuf::from);
    let token = move || {
        std::env::var("CLAUDE_CODE_MESSAGING_TOKEN")
            .ok()
            .or_else(|| token_dir.as_ref().and_then(|d| std::fs::read_to_string(d.join(crate::settings::INBOX_TOKEN)).ok()))
    };
    let relaying = async move {
        let Some(conn) = conn else { return std::future::pending::<()>().await };
        let mut lines = tokio::io::BufReader::new(conn).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(socket) = socket() {
                let token = token();
                tokio::spawn(async move {
                    let _ = post(&socket, token, line.as_bytes()).await;
                });
            }
        }
        std::future::pending::<()>().await
    };
    let serving = async {
        let mut stdout = tokio::io::stdout();
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(serde_json::Value::Object(message)) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            let Some(id) = message.get("id").cloned() else { continue };
            let result = match message.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => json!({
                    "protocolVersion": message.get("params").and_then(|p| p.get("protocolVersion")).cloned().unwrap_or(json!(crate::mcp::LATEST_PROTOCOL_VERSION)),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "claudette inbox", "version": crate::VERSION },
                }),
                Some("tools/list") => json!({ "tools": [] }),
                _ => json!({}),
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            if stdout.write_all(format!("{reply}\n").as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    };
    // claude closes the server's stdin when the session ends.
    tokio::select! {
        () = relaying => {}
        () = serving => {}
    }
    0
}

/// The run's inbox folder (`CLAUDE_CODE_TMPDIR`), in /tmp's real path as the session script makes it.
fn inbox_dir(run: &str) -> PathBuf {
    let tmp = std::fs::canonicalize("/tmp").unwrap_or_else(|_| PathBuf::from("/tmp"));
    tmp.join(format!("claudette-{run}.inbox"))
}

/// claude's inbox socket under its temporary folder (`cc-socks/<pid>.sock`).
fn find_socket(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    for sub in std::fs::read_dir(dir).ok()?.flatten() {
        if sub.file_type().is_ok_and(|t| t.is_dir()) {
            for entry in std::fs::read_dir(sub.path()).ok()?.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_socket()) {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

fn set_size(fd: &OwnedFd, rows: u16, cols: u16) {
    let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCSWINSZ reads one winsize from a valid pointer on an open terminal descriptor.
    unsafe {
        libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ as _, &size);
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}
