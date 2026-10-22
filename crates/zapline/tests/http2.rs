//! HTTP/2 with prior knowledge (h2c) next to HTTP/1.1 on the same port.
#![cfg(feature = "http2")]

mod server_support;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Empty, Full};
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo};
use server_support::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .get("/", |c: ConnInfo| async move {
            format!("hello h2 secure={}", c.secure)
        })
        .post("/echo", |body: Bytes| async move { body })
        .get("/slow", || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "done"
        })
        .get("/version", |req: Request| async move {
            format!("{:?}", req.head().version)
        })
}

async fn h2c<B>(addr: SocketAddr) -> SendRequest<B>
where
    B: http_body::Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let tcp = TcpStream::connect(addr).await.unwrap();
    let (sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(conn);
    sender
}

async fn get(sender: &mut SendRequest<Empty<Bytes>>, path: &str) -> (http::StatusCode, String) {
    let req = http::Request::get(format!("http://localhost{path}"))
        .body(Empty::new())
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    assert_eq!(res.version(), http::Version::HTTP_2);
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn h2c_prior_knowledge_round_trip() {
    let server = TestServer::spawn(app()).await;
    let mut sender = h2c(server.addr).await;
    assert_eq!(
        get(&mut sender, "/").await,
        (http::StatusCode::OK, "hello h2 secure=false".to_owned())
    );
    assert_eq!(get(&mut sender, "/version").await.1, "HTTP/2.0");
    assert_eq!(get(&mut sender, "/missing").await.0, 404);
}

#[tokio::test]
async fn h2c_request_bodies() {
    let server = TestServer::spawn(app()).await;
    let mut sender = h2c(server.addr).await;
    let req = http::Request::post("http://localhost/echo")
        .body(Full::new(Bytes::from_static(b"ping over h2")))
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    assert_eq!(res.status(), 200);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, "ping over h2");
}

