//! A session's connection to its holder on the remote machine (see [`crate::helper`]): everything
//! claude and this machine say to each other, over one ssh connection that may come and go.
//!
//! - **Deploy.** ssh runs the helper from `~/.claudette/bin/<sha256>/claudette` on the remote
//!   machine. When that copy is missing, the start command says which platform the machine is, and
//!   this machine uploads the matching build (this executable, or the release's `claudette-<platform>`
//!   beside it), checks its sha256 there and renames it into place; the two newest builds are kept.
//! - **Channels.** claude's stdio (or its terminal) and every connection to the sockets claude reaches
//!   this machine through travel as channels of the helper's resumable stream.
//! - **Reconnecting.** When ssh ends without the session having ended, this side reconnects (a new
//!   connection, not the shared one, which may be dead) and both sides send again what the other
//!   missed. The session waits meanwhile; if the holder is gone, the session ends with an error.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::config::{Config, is_ssh_binary};
use crate::link::{Frame, GONE, Link, MISSING, PROTOCOL, READY, STDIO, Start, read_frame};
use crate::log::{trace, warn};
use crate::ssh::shell_quote;

/// What a file delivered before the session starts contains.
pub use crate::sockets::RunFile;

/// What the session runs and serves.
pub struct Spec {
    pub run: String,
    pub command: String,
    /// (remote path, local socket) for each socket claude reaches this machine through.
    pub sockets: Vec<(String, PathBuf)>,
    /// Files sent before the Start frame.
    pub files: Vec<RunFile>,
    pub pty: Option<(u16, u16)>,
    pub env: Map<String, Value>,
}

/// Output of claude's stdio channels: (channel, data); `None` data = that stream ended.
pub type Output = mpsc::UnboundedReceiver<(u32, Option<Vec<u8>>)>;

/// claude's stdio channel (its stderr is any other channel callers see).
pub const STDIO_CHANNEL: u32 = STDIO;

pub struct Tunnel {
    link: Arc<Link>,
    exit: watch::Receiver<Option<Result<i32, String>>>,
    done: Arc<AtomicBool>,
    /// Taken by [`Tunnel::finish`].
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Every platform the release carries a build for, named as it names them.
pub const PLATFORMS: [&str; 4] = ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"];

/// This machine's platform, named as the release names its builds.
pub fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", _) => "darwin-x64",
        ("linux", "aarch64") => "linux-arm64",
        _ => "linux-x64",
    }
}

/// A build of claudette for a platform, as the remote machine stores it.
struct Build {
    platform: String,
    /// Held open from the session's start: `+update` renames new builds over the old ones, and a
    /// session that copies its build to the remote machine later must copy the one it hashed.
    file: std::fs::File,
    /// The full sha256, hex; its first 16 digits name the folder on the remote machine.
    sha: String,
}

impl Build {
    fn open(platform: &str, path: &Path) -> std::io::Result<Build> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut chunk = vec![0; 1 << 16];
        loop {
            match file.read(&mut chunk)? {
                0 => break,
                n => hasher.update(&chunk[..n]),
            }
        }
        let sha = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok(Build { platform: platform.to_owned(), file, sha })
    }

    fn folder(&self) -> &str {
        &self.sha[..16]
    }

    /// The build's bytes as they were when it was opened.
    fn bytes(&self) -> std::io::Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut bytes = vec![0; usize::try_from(self.file.metadata()?.len()).unwrap_or(usize::MAX)];
        self.file.read_exact_at(&mut bytes, 0)?;
        Ok(bytes)
    }
}

/// The builds available here: this executable, and the release's other builds beside it.
fn builds() -> Vec<Build> {
    let Ok(exe) = std::env::current_exe().and_then(std::fs::canonicalize) else { return vec![] };
    let mut found = vec![(platform(), exe.clone())];
    if let Some(dir) = exe.parent() {
        for p in PLATFORMS {
            let path = dir.join(format!("claudette-{p}"));
            if p != platform() && path.is_file() {
                found.push((p, path));
            }
        }
    }
    found.into_iter().filter_map(|(platform, path)| Build::open(platform, &path).ok()).collect()
}

