//! Upgraded WebSocket connections and the server: `max_connections`,
//! `max_upgraded` and graceful shutdown.

mod server_support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use server_support::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message as TsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use zapline::prelude::*;
use zapline::ws::{WebSocket, WebSocketUpgrade};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn echo(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if (msg.is_text() || msg.is_binary()) && socket.send(msg).await.is_err() {
                break;
            }
        }
    })
}

async fn connect(addr: std::net::SocketAddr) -> Client {
    let (mut ws, res) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws")),
    )
    .await
    .expect("handshake timed out")
    .expect("handshake failed");
    assert_eq!(res.status(), 101);
    // A round trip: the socket is upgraded and its task is running.
    ws.send(TsMessage::text("ping")).await.unwrap();
    assert_eq!(next_msg(&mut ws).await, TsMessage::text("ping"));
    ws
}

/// Next data or close message, skipping pings and pongs.
async fn next_msg(ws: &mut Client) -> TsMessage {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a message")
            .expect("stream ended")
            .expect("websocket error");
        if !msg.is_ping() && !msg.is_pong() {
            return msg;
        }
    }
}

/// Read until the socket ends (fails the test after 5s).
async fn to_end(ws: &mut Client) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while ws.next().await.is_some() {}
    })
    .await
    .expect("socket did not end");
}

/// Read until the server's close frame and return its code.
async fn close_code_of(ws: &mut Client) -> CloseCode {
    let wait = async {
        loop {
            match next_msg(ws).await {
                TsMessage::Close(Some(frame)) => return frame.code,
                TsMessage::Close(None) => panic!("close frame without a code"),
                _ => continue,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("no close frame from the server")
}

const HANDSHAKE: &str = "GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";

/// Send a raw handshake and return the response head (and body, if any).
async fn raw_handshake(addr: std::net::SocketAddr) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(HANDSHAKE.as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), read_response(&mut s))
        .await
        .expect("no handshake response")
        .expect("connection closed before a response")
}

// ---- max_connections / max_upgraded --------------------------------------------------

#[tokio::test]
async fn upgraded_websockets_keep_counting_against_max_connections() {
    let app = App::new()
        .get("/ws", echo)
        .get("/", || async { "ok" })
        .max_connections(1);
    let mut server = TestServer::spawn(app).await;
    let mut ws = connect(server.addr).await;

    // The socket still holds the only slot: a new connection waits.
    let mut second = TcpStream::connect(server.addr).await.unwrap();
    second
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let waiting =
        tokio::time::timeout(Duration::from_millis(300), read_response(&mut second)).await;
    assert!(
        waiting.is_err(),
        "served over the limit while a WebSocket was open"
    );

    // Closing the socket frees the slot.
    ws.close(None).await.unwrap();
    to_end(&mut ws).await;
    let served = tokio::time::timeout(Duration::from_secs(5), read_response(&mut second))
        .await
        .expect("never served after the WebSocket closed")
        .unwrap();
    assert!(served.ends_with("ok"), "{served}");

    server.stop();
    drop(second);
    server.join().await.unwrap();
}

#[tokio::test]
async fn max_upgraded_gives_websockets_a_separate_limit() {
    let mut app = App::new()
        .get("/ws", echo)
        .get("/", || async { "ok" })
        .max_connections(1);
    app.server_config_mut().max_upgraded = Some(1);
    let mut server = TestServer::spawn(app).await;
    let mut ws = connect(server.addr).await;

    // The HTTP slot was given back at the upgrade.
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        raw(
            server.addr,
            "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        ),
    )
    .await
    .expect("HTTP connection waited for the WebSocket's slot");
    assert!(out.ends_with("ok"), "{out}");

    // A second socket is over `max_upgraded`: refused before the handshake.
    let refused = raw_handshake(server.addr).await;
    assert!(refused.starts_with("HTTP/1.1 503"), "{refused}");
    assert!(refused.contains("ws_too_many_connections"), "{refused}");
    assert!(
        refused.to_ascii_lowercase().contains("retry-after: 1"),
        "{refused}"
    );

    // Once the first closes, there is room again.
    ws.close(None).await.unwrap();
    to_end(&mut ws).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = raw_handshake(server.addr).await;
        if out.starts_with("HTTP/1.1 101") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the closed socket kept its place: {out}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    server.stop();
    server.join().await.unwrap();
}

#[tokio::test]
async fn max_upgraded_zero_is_rejected() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut app = App::new();
    app.server_config_mut().max_upgraded = Some(0);
    let err = app.serve(listener).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("max_upgraded"), "{err}");
}

// ---- graceful shutdown ---------------------------------------------------------------

