//! The stream between claudette here and its holder on the remote machine, carried over ssh's
//! stdio. It survives ssh going away: every frame but HELLO and ACK has a sequence number, each side
//! keeps what the other has not acknowledged, and after a reconnect each resends from where the other
//! says it got to. A frame is `u32 length | u64 sequence (0 = none) | u8 kind | body`, big-endian.
//!
//! Both ends use it: [`crate::tunnel`] here, [`crate::helper`] on the remote machine. Before the
//! frames, the helper's bridge prints one status line ([`READY`], [`MISSING`] or [`GONE`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped when frames change; both ends run the same build, so this only guards against mix-ups.
pub const PROTOCOL: u32 = 1;
/// Largest frame accepted.
const MAX_FRAME: usize = 16 << 20;
/// Unacknowledged bytes a side holds before its senders wait: what piles up while ssh is away.
const MAX_UNACKED: usize = 64 << 20;

/// Channel 0 is claude's stdio (or its terminal), 1 its stderr; sockets get 2 and up.
pub const STDIO: u32 = 0;
pub const STDERR: u32 = 1;

/// The first line the bridge prints, before the frames: the binary ran and reached the holder.
pub const READY: &str = "claudette helper ready";
/// Printed instead when the binary for this machine is not there yet, with the platform after it.
pub const MISSING: &str = "claudette helper missing";
/// Printed by `connect` without `--spawn` when the run has no holder any more.
pub const GONE: &str = "claudette helper gone";

/// What the holder runs, sent once at the start of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    /// The session, as a POSIX shell script: the workspace setup, then claude.
    pub command: String,
    /// The Unix sockets the holder serves for this machine, by index.
    pub sockets: Vec<String>,
    /// A terminal of this size for claude's interface; pipes otherwise.
    pub pty: Option<(u16, u16)>,
    /// Extra environment for the session (the terminal's `TERM`, say).
    pub env: Map<String, Value>,
    /// How long the holder keeps a session whose local machine has gone away.
    pub detach_timeout: u64,
}

impl Start {
    pub fn to_json(&self) -> Value {
        json!({
            "command": self.command,
            "sockets": self.sockets,
            "pty": self.pty.map(|(rows, cols)| json!({ "rows": rows, "cols": cols })),
            "env": self.env,
            "detachTimeout": self.detach_timeout,
        })
    }

    pub fn from_json(v: &Value) -> Option<Start> {
        Some(Start {
            command: v.get("command")?.as_str()?.to_owned(),
            sockets: v.get("sockets")?.as_array()?.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect(),
            pty: v.get("pty").filter(|p| !p.is_null()).and_then(|p| Some((p.get("rows")?.as_u64()? as u16, p.get("cols")?.as_u64()? as u16))),
            env: v.get("env").and_then(Value::as_object).cloned().unwrap_or_default(),
            detach_timeout: v.get("detachTimeout").and_then(Value::as_u64).unwrap_or(12 * 3600),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// First frame each way on every connection: JSON with `protocol`, `run`, `received`, `fresh`.
    Hello(Value),
    /// Local → holder, once: what to run (JSON, see [`Start`]).
    Start(Value),
    /// Local → holder, before Start: a private run file, by basename.
    File { name: String, data: Vec<u8> },
    Data(u32, Vec<u8>),
    /// Holder → local: a connection arrived on socket `index`, as channel `chan`.
    Open { chan: u32, index: u32 },
    /// The side that sends it has nothing more to write on the channel.
    Close(u32),
    Resize { rows: u16, cols: u16 },
    Signal(i32),
    /// Holder → local: the session ended with this code.
    Exit(i32),
    /// Local → holder: this machine is done; end the session.
    Bye,
    /// Every frame up to this sequence number arrived.
    Ack(u64),
    /// Local → holder: one line for claude's inbox.
    Deliver(Vec<u8>),
    /// Holder → local: something to tell the user.
    Notice(String),
}

impl Frame {
    fn kind(&self) -> u8 {
        match self {
            Frame::Hello(_) => 1,
            Frame::Start(_) => 2,
            Frame::File { .. } => 13,
            Frame::Data(..) => 3,
            Frame::Open { .. } => 4,
            Frame::Close(_) => 5,
            Frame::Resize { .. } => 6,
            Frame::Signal(_) => 7,
            Frame::Exit(_) => 8,
            Frame::Bye => 9,
            Frame::Ack(_) => 10,
            Frame::Deliver(_) => 11,
            Frame::Notice(_) => 12,
        }
    }

    fn body(&self) -> Vec<u8> {
        match self {
            Frame::Hello(v) | Frame::Start(v) => v.to_string().into_bytes(),
            Frame::File { name, data } => [&(name.len() as u32).to_be_bytes()[..], name.as_bytes(), data].concat(),
            Frame::Data(chan, data) => [&chan.to_be_bytes()[..], data].concat(),
            Frame::Open { chan, index } => [chan.to_be_bytes(), index.to_be_bytes()].concat(),
            Frame::Close(chan) => chan.to_be_bytes().to_vec(),
            Frame::Resize { rows, cols } => [rows.to_be_bytes(), cols.to_be_bytes()].concat(),
            Frame::Signal(n) | Frame::Exit(n) => n.to_be_bytes().to_vec(),
            Frame::Bye => vec![],
            Frame::Ack(seq) => seq.to_be_bytes().to_vec(),
            Frame::Deliver(line) => line.clone(),
            Frame::Notice(text) => text.as_bytes().to_vec(),
        }
    }

    fn parse(kind: u8, body: &[u8]) -> std::io::Result<Frame> {
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad frame of kind {kind}"));
        let u32_at = |at: usize| body.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().expect("4 bytes"))).ok_or_else(bad);
        Ok(match kind {
            1 | 2 => {
                let v: Value = serde_json::from_slice(body).map_err(|_| bad())?;
                if kind == 1 { Frame::Hello(v) } else { Frame::Start(v) }
            }
            3 => Frame::Data(u32_at(0)?, body[4..].to_vec()),
            4 => Frame::Open { chan: u32_at(0)?, index: u32_at(4)? },
            5 => Frame::Close(u32_at(0)?),
            6 => {
                let v = u32_at(0)?;
                Frame::Resize { rows: (v >> 16) as u16, cols: (v & 0xffff) as u16 }
            }
            7 => Frame::Signal(u32_at(0)? as i32),
            8 => Frame::Exit(u32_at(0)? as i32),
            9 => Frame::Bye,
            10 => Frame::Ack(body.get(..8).map(|b| u64::from_be_bytes(b.try_into().expect("8 bytes"))).ok_or_else(bad)?),
            11 => Frame::Deliver(body.to_vec()),
            12 => Frame::Notice(String::from_utf8_lossy(body).into_owned()),
            13 => {
                let len = u32_at(0)? as usize;
                let end = 4usize.checked_add(len).ok_or_else(bad)?;
                let name = body.get(4..end).ok_or_else(bad).and_then(|b| String::from_utf8(b.to_vec()).map_err(|_| bad()))?;
                Frame::File { name, data: body.get(end..).ok_or_else(bad)?.to_vec() }
            }
            _ => return Err(bad()),
        })
    }

    pub fn encode(&self, seq: u64) -> Vec<u8> {
        let body = self.body();
        let len = (8 + 1 + body.len()) as u32;
        let mut out = Vec::with_capacity(4 + len as usize);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&seq.to_be_bytes());
        out.push(self.kind());
        out.extend_from_slice(&body);
        out
    }
}