/// Runs the helper's bridge for `run`: with `spawn` (the first connection) it starts the holder, and
/// says which platform the machine is if the build for it is not there yet.
fn connect_script(builds: &[Build], run: &str, spawn: bool) -> String {
    helper_script(builds, &format!("connect {}{}", shell_quote(run), if spawn { " --spawn" } else { "" }))
}

/// Runs `__helper <command>` with the cached build for this machine, or prints [`MISSING`] and the
/// machine's platform.
fn helper_script(builds: &[Build], command: &str) -> String {
    let cases: String = builds.iter().map(|b| format!("{}) h={};; ", b.platform, b.folder())).collect();
    format!(
        r#"case "$(uname -sm)" in "Linux x86_64") p=linux-x64;; "Linux aarch64"|"Linux arm64") p=linux-arm64;; "Darwin arm64") p=darwin-arm64;; "Darwin x86_64") p=darwin-x64;; *) p="$(uname -sm | tr ' ' -)";; esac
h=; case $p in {cases}esac
B="$HOME/.claudette/bin/$h/claudette"
if [ -n "$h" ] && [ -x "$B" ]; then exec "$B" __helper {command}; fi
echo "{MISSING} $p""#
    )
}

/// For `claudette +doctor`: makes sure the helper for the remote machine is there (copying it as a session
/// would) and runs; returns the version it reports.
pub async fn check_helper(config: &Config) -> anyhow::Result<String> {
    let builds = builds();
    let mut uploaded = false;
    loop {
        let argv = crate::ssh::ssh_argv(config, &helper_script(&builds, "version"), false);
        let out = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).output().await?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let Some(remote) = text.strip_prefix(MISSING).map(str::trim) else {
            if !out.status.success() {
                bail!("{}{}", String::from_utf8_lossy(&out.stderr).trim(), crate::ssh::failure_hint(config, out.status));
            }
            return Ok(text);
        };
        if uploaded {
            bail!("copied to {} but it does not run there", config.host);
        }
        let build = builds
            .iter()
            .find(|b| b.platform == remote)
            .with_context(|| format!("no build for {remote}: put the release's claudette-{remote} next to this claudette"))?;
        upload(config, build).await?;
        uploaded = true;
    }
}

