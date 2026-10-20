//! Real-socket tests for the production server: graceful shutdown, limits,
//! runtime modes and HTTP/1.1 behaviour.

mod server_support;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use server_support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zapline::prelude::*;
use zapline::server::RuntimeMode;
use zapline::{Body, ServerConfig};

const SLOW: &str = "GET /slow HTTP/1.1\r\nHost: test\r\n\r\n";

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_millis(300)).await;
    "done"
}

fn flag() -> (
    Arc<AtomicBool>,
    impl FnOnce(Resources) -> std::future::Ready<()> + Send,
) {
    let f = Arc::new(AtomicBool::new(false));
    let g = f.clone();
    (f, move |_| {
        g.store(true, Ordering::SeqCst);
        std::future::ready(())
    })
}

fn quiet(mut app: App) -> App {
    app.server_config_mut().banner = false;
    app
}

// ---- graceful shutdown ----------------------------------------------------------

#[tokio::test]
async fn serve_graceful_shutdown_finishes_in_flight_request() {
    let (hook_ran, hook) = flag();
    let app = App::new().get("/slow", slow).on_shutdown(hook);
    let mut server = TestServer::spawn(app).await;
    let addr = server.addr;

    let in_flight = tokio::spawn(raw(addr, SLOW));
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.stop();

    // New connections are refused while the in-flight request finishes.
    wait_refused(addr).await;
    let out = in_flight.await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
    assert!(out.contains("connection: close"), "{out}");
    assert!(out.ends_with("done"), "{out}");

    server.join().await.unwrap();
    assert!(hook_ran.load(Ordering::SeqCst), "shutdown hook did not run");
}

#[tokio::test]
async fn idle_keep_alive_connections_do_not_delay_shutdown() {
    let app = App::new().get("/", || async { "ok" });
    let mut server = TestServer::spawn(app).await;
    let mut conn = TcpStream::connect(server.addr).await.unwrap();
    assert!(get_on(&mut conn, "/").await.ends_with("ok"));

    let start = Instant::now();
    server.stop();
    server.join().await.unwrap();
    // The default shutdown timeout is 30s; an idle connection closes at once.
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(read_to_close(&mut conn).await, "");
}

#[tokio::test]
async fn shutdown_timeout_closes_a_never_ending_request() {
    let app = App::new()
        .get("/hang", || async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            "never"
        })
        .shutdown_timeout(Duration::from_millis(200));
    let mut server = TestServer::spawn(app).await;
    let hang = tokio::spawn(raw(server.addr, "GET /hang HTTP/1.1\r\nHost: x\r\n\r\n"));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let start = Instant::now();
    server.stop();
    server.join().await.unwrap();
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(150), "{took:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert_eq!(
        hang.await.unwrap(),
        "",
        "connection must close without a response"
    );
}

#[tokio::test]
async fn shutdown_timeout_cuts_off_an_endless_streaming_response() {
    let app = App::new()
        .get("/stream", || async {
            let ticks = futures_util::stream::unfold(0u64, |i| async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let frame = http_body::Frame::data(Bytes::from(format!("tick {i}\n")));
                Some((Ok::<_, io::Error>(frame), i + 1))
            });
            Response::new(Body::stream(http_body_util::StreamBody::new(ticks)))
        })
        .shutdown_timeout(Duration::from_millis(200));
    let mut server = TestServer::spawn(app).await;
    let mut s = TcpStream::connect(server.addr).await.unwrap();
    s.write_all(b"GET /stream HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut first = [0u8; 64];
    let n = s.read(&mut first).await.unwrap();
    assert!(String::from_utf8_lossy(&first[..n]).starts_with("HTTP/1.1 200 OK"));

    server.stop();
    server.join().await.unwrap();
    let rest = read_to_close(&mut s).await;
    assert!(rest.contains("tick"), "{rest}");
    assert!(
        !rest.ends_with("0\r\n\r\n"),
        "a cut-off chunked body must not be terminated: {rest}"
    );
}

