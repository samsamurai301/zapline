//! Helpers shared by the real-socket server tests.
#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use zapline::App;

/// An `App::serve_with_shutdown` server on `127.0.0.1:0`.
pub struct TestServer {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<()>>,
}

impl TestServer {
    pub async fn spawn(app: App) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(app.serve_with_shutdown(listener, async move {
            let _ = rx.await;
        }));
        TestServer {
            addr,
            stop: Some(tx),
            task,
        }
    }

    /// Start a graceful shutdown.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }

    /// Wait for `serve` to return (fails the test after 10s).
    pub async fn join(self) -> io::Result<()> {
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("server did not shut down")
            .expect("server task panicked")
    }
}

/// A port that was free a moment ago.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Wait until something accepts connections on `addr`.
pub async fn wait_ready(addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(addr).await.is_err() {
        assert!(Instant::now() < deadline, "server at {addr} never came up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until connections to `addr` are refused (the listener is closed).
pub async fn wait_refused(addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(addr).await.is_ok() {
        assert!(
            Instant::now() < deadline,
            "server at {addr} still accepting"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Send raw bytes on a new connection and read until the server closes it.
pub async fn raw(addr: SocketAddr, request: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    read_to_close(&mut s).await
}

/// Read until EOF; a reset counts as EOF.
pub async fn read_to_close<S: AsyncRead + Unpin>(s: &mut S) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GET path` with keep-alive on an open connection; returns one response.
pub async fn get_on<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, path: &str) -> String {
    let req = format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    read_response(s)
        .await
        .expect("connection closed before a response")
}

/// Read one `Content-Length`-delimited HTTP/1.1 response.
pub async fn read_response<S: AsyncRead + Unpin>(s: &mut S) -> Option<String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    while !out.ends_with(b"\r\n\r\n") {
        match s.read(&mut byte).await {
            Ok(1) => out.push(byte[0]),
            _ => return None,
        }
    }
    let head = String::from_utf8_lossy(&out).to_ascii_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map_or(0, |v| v.trim().parse().unwrap());
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.ok()?;
    out.extend_from_slice(&body);
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// A blocking-thread client runtime for tests that drive `App::run`.
pub fn client_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