/// Uploads a build: into a temporary name, checked, then renamed; older builds but one are removed.
async fn upload(config: &Config, build: &Build) -> anyhow::Result<()> {
    let script = format!(
        r#"set -e; D="$HOME/.claudette/bin/{folder}"; mkdir -p "$D"; t="$D/.upload.$$"; cat >"$t"
s=$( (sha256sum "$t" 2>/dev/null || shasum -a 256 "$t") | cut -d' ' -f1)
if [ "$s" != {sha} ]; then rm -f "$t"; echo "the uploaded helper does not match its checksum" >&2; exit 1; fi
chmod 755 "$t"; mv "$t" "$D/claudette"
cd "$HOME/.claudette/bin" && ls -t | tail -n +3 | while read -r old; do rm -rf -- "$old"; done"#,
        folder = build.folder(),
        sha = build.sha,
    );
    let argv = crate::ssh::ssh_argv(config, &script, false);
    let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    let bytes = build.bytes()?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(&bytes).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("could not copy claudette to {}: {}", config.host, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// ssh for the bridge: the shared connection the first time, a fresh one when reconnecting, since a
/// shared connection that outlived a sleep may hang until its keepalives give up. A reconnect never
/// prompts: claude's interface holds the terminal, and the prompt would be lost in it.
fn bridge_argv(config: &Config, script: &str, fresh: bool) -> Vec<String> {
    let mut argv = vec![config.ssh[0].clone()];
    if fresh && is_ssh_binary(&config.ssh[0]) {
        argv.extend(["-o", "ControlPath=none"].iter().map(|s| (*s).to_owned()));
        argv.extend(crate::config::BATCH.iter().map(|s| (*s).to_owned()));
    }
    argv.extend(config.ssh[1..].iter().cloned());
    argv.extend(["-T".to_owned(), config.host.clone(), script.to_owned()]);
    argv
}

/// Channel writers for socket connections, by channel.
type Sockets = Arc<Mutex<std::collections::HashMap<u32, mpsc::UnboundedSender<Option<Vec<u8>>>>>>;

impl Tunnel {
    /// Starts the session on the remote machine. `deliveries` are lines for claude's inbox; output on
    /// claude's stdio channels arrives on the returned receiver.
    pub async fn start(config: &Config, spec: Spec, deliveries: mpsc::UnboundedReceiver<Value>) -> anyhow::Result<(Tunnel, Output)> {
        let builds = builds();
        if builds.is_empty() {
            bail!("cannot read this claudette executable to copy it to {}", config.host);
        }
        let link = Arc::new(Link::default());
        let start = Start { command: spec.command, sockets: spec.sockets.iter().map(|(r, _)| r.clone()).collect(), pty: spec.pty, env: spec.env, detach_timeout: config.detach_timeout };
        // Files precede Start so the holder has them before spawning the shell.
        for file in &spec.files {
            link.send(&Frame::File { name: file.name.clone(), data: file.data.clone() }).await;
        }
        link.send(&Frame::Start(start.to_json())).await;

        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = watch::channel(None);
        let done = Arc::new(AtomicBool::new(false));
        let locals: Arc<Vec<PathBuf>> = Arc::new(spec.sockets.into_iter().map(|(_, l)| l).collect());
        let sockets: Sockets = Arc::default();

        // The first connection is made here, so a run that cannot start fails before claude is awaited.
        let first = Connection::open(config, &builds, &spec.run, true, &link).await?;
        tokio::spawn({
            let link = link.clone();
            let mut deliveries = deliveries;
            async move {
                while let Some(line) = deliveries.recv().await {
                    link.send(&Frame::Deliver(line.to_string().into_bytes())).await;
                }
            }
        });
        let supervisor = tokio::spawn(supervise(Supervisor {
            config: config.clone(),
            builds,
            run: spec.run,
            link: link.clone(),
            out: out_tx,
            exit: exit_tx,
            done: done.clone(),
            locals,
            sockets,
            first: Some(first),
        }));
        Ok((Tunnel { link, exit: exit_rx, done, supervisor: Mutex::new(Some(supervisor)) }, out_rx))
    }

    pub async fn write(&self, bytes: Vec<u8>) {
        self.link.send(&Frame::Data(STDIO, bytes)).await;
    }

    pub async fn close_stdin(&self) {
        self.link.send(&Frame::Close(STDIO)).await;
    }

    pub async fn resize(&self, rows: u16, cols: u16) {
        self.link.send(&Frame::Resize { rows, cols }).await;
    }

    pub async fn signal(&self, signal: i32) {
        self.link.send(&Frame::Signal(signal)).await;
    }

    /// The session's exit code, once it has ended.
    pub async fn wait(&self) -> anyhow::Result<i32> {
        let mut exit = self.exit.clone();
        let result = exit.wait_for(Option::is_some).await.map_err(|_| anyhow::anyhow!("the connection to the session ended"))?.clone();
        result.expect("checked").map_err(anyhow::Error::msg)
    }

    /// Tells the holder this machine is done (ending the session if it still runs), then lets go.
    pub async fn finish(&self) {
        self.done.store(true, Ordering::SeqCst);
        self.link.send(&Frame::Bye).await;
        // Give the goodbye a moment to leave; the holder also ends on its own when nobody returns.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let supervisor = self.supervisor.lock().unwrap().take();
        if let Some(supervisor) = supervisor {
            supervisor.abort();
            // Once it is gone ssh is killed, which would otherwise hold the caller's stderr open
            // until the remote end closes, a round trip later.
            let _ = supervisor.await;
        }
    }

    /// Runs the session on this process's own stdio: in a terminal (raw, resized with it) when `tty`,
    /// otherwise stdin, stdout and stderr as streams. Returns the session's exit code.
    pub async fn attach_stdio(&self, mut output: Output, tty: bool) -> anyhow::Result<i32> {
        use tokio::signal::unix::{SignalKind, signal};
        // Dropped after the terminal is restored (declared first), so held warnings print below the session.
        let _held = tty.then(crate::log::hold_warnings);
        let _raw = tty.then(RawMode::enter).flatten();
        let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
        // Reading a terminal blocks: a thread, which ends with the process.
        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = in_tx.send(None);
                        break;
                    }
                    Ok(n) => {
                        if in_tx.send(Some(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let (mut winch, mut int, mut term, mut hup) =
            (signal(SignalKind::window_change())?, signal(SignalKind::interrupt())?, signal(SignalKind::terminate())?, signal(SignalKind::hangup())?);
        let mut stdin_open = true;
        let code = loop {
            tokio::select! {
                item = output.recv() => match item {
                    Some((chan, Some(bytes))) => write_out(chan, &bytes),
                    Some((_, None)) => {}
                    None => break self.wait().await?,
                },
                input = in_rx.recv(), if stdin_open => match input {
                    Some(Some(bytes)) => self.write(bytes).await,
                    _ => {
                        stdin_open = false;
                        // A terminal's end of input is the user's to send (Ctrl-D reaches claude as a byte).
                        if !tty {
                            self.close_stdin().await;
                        }
                    }
                },
                _ = winch.recv() => {
                    if let Some((rows, cols)) = terminal_size() {
                        self.resize(rows, cols).await;
                    }
                }
                _ = int.recv() => self.signal(libc::SIGINT).await,
                _ = term.recv() => self.signal(libc::SIGTERM).await,
                _ = hup.recv() => self.signal(libc::SIGHUP).await,
                code = self.wait() => break code?,
            }
        };
        // Output the session wrote before it ended may still be queued.
        while let Ok((chan, Some(bytes))) = output.try_recv() {
            write_out(chan, &bytes);
        }
        Ok(code)
    }
}

fn write_out(chan: u32, bytes: &[u8]) {
    use std::io::Write;
    if chan == STDIO {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes).and_then(|()| out.flush());
    } else {
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(bytes).and_then(|()| err.flush());
    }
}

/// This terminal's size, as rows and columns.
pub fn terminal_size() -> Option<(u16, u16)> {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ fills one winsize through a valid pointer.
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ as _, &mut size) } == 0;
    (ok && size.ws_row > 0 && size.ws_col > 0).then_some((size.ws_row, size.ws_col))
}

/// The terminal in raw mode while the session draws on it; restored however the session ends.
struct RawMode(nix::sys::termios::Termios);

impl RawMode {
    fn enter() -> Option<RawMode> {
        use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
        let stdin = std::io::stdin();
        let saved = tcgetattr(&stdin).ok()?;
        let mut raw = saved.clone();
        cfmakeraw(&mut raw);
        tcsetattr(&stdin, SetArg::TCSANOW, &raw).ok()?;
        Some(RawMode(saved))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = nix::sys::termios::tcsetattr(std::io::stdin(), nix::sys::termios::SetArg::TCSANOW, &self.0);
    }
}

/// One ssh connection running the bridge, after the handshake.
struct Connection {
    child: tokio::process::Child,
    reader: BufReader<tokio::process::ChildStdout>,
}

/// How a connection's handshake ended, short of an error.
enum Handshake {
    Ready,
    /// No helper of this version on the remote machine, which is of this platform.
    Missing(String),
}

impl Connection {
    /// Connects, deploying the helper first if needed. Errors when the run's holder is gone.
    async fn open(config: &Config, builds: &[Build], run: &str, first: bool, link: &Link) -> anyhow::Result<Connection> {
        let mut uploaded = false;
        loop {
            let argv = bridge_argv(config, &connect_script(builds, run, first), !first);
            trace("spawn", &json!({ "bridge": argv }).to_string());
            // ssh's stderr is relayed rather than shared: a ControlMaster takes over the descriptors
            // of a client and keeps them until the remote command ends, a round trip after this
            // process, which would hold the caller's stderr open that long. Relayed by line, so that
            // while claude's interface owns the terminal, ssh's messages wait with claudette's.
            let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
            let errors = child.stderr.take().map(|stderr| {
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        crate::log::stderr_line(&line);
                    }
                })
            });
            let mut reader = BufReader::new(child.stdout.take().expect("piped"));
            match handshake(config, run, first, link, &mut child, &mut reader).await {
                Ok(Handshake::Ready) => return Ok(Connection { child, reader }),
                Ok(Handshake::Missing(remote)) => {
                    link.detach().await;
                    let _ = child.wait().await;
                    let build = builds.iter().find(|b| b.platform == remote).with_context(|| {
                        format!("no claudette build for {} ({remote}): put the release's claudette-{remote} next to this claudette", config.host)
                    })?;
                    if uploaded {
                        bail!("claudette was copied to {} but does not run there", config.host);
                    }
                    upload(config, build).await?;
                    uploaded = true;
                }
                Err(e) => {
                    link.detach().await;
                    // What ssh said about it comes first. Its stderr ends with it, so ssh is stopped
                    // if it still runs.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    if let Some(errors) = errors {
                        let _ = tokio::time::timeout(Duration::from_secs(1), errors).await;
                    }
                    return Err(e);
                }
            }
        }
    }
}

