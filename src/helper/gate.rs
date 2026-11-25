use std::io::Read;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const WAIT: Duration = Duration::from_secs(30);
const REQUEST_READ: Duration = Duration::from_secs(10);
const HEAD_LIMIT: usize = 16 * 1024;

pub async fn open(socket: &str, url: &str) -> i32 {
    let url = match crate::browser::check_url(url) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("claudette: {e}");
            return 1;
        }
    };
    let prepared = async { Ok::<_, std::io::Error>((TcpListener::bind(("127.0.0.1", 0)).await?, token()?)) };
    let (listener, token) = match prepared.await {
        Ok(ready) => ready,
        Err(e) => {
            eprintln!("claudette: could not get ready to hand the link over: {e}");
            return 1;
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            eprintln!("claudette: could not get ready to hand the link over: {e}");
            return 1;
        }
    };
    let status = super::desktop(socket, &format!("open {port} {token}"), false).await;
    if status != 0 {
        return status;
    }
    match tokio::time::timeout(WAIT, hand_over(listener, &token, url.as_str())).await {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            eprintln!("claudette: could not hand the link over: {e}");
            1
        }
        Err(_) => {
            eprintln!("claudette: the browser on the local machine did not come through this machine within {}s, so it was not given the link", WAIT.as_secs());
            1
        }
    }
}

fn token() -> std::io::Result<String> {
    let mut bytes = [0u8; crate::browser::GATE_TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn hand_over(listener: TcpListener, token: &str, url: &str) -> std::io::Result<()> {
    let expected = format!("GET /{token} ");
    let redirect = format!("HTTP/1.1 302 Found\r\nLocation: {url}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let (done, mut finished) = tokio::sync::mpsc::channel::<()>(1);
    loop {
        tokio::select! {
            _ = finished.recv() => return Ok(()),
            accepted = listener.accept() => {
                let (conn, _) = accepted?;
                let (expected, redirect, done) = (expected.clone(), redirect.clone(), done.clone());
                tokio::spawn(async move {
                    if answer(conn, &expected, &redirect).await {
                        let _ = done.send(()).await;
                    }
                });
            }
        }
    }
}

async fn answer(mut conn: TcpStream, expected: &str, redirect: &str) -> bool {
    let Ok(Ok(head)) = tokio::time::timeout(REQUEST_READ, read_head(&mut conn)).await else { return false };
    let ours = head.starts_with(expected.as_bytes());
    let reply = if ours { redirect } else { "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n" };
    let sent = conn.write_all(reply.as_bytes()).await.is_ok() && conn.shutdown().await.is_ok();
    ours && sent
}

async fn read_head(conn: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    while head.len() < HEAD_LIMIT && !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = conn.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..n]);
    }
    Ok(head)
}