/// Reads one frame and its sequence number.
pub async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<(u64, Frame)> {
    let len = reader.read_u32().await? as usize;
    if !(9..=MAX_FRAME).contains(&len) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let seq = u64::from_be_bytes(buf[..8].try_into().expect("8 bytes"));
    Ok((seq, Frame::parse(buf[8], &buf[9..])?))
}

type Writer = Box<dyn AsyncWrite + Unpin + Send>;

struct Out {
    writer: Option<Writer>,
    next: u64,
    unacked: VecDeque<(u64, Vec<u8>)>,
    bytes: usize,
}

/// One side of the resumable stream.
pub struct Link {
    out: tokio::sync::Mutex<Out>,
    /// Bumped when frames are acknowledged, so blocked senders look again.
    room: tokio::sync::watch::Sender<u64>,
    /// The last sequence number received in order.
    received: AtomicU64,
    /// The last acknowledgement sent.
    acked: AtomicU64,
}

impl Default for Link {
    fn default() -> Self {
        Link {
            out: tokio::sync::Mutex::new(Out { writer: None, next: 1, unacked: VecDeque::new(), bytes: 0 }),
            room: tokio::sync::watch::channel(0).0,
            received: AtomicU64::new(0),
            acked: AtomicU64::new(0),
        }
    }
}

impl Link {
    /// Sends a frame, now if connected, otherwise once reconnected. Waits while too much is
    /// unacknowledged, which holds back the sources feeding it.
    pub async fn send(&self, frame: &Frame) {
        let mut room = self.room.subscribe();
        loop {
            let mut out = self.out.lock().await;
            if out.bytes > MAX_UNACKED {
                drop(out);
                if room.changed().await.is_err() {
                    return;
                }
                continue;
            }
            let seq = out.next;
            out.next += 1;
            let bytes = frame.encode(seq);
            out.bytes += bytes.len();
            if let Some(writer) = out.writer.as_mut()
                && (writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err())
            {
                out.writer = None;
            }
            out.unacked.push_back((seq, bytes));
            return;
        }
    }

    /// Sends a frame without a sequence number (HELLO, ACK), only if connected.
    pub async fn send_now(&self, frame: &Frame) {
        let mut out = self.out.lock().await;
        if let Some(writer) = out.writer.as_mut()
            && (writer.write_all(&frame.encode(0)).await.is_err() || writer.flush().await.is_err())
        {
            out.writer = None;
        }
    }

    /// Drops frames the other side has.
    pub async fn acknowledged(&self, upto: u64) {
        let mut out = self.out.lock().await;
        while out.unacked.front().is_some_and(|(seq, _)| *seq <= upto) {
            let (_, bytes) = out.unacked.pop_front().expect("checked");
            out.bytes -= bytes.len();
        }
        drop(out);
        self.room.send_replace(upto);
    }