/// The bridge's status line, then the helper's greeting.
async fn handshake(config: &Config, run: &str, first: bool, link: &Link, child: &mut tokio::process::Child, reader: &mut BufReader<tokio::process::ChildStdout>) -> anyhow::Result<Handshake> {
    let hello = || Frame::Hello(json!({ "protocol": PROTOCOL, "run": run, "received": link.received() })).encode(0);
    if first {
        // The holder is fresh, so the hello and all run frames go before READY. A missing helper
        // exits at once without reading them: a broken pipe here only means the line says why.
        let mut stdin = child.stdin.take().expect("piped");
        if stdin.write_all(&hello()).await.is_ok() && stdin.flush().await.is_ok() {
            let _ = link.attach(Box::new(stdin), 0).await;
        }
    }
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let line = line.trim();
    if let Some(remote) = line.strip_prefix(MISSING) {
        return Ok(Handshake::Missing(remote.trim().to_owned()));
    }
    if line == GONE {
        bail!("the session on {} is gone", config.host);
    }
    if line != READY {
        let status = child.wait().await;
        let hint = status.as_ref().map(|s| crate::ssh::failure_hint(config, *s)).unwrap_or_default();
        bail!("could not reach the claudette helper on {} ({}{hint})", config.host, status.map(|s| s.to_string()).unwrap_or_default());
    }
    let stdin = if first {
        None
    } else {
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(&hello()).await?;
        stdin.flush().await?;
        Some(stdin)
    };
    let hello = match read_frame(reader).await? {
        (_, Frame::Hello(v)) => v,
        _ => bail!("the helper on {} did not greet", config.host),
    };
    if hello.get("protocol").and_then(Value::as_u64) != Some(u64::from(PROTOCOL)) {
        bail!("the helper on {} speaks another protocol", config.host);
    }
    if let Some(stdin) = stdin {
        if hello.get("fresh").and_then(Value::as_bool) == Some(true) {
            bail!("the session on {} was lost", config.host);
        }
        link.attach(Box::new(stdin), hello.get("received").and_then(Value::as_u64).unwrap_or(0)).await?;
    }
    Ok(Handshake::Ready)
}