#[tokio::test]
async fn many_concurrent_streams_on_one_connection() {
    let server = TestServer::spawn(app()).await;
    let sender = h2c(server.addr).await;
    let started = Instant::now();
    let streams: Vec<_> = (0..20)
        .map(|_| {
            let mut sender = sender.clone();
            tokio::spawn(async move { get(&mut sender, "/slow").await })
        })
        .collect();
    for s in streams {
        assert_eq!(s.await.unwrap(), (http::StatusCode::OK, "done".to_owned()));
    }
    // 20 x 300ms handlers multiplexed on one connection run concurrently.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn http1_still_works_on_the_same_port() {
    let server = TestServer::spawn(app()).await;
    let out = raw(
        server.addr,
        "GET /version HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(out.ends_with("HTTP/1.1"), "{out}");

    // A request line split across writes, starting with the same byte as
    // the HTTP/2 preface, is still HTTP/1.1.
    let mut s = TcpStream::connect(server.addr).await.unwrap();
    s.write_all(b"P").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.write_all(
        b"OST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
    )
    .await
    .unwrap();
    let out = read_to_close(&mut s).await;
    assert!(
        out.starts_with("HTTP/1.1 200 OK") && out.ends_with("hi"),
        "{out}"
    );
}

#[tokio::test]
async fn http2_can_be_disabled() {
    let server = TestServer::spawn(app().http2(false)).await;
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let handshake = hyper::client::conn::http2::handshake::<_, _, Empty<Bytes>>(
        TokioExecutor::new(),
        TokioIo::new(tcp),
    )
    .await;
    match handshake {
        Err(_) => {}
        Ok((mut sender, conn)) => {
            tokio::spawn(conn);
            let req = http::Request::get("http://localhost/")
                .body(Empty::new())
                .unwrap();
            assert!(
                sender.send_request(req).await.is_err(),
                "h2c served with http2(false)"
            );
        }
    }
    let out = raw(
        server.addr,
        "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
}

#[tokio::test]
async fn graceful_shutdown_lets_open_streams_finish() {
    let mut server = TestServer::spawn(app()).await;
    let addr = server.addr;
    let mut sender = h2c::<Empty<Bytes>>(addr).await;
    let in_flight = tokio::spawn(async move { get(&mut sender, "/slow").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.stop();
    wait_refused(addr).await;
    assert_eq!(
        in_flight.await.unwrap(),
        (http::StatusCode::OK, "done".to_owned())
    );
    server.join().await.unwrap();
}

#[tokio::test]
async fn shutdown_timeout_closes_hanging_streams() {
    let app = app()
        .get("/hang", || async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            "never"
        })
        .shutdown_timeout(Duration::from_millis(200));
    let mut server = TestServer::spawn(app).await;
    let mut sender = h2c::<Empty<Bytes>>(server.addr).await;
    let hang = tokio::spawn(async move {
        let req = http::Request::get("http://localhost/hang")
            .body(Empty::new())
            .unwrap();
        sender.send_request(req).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let start = Instant::now();
    server.stop();
    server.join().await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(hang.await.unwrap().is_err(), "hanging stream must fail");
}

#[tokio::test]
async fn silent_connections_time_out_during_protocol_detection() {
    let mut app = app();
    app.server_config_mut().header_read_timeout = Some(Duration::from_millis(200));
    let server = TestServer::spawn(app).await;
    // A prefix of the HTTP/2 preface, then silence.
    let mut s = TcpStream::connect(server.addr).await.unwrap();
    s.write_all(b"PRI * HTTP/2.0\r\n").await.unwrap();
    // Detection gives up on a partial preface after a short bounded wait
    // (no busy polling), then HTTP/1 rejects or times out the connection.
    tokio::time::timeout(Duration::from_secs(5), read_to_close(&mut s))
        .await
        .expect("connection stuck in protocol detection");
}

#[test]
fn run_serves_h2c_on_worker_threads() {
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut app = app().workers(2);
    app.server_config_mut().banner = false;
    let server = std::thread::spawn(move || {
        app.run_with_shutdown(addr, async move {
            let _ = stopped.await;
        })
    });
    client_runtime().block_on(async {
        wait_ready(addr).await;
        let mut sender = h2c(addr).await;
        assert_eq!(get(&mut sender, "/version").await.1, "HTTP/2.0");
        drop(sender);
        stop.send(()).unwrap();
    });
    server.join().unwrap().unwrap();
}

#[cfg(unix)]
#[test]
fn h2c_over_a_unix_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h2.sock");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut app = app();
    app.server_config_mut().banner = false;
    let server_path = path.clone();
    let server = std::thread::spawn(move || {
        app.run_unix_with_shutdown(&server_path, async move {
            let _ = stopped.await;
        })
    });
    client_runtime().block_on(async {
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match tokio::net::UnixStream::connect(&path).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(e) => panic!("unix server never came up: {e}"),
            }
        };
        let (mut sender, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(conn);
        assert_eq!(get(&mut sender, "/version").await.1, "HTTP/2.0");
        assert_eq!(get(&mut sender, "/").await.1, "hello h2 secure=false");
        drop(sender);
        stop.send(()).unwrap();
    });
    server.join().unwrap().unwrap();
}

#[tokio::test]
async fn idle_connections_are_closed() {
    let mut app = app();
    app.server_config_mut().header_read_timeout = Some(Duration::from_millis(200));
    let server = TestServer::spawn(app).await;
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    let conn = tokio::spawn(conn);
    // A request longer than the idle time is not idle.
    assert_eq!(get(&mut sender, "/slow").await.1, "done");
    assert_eq!(get(&mut sender, "/").await.0, 200);
    // Then nothing: the server says goodbye.
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), conn)
        .await
        .expect("idle HTTP/2 connection stayed open")
        .unwrap()
        .ok();
    assert!(started.elapsed() >= Duration::from_millis(150));
}