    /// A new connection: the other side has everything up to `peer_received`; the rest is sent again,
    /// in order, before anything new.
    pub async fn attach(&self, mut writer: Writer, peer_received: u64) -> std::io::Result<()> {
        self.acknowledged(peer_received).await;
        let mut out = self.out.lock().await;
        for (_, bytes) in &out.unacked {
            writer.write_all(bytes).await?;
        }
        writer.flush().await?;
        out.writer = Some(writer);
        Ok(())
    }

    pub async fn detach(&self) {
        self.out.lock().await.writer = None;
    }

    pub fn received(&self) -> u64 {
        self.received.load(Ordering::SeqCst)
    }

    /// Whether a received sequenced frame is new (and so to be handled). A gap means frames were
    /// lost, which a correct peer never causes: the connection is then given up.
    pub fn accept(&self, seq: u64) -> std::io::Result<bool> {
        let last = self.received.load(Ordering::SeqCst);
        if seq <= last {
            return Ok(false);
        }
        if seq != last + 1 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("frame {seq} after {last}")));
        }
        self.received.store(seq, Ordering::SeqCst);
        Ok(true)
    }

    /// Acknowledges what arrived since the last acknowledgement.
    pub async fn ack(&self) {
        let received = self.received();
        if received > self.acked.swap(received, Ordering::SeqCst) {
            self.send_now(&Frame::Ack(received)).await;
        }
    }

    /// Reads frames until the connection ends: acknowledgements are applied, duplicates dropped, and
    /// the rest handed to `handle` in order. Acknowledges every 64 frames and on a timer.
    pub async fn receive<R, F, Fut>(&self, mut reader: R, mut handle: F) -> std::io::Result<()>
    where
        R: AsyncRead + Unpin,
        F: FnMut(Frame) -> Fut,
        Fut: Future<Output = ()>,
    {
        // Two futures that live for the whole connection: a frame half read is never dropped.
        let reading = async {
            let mut since = 0u32;
            loop {
                let (seq, frame) = match read_frame(&mut reader).await {
                    Ok(read) => read,
                    Err(e) => return Err::<(), std::io::Error>(e),
                };
                match frame {
                    Frame::Ack(upto) => self.acknowledged(upto).await,
                    Frame::Hello(_) => {}
                    frame if seq != 0 => {
                        if self.accept(seq)? {
                            handle(frame).await;
                            since += 1;
                            if since >= 64 {
                                since = 0;
                                self.ack().await;
                            }
                        }
                    }
                    frame => handle(frame).await,
                }
            }
        };
        let ticking = async {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                tick.tick().await;
                self.ack().await;
            }
        };
        tokio::select! {
            result = reading => result,
            () = ticking => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_round_trips() {
        let start = Start { command: "echo hi".into(), sockets: vec!["/tmp/a.sock".into()], pty: Some((40, 120)), env: Map::new(), detach_timeout: 60 };
        assert_eq!(Start::from_json(&start.to_json()), Some(start));
    }

    #[test]
    fn every_frame_round_trips() {
        let frames = [
            Frame::Hello(json!({ "protocol": 1 })),
            Frame::Start(json!({ "command": "x" })),
            Frame::File { name: "mcp.json".into(), data: b"{}".to_vec() },
            Frame::Data(7, b"bytes".to_vec()),
            Frame::Open { chan: 3, index: 9 },
            Frame::Close(4),
            Frame::Resize { rows: 40, cols: 120 },
            Frame::Signal(15),
            Frame::Exit(-1),
            Frame::Bye,
            Frame::Ack(1 << 40),
            Frame::Deliver(b"{}".to_vec()),
            Frame::Notice("hi".into()),
        ];
        for frame in frames {
            let bytes = frame.encode(5);
            let (seq, back) = futures_util::FutureExt::now_or_never(read_frame(&mut &bytes[..])).unwrap().unwrap();
            assert_eq!((seq, back), (5, frame));
        }
    }

    #[tokio::test]
    async fn after_a_reconnect_only_the_unacknowledged_frames_go_again_in_order() {
        let link = Link::default();
        // Disconnected: frames wait.
        for n in 0..3u32 {
            link.send(&Frame::Data(0, vec![n as u8])).await;
        }
        let (mut a, b) = tokio::io::duplex(1 << 16);
        link.attach(Box::new(b), 1).await.unwrap();
        link.send(&Frame::Data(0, vec![3])).await;
        let mut got = vec![];
        for _ in 0..3 {
            let (seq, frame) = read_frame(&mut a).await.unwrap();
            got.push((seq, frame));
        }
        assert_eq!(got, [(2, Frame::Data(0, vec![1])), (3, Frame::Data(0, vec![2])), (4, Frame::Data(0, vec![3]))]);
    }

    #[test]
    fn duplicates_are_dropped_and_gaps_refused() {
        let link = Link::default();
        assert!(link.accept(1).unwrap());
        assert!(!link.accept(1).unwrap());
        assert!(link.accept(2).unwrap());
        assert!(link.accept(4).is_err());
    }
}