struct Supervisor {
    config: Config,
    builds: Vec<Build>,
    run: String,
    link: Arc<Link>,
    out: mpsc::UnboundedSender<(u32, Option<Vec<u8>>)>,
    exit: watch::Sender<Option<Result<i32, String>>>,
    done: Arc<AtomicBool>,
    locals: Arc<Vec<PathBuf>>,
    sockets: Sockets,
    first: Option<Connection>,
}

/// Keeps the session connected until it ends.
async fn supervise(mut s: Supervisor) {
    let mut backoff = Duration::from_secs(1);
    let mut lost_at: Option<std::time::Instant> = None;
    loop {
        let connection = match s.first.take() {
            Some(c) => Ok(c),
            None => Connection::open(&s.config, &s.builds, &s.run, false, &s.link).await,
        };
        let mut connection = match connection {
            Ok(c) => c,
            Err(e) => {
                if e.to_string().contains("gone") || e.to_string().contains("lost") {
                    let _ = s.exit.send(Some(Err(e.to_string())));
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(15));
                continue;
            }
        };
        if let Some(at) = lost_at.take() {
            warn(&format!("reconnected to {} after {}s", s.config.host, at.elapsed().as_secs()));
        }
        backoff = Duration::from_secs(1);
        let result = {
            let (out, exit, locals, sockets, link) = (s.out.clone(), s.exit.clone(), s.locals.clone(), s.sockets.clone(), s.link.clone());
            s.link
                .receive(&mut connection.reader, move |frame| {
                    let (out, exit, locals, sockets, link) = (out.clone(), exit.clone(), locals.clone(), sockets.clone(), link.clone());
                    async move { handle(frame, &out, &exit, &locals, &sockets, &link).await }
                })
                .await
        };
        s.link.detach().await;
        let _ = connection.child.start_kill();
        if s.done.load(Ordering::SeqCst) || s.exit.borrow().is_some() {
            return;
        }
        trace("tunnel", &format!("connection lost: {result:?}"));
        warn(&format!("the connection to {} was lost; the session waits while claudette reconnects", s.config.host));
        lost_at = Some(std::time::Instant::now());
    }
}