#[tokio::test]
async fn shutdown_sends_close_frames_and_waits_for_websockets() {
    let finished = Arc::new(AtomicBool::new(false));
    let done = finished.clone();
    let app = App::new().get("/ws", move |ws: WebSocketUpgrade| {
        let done = done.clone();
        async move {
            ws.on_upgrade(move |mut socket: WebSocket| async move {
                while let Some(Ok(msg)) = socket.recv().await {
                    if msg.is_text() && socket.send(msg).await.is_err() {
                        break;
                    }
                }
                // Cleanup after the close handshake still runs before the
                // server returns.
                tokio::time::sleep(Duration::from_millis(200)).await;
                done.store(true, Ordering::SeqCst);
            })
        }
    });
    let mut server = TestServer::spawn(app).await;
    let mut a = connect(server.addr).await;
    let mut b = connect(server.addr).await;

    let start = Instant::now();
    server.stop();
    assert_eq!(close_code_of(&mut a).await, CloseCode::Away);
    assert_eq!(close_code_of(&mut b).await, CloseCode::Away);
    // tungstenite answered the close frames; the sockets end cleanly.
    to_end(&mut a).await;
    to_end(&mut b).await;

    server.join().await.unwrap();
    assert!(
        finished.load(Ordering::SeqCst),
        "the server returned before the WebSocket task finished"
    );
    // Well within the default 30s shutdown timeout.
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn sending_only_sockets_get_the_close_frame_and_a_ws_closed_error() {
    let error = Arc::new(Mutex::new(None::<String>));
    let seen = error.clone();
    let app = App::new().get("/ws", move |ws: WebSocketUpgrade| {
        let seen = seen.clone();
        async move {
            ws.on_upgrade(move |mut socket: WebSocket| async move {
                // Never reads: a ticker pushing to the client.
                loop {
                    if let Err(e) = socket.send("tick").await {
                        *seen.lock().unwrap() = e.code().map(str::to_owned);
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        }
    });
    let mut server = TestServer::spawn(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", server.addr))
        .await
        .unwrap();
    assert_eq!(next_msg(&mut ws).await, TsMessage::text("tick"));

    server.stop();
    assert_eq!(close_code_of(&mut ws).await, CloseCode::Away);
    to_end(&mut ws).await;
    server.join().await.unwrap();
    assert_eq!(error.lock().unwrap().as_deref(), Some("ws_closed"));
}

#[tokio::test]
async fn shutdown_timeout_drops_websockets_that_ignore_shutdown() {
    let app = App::new()
        .get("/ws", |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|_socket: WebSocket| async move {
                // Holds the socket but never touches it.
                tokio::time::sleep(Duration::from_secs(3600)).await;
            })
        })
        .shutdown_timeout(Duration::from_millis(300));
    let mut server = TestServer::spawn(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", server.addr))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let start = Instant::now();
    server.stop();
    server.join().await.unwrap();
    let took = start.elapsed();
    assert!(
        took >= Duration::from_millis(250),
        "did not wait for the socket: {took:?}"
    );
    assert!(took < Duration::from_secs(5), "{took:?}");
    // The task was dropped, so the socket is gone.
    let end = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("socket still open after the shutdown timeout");
    assert!(!matches!(end, Some(Ok(TsMessage::Text(_)))), "{end:?}");
}

#[tokio::test]
async fn upgrades_are_refused_once_shutdown_starts() {
    let app = App::new().get("/ws", |ws: WebSocketUpgrade| async move {
        // Still deciding when the shutdown starts.
        tokio::time::sleep(Duration::from_millis(300)).await;
        ws.on_upgrade(|_socket: WebSocket| async {})
    });
    let mut server = TestServer::spawn(app).await;
    let pending = tokio::spawn(raw_handshake(server.addr));
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.stop();

    let out = pending.await.unwrap();
    assert!(out.starts_with("HTTP/1.1 503"), "{out}");
    assert!(out.contains("ws_shutting_down"), "{out}");
    server.join().await.unwrap();
}

#[test]
fn per_core_run_drains_websockets_on_every_worker() {
    let port = free_port();
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut app = App::new().get("/ws", echo).workers(2).max_connections(4);
    app.server_config_mut().banner = false;
    let server = std::thread::spawn(move || {
        app.run_with_shutdown(addr, async move {
            let _ = stopped.await;
        })
    });

    let rt = client_runtime();
    rt.block_on(async {
        wait_ready(addr).await;
        let mut sockets = Vec::new();
        for _ in 0..4 {
            sockets.push(connect(addr).await);
        }
        let _ = stop.send(());
        for ws in &mut sockets {
            assert_eq!(close_code_of(ws).await, CloseCode::Away);
            to_end(ws).await;
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !server.is_finished() {
        assert!(
            Instant::now() < deadline,
            "run did not return after the drain"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    server.join().unwrap().unwrap();
}