#[test]
fn run_with_shutdown_per_core_drains_all_workers_and_runs_hooks() {
    let port = free_port();
    let addr = ([127, 0, 0, 1], port).into();
    let (hook_ran, hook) = flag();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let app = quiet(
        App::new()
            .get("/slow", slow)
            .get("/thread", || async {
                std::thread::current().name().unwrap_or("").to_owned()
            })
            .workers(2)
            .runtime(RuntimeMode::PerCore)
            .on_shutdown(hook),
    );
    let server = std::thread::spawn(move || {
        app.run_with_shutdown(addr, async move {
            let _ = stopped.await;
        })
    });

    client_runtime().block_on(async {
        wait_ready(addr).await;
        let thread = raw(
            addr,
            "GET /thread HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(thread.contains("zapline-worker-"), "{thread}");

        let in_flight = tokio::spawn(raw(addr, SLOW));
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.send(()).unwrap();
        wait_refused(addr).await;
        let out = in_flight.await.unwrap();
        assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
        assert!(out.ends_with("done"), "{out}");
    });

    server.join().unwrap().unwrap();
    assert!(hook_ran.load(Ordering::SeqCst), "shutdown hook did not run");
}

#[test]
fn multi_thread_runtime_mode_serves_requests() {
    let port = free_port();
    let addr = ([127, 0, 0, 1], port).into();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let app = quiet(
        App::new()
            .get("/thread", || async {
                std::thread::current().name().unwrap_or("").to_owned()
            })
            .get("/slow", slow)
            .workers(3)
            .runtime(RuntimeMode::MultiThread),
    );
    let server = std::thread::spawn(move || {
        app.run_with_shutdown(addr, async move {
            let _ = stopped.await;
        })
    });

    client_runtime().block_on(async {
        wait_ready(addr).await;
        let started = Instant::now();
        let requests: Vec<_> = (0..8)
            .map(|_| {
                tokio::spawn(raw(
                    addr,
                    "GET /slow HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
                ))
            })
            .collect();
        for r in requests {
            assert!(r.await.unwrap().ends_with("done"));
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "requests were serialized"
        );

        let thread = raw(
            addr,
            "GET /thread HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(thread.ends_with("\r\n\r\nzapline-worker"), "{thread}");
        stop.send(()).unwrap();
    });
    server.join().unwrap().unwrap();
}

#[test]
fn run_inside_a_runtime_is_an_error_not_a_panic() {
    let err = client_runtime()
        .block_on(async { App::new().run("127.0.0.1:0") })
        .unwrap_err();
    assert!(err.to_string().contains("App::serve"), "{err}");
}

#[test]
fn startup_failure_is_reported_by_run() {
    let app = quiet(App::new().on_startup(|_| async { Err(Error::internal("db down")) }));
    let err = app
        .run_with_shutdown("127.0.0.1:0", std::future::pending())
        .unwrap_err();
    assert!(err.to_string().contains("startup failed"), "{err}");
}

// ---- limits & tuning -------------------------------------------------------------

#[tokio::test]
async fn max_connections_pauses_accepting_until_a_connection_closes() {
    let app = App::new().get("/", || async { "ok" }).max_connections(1);
    let mut server = TestServer::spawn(app).await;

    let mut first = TcpStream::connect(server.addr).await.unwrap();
    assert!(get_on(&mut first, "/").await.ends_with("ok"));

    // The kernel completes the handshake, but the server does not accept it.
    let mut second = TcpStream::connect(server.addr).await.unwrap();
    second
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let waiting =
        tokio::time::timeout(Duration::from_millis(300), read_response(&mut second)).await;
    assert!(
        waiting.is_err(),
        "second connection was served over the limit"
    );

    drop(first);
    let served = tokio::time::timeout(Duration::from_secs(5), read_response(&mut second))
        .await
        .expect("second connection never served")
        .unwrap();
    assert!(served.ends_with("ok"), "{served}");

    server.stop();
    drop(second);
    server.join().await.unwrap();
}

#[tokio::test]
async fn pipelined_requests_are_answered_in_order() {
    let app = App::new()
        .get("/a", || async { "first" })
        .get("/b", || async { "second" })
        .get("/c", || async { "third" });
    let server = TestServer::spawn(app).await;
    let out = raw(
        server.addr,
        "GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\nHost: x\r\n\r\nGET /c HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(out.matches("HTTP/1.1 200 OK").count(), 3, "{out}");
    let (a, b, c) = (
        out.find("first").unwrap(),
        out.find("second").unwrap(),
        out.find("third").unwrap(),
    );
    assert!(a < b && b < c, "{out}");
}

#[tokio::test]
async fn waiting_handlers_are_woken_on_keep_alive_connections() {
    // HTTP/1 polls each response future once before hyper does, with a
    // no-op waker: handlers that wait (a timer, a body still in flight) must
    // still be woken through the connection.
    let app = App::new()
        .get("/sleep/{ms}", |Path(ms): Path<u64>| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            format!("slept {ms}")
        })
        .post("/echo", |body: String| async move { body });
    let server = TestServer::spawn(app).await;
    let mut conn = TcpStream::connect(server.addr).await.unwrap();
    let exchange = async {
        let slept = get_on(&mut conn, "/sleep/50").await;
        let head = "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\n";
        conn.write_all(head.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        conn.write_all(b"hello").await.unwrap();
        let echoed = read_response(&mut conn).await.unwrap();
        (slept, echoed)
    };
    let (slept, echoed) = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("a waiting handler was never woken");
    assert!(slept.ends_with("slept 50"), "{slept}");
    assert!(echoed.ends_with("hello"), "{echoed}");
}

#[tokio::test]
async fn keep_alive_can_be_disabled() {
    let mut app = App::new().get("/", || async { "ok" });
    app.server_config_mut().keep_alive = false;
    let server = TestServer::spawn(app).await;
    // No `Connection: close` from the client, yet the server closes.
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        raw(server.addr, "GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
    )
    .await
    .expect("server kept the connection open");
    assert!(out.contains("connection: close"), "{out}");
    assert!(out.ends_with("ok"), "{out}");
}

#[tokio::test]
async fn oversized_request_heads_are_rejected() {
    let big_request = format!(
        "GET / HTTP/1.1\r\nHost: x\r\nX-Big: {}\r\n\r\n",
        "a".repeat(2000)
    );
    let ok_request = "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

    // Default (pipeline flushing on): the connection is closed, no response.
    let mut app = App::new().get("/", || async { "ok" });
    app.server_config_mut().max_header_size = Some(1024);
    let server = TestServer::spawn(app).await;
    let out = raw(server.addr, &big_request).await;
    assert!(!out.contains("200 OK"), "{out}");
    assert!(raw(server.addr, ok_request).await.ends_with("ok"));

    // Without pipeline flushing hyper answers with 431.
    let mut app = App::new().get("/", || async { "ok" });
    let config = app.server_config_mut();
    config.max_header_size = Some(1024);
    config.pipeline_flush = false;
    let server = TestServer::spawn(app).await;
    let out = raw(server.addr, &big_request).await;
    assert!(out.starts_with("HTTP/1.1 431"), "{out}");
    assert!(raw(server.addr, ok_request).await.ends_with("ok"));
}

#[tokio::test]
async fn silent_connections_are_dropped_after_the_header_timeout() {
    let mut app = App::new().get("/", || async { "ok" });
    app.server_config_mut().header_read_timeout = Some(Duration::from_millis(200));
    let server = TestServer::spawn(app).await;
    let mut s = TcpStream::connect(server.addr).await.unwrap();
    let start = Instant::now();
    let out = tokio::time::timeout(Duration::from_secs(5), read_to_close(&mut s))
        .await
        .expect("silent connection was never closed");
    assert!(!out.contains("200 OK"), "{out}");
    assert!(start.elapsed() >= Duration::from_millis(150));
}

#[tokio::test]
async fn invalid_config_is_rejected_before_serving() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut app = App::new();
    app.server_config_mut().max_buf_size = Some(100);
    let err = app.serve(listener).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("max_buf_size"), "{err}");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut app = App::new();
    app.server_config_mut().max_connections = Some(0);
    let err = app.serve(listener).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn server_config_defaults() {
    let c = ServerConfig::default();
    assert_eq!(c.shutdown_timeout, Duration::from_secs(30));
    assert_eq!(c.tls_handshake_timeout, Duration::from_secs(10));
    assert_eq!(c.header_read_timeout, Some(Duration::from_secs(30)));
    assert!(c.keep_alive && c.banner);
    assert_eq!(c.max_connections, None);
    assert_eq!(c.http2, cfg!(feature = "http2"));
    assert_eq!(c.runtime, RuntimeMode::default());
    #[cfg(target_os = "linux")]
    assert_eq!(c.runtime, RuntimeMode::PerCore);

    let mut app = App::new()
        .max_connections(0)
        .shutdown_timeout(Duration::from_secs(5))
        .runtime(RuntimeMode::MultiThread)
        .http2(false);
    let c = app.server_config_mut();
    assert_eq!(c.max_connections, Some(1), "clamped to at least one");
    assert_eq!(c.shutdown_timeout, Duration::from_secs(5));
    assert_eq!(c.runtime, RuntimeMode::MultiThread);
    assert!(!c.http2);
}

// ---- unix domain sockets -----------------------------------------------------------

#[cfg(unix)]
#[test]
fn run_unix_serves_and_cleans_up_the_socket() {
    use tokio::net::UnixStream;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.sock");
    // A stale socket file from a dead server is replaced.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let app = quiet(
        App::new()
            .get("/peer", |c: ConnInfo| async move {
                format!("{:?} {}", c.remote_addr, c.secure)
            })
            .workers(2),
    );
    let server_path = path.clone();
    let server = std::thread::spawn(move || {
        app.run_unix_with_shutdown(&server_path, async move {
            let _ = stopped.await;
        })
    });

    client_runtime().block_on(async {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut s = loop {
            match UnixStream::connect(&path).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(e) => panic!("unix server never came up: {e}"),
            }
        };
        let out = get_on(&mut s, "/peer").await;
        assert!(out.ends_with("None false"), "{out}");
        drop(s);
        stop.send(()).unwrap();
    });
    server.join().unwrap().unwrap();
    assert!(!path.exists(), "socket file was not removed");
}

#[cfg(unix)]
#[test]
fn run_unix_refuses_to_replace_a_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-a-socket");
    std::fs::write(&path, "data").unwrap();
    let err = quiet(App::new())
        .run_unix_with_shutdown(&path, std::future::ready(()))
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "data");
}