async fn handle(frame: Frame, out: &mpsc::UnboundedSender<(u32, Option<Vec<u8>>)>, exit: &watch::Sender<Option<Result<i32, String>>>, locals: &[PathBuf], sockets: &Sockets, link: &Arc<Link>) {
    match frame {
        Frame::Open { chan, index } => open(chan, locals.get(index as usize).cloned(), sockets, link).await,
        Frame::Data(chan, bytes) if chan <= 1 => {
            let _ = out.send((chan, Some(bytes)));
        }
        Frame::Close(chan) if chan <= 1 => {
            let _ = out.send((chan, None));
        }
        Frame::Data(chan, bytes) => {
            if let Some(queue) = sockets.lock().unwrap().get(&chan) {
                let _ = queue.send(Some(bytes));
            }
        }
        Frame::Close(chan) => {
            if let Some(queue) = sockets.lock().unwrap().get(&chan) {
                let _ = queue.send(None);
            }
        }
        Frame::Exit(code) => {
            trace("exit", &code.to_string());
            let _ = exit.send(Some(Ok(code)));
        }
        Frame::Notice(text) => warn(&text),
        _ => {}
    }
}

/// A connection the remote side opened: to the local socket it names.
async fn open(chan: u32, local: Option<PathBuf>, sockets: &Sockets, link: &Arc<Link>) {
    let conn = match local {
        Some(path) => UnixStream::connect(&path).await.ok(),
        None => None,
    };
    let Some(conn) = conn else {
        link.send(&Frame::Close(chan)).await;
        return;
    };
    let (mut read, mut write) = conn.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    sockets.lock().unwrap().insert(chan, tx);
    let cleanup = sockets.clone();
    tokio::spawn(async move {
        while let Some(Some(bytes)) = rx.recv().await {
            if write.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = write.shutdown().await;
        cleanup.lock().unwrap().remove(&chan);
    });
    let link = link.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => link.send(&Frame::Data(chan, buf[..n].to_vec())).await,
            }
        }
        link.send(&Frame::Close(chan)).await;
    });
}

/// The local socket paths a session serves, from the run's forwards.
pub fn socket_pairs(forwards: &crate::sockets::Forwards) -> Vec<(String, PathBuf)> {
    forwards.list.iter().map(|f| (f.remote.clone(), Path::new(&f.local).to_path_buf())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_command_runs_the_cached_build_for_the_machines_platform_or_names_the_platform() {
        let builds = vec![Build { platform: "linux-arm64".into(), file: std::fs::File::open("/dev/null").unwrap(), sha: "0123456789abcdef".repeat(4) }];
        let script = connect_script(&builds, "00000000000000ab", true);
        assert!(script.contains("linux-arm64) h=0123456789abcdef;;"));
        assert!(script.contains("__helper connect '00000000000000ab' --spawn"));
        assert!(script.ends_with(&format!("echo \"{MISSING} $p\"")));
        let out = std::process::Command::new("sh").arg("-c").arg(script.replace("$HOME", "/nonexistent")).output().unwrap();
        let line = String::from_utf8_lossy(&out.stdout);
        assert!(line.starts_with(MISSING), "{line}");
        assert!(!connect_script(&builds, "00000000000000ab", false).contains("--spawn"));
    }

    #[test]
    fn a_build_replaced_on_disk_still_gives_the_bytes_it_was_hashed_from() {
        let dir = std::env::temp_dir().join(format!("ct-build-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("claudette-linux-x64");
        std::fs::write(&path, b"old build").unwrap();
        let build = Build::open("linux-x64", &path).unwrap();
        // `+update` writes the new build beside the old one and renames it over.
        let next = dir.join(".claudette-linux-x64.update");
        std::fs::write(&next, b"the new build, longer").unwrap();
        std::fs::rename(&next, &path).unwrap();
        assert_eq!(build.bytes().unwrap(), b"old build");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
