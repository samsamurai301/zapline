//! WebSocket tests: handshake validation in-process, conversations over a
//! real socket with the tokio-tungstenite client.
#![cfg(all(feature = "ws", feature = "json"))]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as TsCloseFrame;
use tokio_tungstenite::tungstenite::Message as TsMessage;
use zapline::prelude::*;
use zapline::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};

async fn serve(app: App) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

async fn echo(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if (msg.is_text() || msg.is_binary()) && socket.send(msg).await.is_err() {
                break;
            }
        }
    })
}

/// Next data/close message, skipping pings and pongs.
async fn next_msg<S>(ws: &mut S) -> TsMessage
where
    S: futures_util::Stream<Item = Result<TsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
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

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

fn handshake(c: &zapline::test::TestClient, path: &str) -> zapline::test::TestRequest {
    c.get(path)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", KEY)
}

#[tokio::test]
async fn echo_text_and_binary() {
    let addr = serve(App::new().get("/ws", echo)).await;
    let (mut ws, res) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    assert_eq!(res.status(), 101);

    ws.send(TsMessage::text("hello")).await.unwrap();
    assert_eq!(next_msg(&mut ws).await, TsMessage::text("hello"));

    let payload = vec![0u8, 1, 2, 255];
    ws.send(TsMessage::binary(payload.clone())).await.unwrap();
    assert_eq!(next_msg(&mut ws).await, TsMessage::binary(payload));

    // Many messages in a row keep their order.
    for i in 0..50 {
        ws.send(TsMessage::text(i.to_string())).await.unwrap();
    }
    for i in 0..50 {
        assert_eq!(next_msg(&mut ws).await, TsMessage::text(i.to_string()));
    }
}

#[tokio::test]
async fn ping_is_answered_and_delivered() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let app = App::new().get("/ws", move |ws: WebSocketUpgrade| {
        let tx = tx.clone();
        async move {
            ws.on_upgrade(move |mut socket| async move {
                while let Some(Ok(msg)) = socket.recv().await {
                    let _ = tx.send(msg);
                }
            })
        }
    });
    let addr = serve(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();

    ws.send(TsMessage::Ping("are you there".into()))
        .await
        .unwrap();
    let pong = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(pong, TsMessage::Pong("are you there".into()));
    // The server application also sees the ping.
    assert_eq!(
        rx.recv().await.unwrap(),
        Message::Ping("are you there".into())
    );
}

#[tokio::test]
async fn client_initiated_close_handshake() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<Message>>();
    let app = App::new().get("/ws", move |ws: WebSocketUpgrade| {
        let tx = tx.clone();
        async move {
            ws.on_upgrade(move |mut socket| async move {
                loop {
                    let msg = socket.recv().await.map(|m| m.unwrap());
                    let end = msg.is_none();
                    let _ = tx.send(msg);
                    if end {
                        break;
                    }
                }
            })
        }
    });
    let addr = serve(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();

    ws.close(Some(TsCloseFrame {
        code: CloseCode::Normal,
        reason: "bye".into(),
    }))
    .await
    .unwrap();

    // The server sees the close frame, then the end of the stream.
    assert_eq!(
        rx.recv().await.unwrap(),
        Some(Message::Close(Some(CloseFrame {
            code: close_code::NORMAL,
            reason: "bye".into()
        })))
    );
    assert_eq!(rx.recv().await.unwrap(), None);

    // The client gets the echoed close frame and then the stream ends.
    match next_msg(&mut ws).await {
        TsMessage::Close(Some(f)) => assert_eq!(f.code, CloseCode::Normal),
        other => panic!("expected close, got {other:?}"),
    }
    assert!(ws.next().await.is_none());
}

#[tokio::test]
async fn server_initiated_close() {
    let app = App::new().get("/ws", |ws: WebSocketUpgrade| async move {
        ws.on_upgrade(|mut socket| async move {
            socket.send("goodbye").await.unwrap();
            socket
                .close_with(close_code::AWAY, "server shutting down")
                .await
                .unwrap();
            // Drain until the client acknowledges.
            while let Some(Ok(_)) = socket.recv().await {}
        })
    });
    let addr = serve(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    assert_eq!(next_msg(&mut ws).await, TsMessage::text("goodbye"));
    match next_msg(&mut ws).await {
        TsMessage::Close(Some(f)) => {
            assert_eq!(f.code, CloseCode::Away);
            assert_eq!(f.reason, "server shutting down");
        }
        other => panic!("expected close, got {other:?}"),
    }
    // tungstenite replies to the close automatically; the stream then ends.
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn protocol_negotiation_over_socket() {
    let app = App::new().get("/ws", |ws: WebSocketUpgrade| async move {
        let ws = ws.protocols(["v1", "v2"]);
        ws.on_upgrade(|mut socket| async move {
            let proto = socket.protocol().unwrap_or("none").to_owned();
            let _ = socket.send(proto).await;
        })
    });
    let addr = serve(app).await;

    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("sec-websocket-protocol", "v2, v1".parse().unwrap());
    let (mut ws, res) = tokio_tungstenite::connect_async(req).await.unwrap();
    // Server preference order wins.
    assert_eq!(res.headers()["sec-websocket-protocol"], "v1");
    assert_eq!(next_msg(&mut ws).await, TsMessage::text("v1"));

    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("sec-websocket-protocol", "v2".parse().unwrap());
    let (mut ws, res) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(res.headers()["sec-websocket-protocol"], "v2");
    assert_eq!(next_msg(&mut ws).await, TsMessage::text("v2"));
}

#[tokio::test]
async fn protocol_negotiation_in_process() {
    let c = App::new()
        .get("/ws", |ws: WebSocketUpgrade| async move {
            let offered: Vec<String> = ws.requested_protocols().map(str::to_owned).collect();
            let ws = ws.protocols(["graphql-ws"]);
            let selected = ws.selected_protocol().unwrap_or("-").to_owned();
            let mut res = ws.on_upgrade(|_| async {});
            res.headers_mut().insert(
                "x-offered",
                HeaderValue::from_str(&format!("{}|{selected}", offered.join(","))).unwrap(),
            );
            res
        })
        .test();

    // Offered across two headers, no match: no protocol selected.
    let r = handshake(&c, "/ws")
        .header("sec-websocket-protocol", "chat, superchat")
        .header("sec-websocket-protocol", "mqtt")
        .send()
        .await;
    assert_eq!(r.status(), 101);
    assert_eq!(r.header("sec-websocket-protocol"), None);
    assert_eq!(r.header("x-offered"), Some("chat,superchat,mqtt|-"));

    let r = handshake(&c, "/ws")
        .header("sec-websocket-protocol", "mqtt, graphql-ws")
        .send()
        .await;
    assert_eq!(r.header("sec-websocket-protocol"), Some("graphql-ws"));
}

#[tokio::test]
async fn handshake_response_in_process_never_runs_callback() {
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    let c = App::new()
        .get("/ws", move |ws: WebSocketUpgrade| {
            let flag = flag.clone();
            async move {
                ws.on_upgrade(move |_socket| async move {
                    flag.store(true, Ordering::SeqCst);
                })
            }
        })
        .test();

    let r = handshake(&c, "/ws").send().await;
    assert_eq!(r.status(), 101);
    // RFC 6455 section 1.3 example.
    assert_eq!(
        r.header("sec-websocket-accept"),
        Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
    );
    assert_eq!(r.header("upgrade"), Some("websocket"));
    assert_eq!(r.header("connection"), Some("upgrade"));
    assert!(r.bytes().is_empty());

    // Token lists and mixed case are accepted.
    let r = c
        .get("/ws")
        .header("connection", "keep-alive, UPGRADE")
        .header("upgrade", "WebSocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", KEY)
        .send()
        .await;
    assert_eq!(r.status(), 101);

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!ran.load(Ordering::SeqCst));
}

#[tokio::test]
async fn rejects_bad_handshakes() {
    let c = App::new()
        .any("/ws", |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|_| async {})
        })
        .test();

    // Plain GET: not an upgrade request.
    let r = c.get("/ws").send().await;
    assert_eq!(r.status(), 426);
    assert_eq!(r.header("upgrade"), Some("websocket"));
    let body: serde_json::Value = r.json();
    assert_eq!(body["code"], "ws_upgrade_required");

    // Upgrade to something else.
    let r = c
        .get("/ws")
        .header("connection", "upgrade")
        .header("upgrade", "h2c")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", KEY)
        .send()
        .await;
    assert_eq!(r.status(), 426);

    // Connection header without the upgrade token.
    let r = c
        .get("/ws")
        .header("connection", "keep-alive")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", KEY)
        .send()
        .await;
    assert_eq!(r.status(), 426);

    // Wrong or missing version.
    for version in [Some("8"), None] {
        let mut req = c
            .get("/ws")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-key", KEY);
        if let Some(v) = version {
            req = req.header("sec-websocket-version", v);
        }
        let r = req.send().await;
        assert_eq!(r.status(), 426);
        assert_eq!(r.header("sec-websocket-version"), Some("13"));
        assert_eq!(
            r.json::<serde_json::Value>()["code"],
            "ws_unsupported_version"
        );
    }

    // Missing or malformed keys.
    for key in [
        None,
        Some("short"),
        Some("not base64 at all!!!!!!!"),
        Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
    ] {
        let mut req = c
            .get("/ws")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13");
        if let Some(k) = key {
            req = req.header("sec-websocket-key", k);
        }
        let r = req.send().await;
        assert_eq!(r.status(), 400, "key {key:?}");
        assert_eq!(r.json::<serde_json::Value>()["code"], "ws_invalid_key");
    }

    // Not a GET.
    let r = c
        .post("/ws")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", KEY)
        .send()
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(r.json::<serde_json::Value>()["code"], "ws_invalid_method");
}

#[tokio::test]
async fn rejects_plain_http_over_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = serve(App::new().get("/ws", echo)).await;
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 426"), "{out}");
    assert!(out.contains("upgrade: websocket"), "{out}");
}

#[tokio::test]
async fn message_size_limit_closes_connection() {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let app = App::new().get("/ws", move |ws: WebSocketUpgrade| {
        let tx = tx.clone();
        async move {
            ws.max_message_size(16)
                .max_frame_size(16)
                .on_upgrade(move |mut socket| async move {
                    while let Some(msg) = socket.recv().await {
                        match msg {
                            Ok(m) => {
                                let _ = tx.send(format!("ok:{}", m.as_text().unwrap_or("")));
                            }
                            Err(e) => {
                                let _ = tx.send(format!("err:{}", e.code().unwrap_or("")));
                                break;
                            }
                        }
                    }
                })
        }
    });
    let addr = serve(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    ws.send(TsMessage::text("small")).await.unwrap();
    assert_eq!(rx.recv().await.unwrap(), "ok:small");
    ws.send(TsMessage::text("x".repeat(100))).await.unwrap();
    assert_eq!(rx.recv().await.unwrap(), "err:ws_message_too_large");
}

#[tokio::test]
async fn json_messages() {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Move {
        x: i32,
        y: i32,
    }
    let app = App::new().get("/ws", |ws: WebSocketUpgrade| async move {
        ws.on_upgrade(|mut socket| async move {
            while let Some(m) = socket.recv_json::<Move>().await {
                match m {
                    Ok(m) => {
                        let reply = Move {
                            x: m.x * 2,
                            y: m.y * 2,
                        };
                        socket.send_json(&reply).await?;
                    }
                    Err(e) => {
                        socket
                            .send(format!("error:{}", e.code().unwrap_or("")))
                            .await?
                    }
                }
            }
            Ok::<_, Error>(())
        })
    });
    let addr = serve(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    ws.send(TsMessage::text(r#"{"x":1,"y":2}"#)).await.unwrap();
    let reply: Move = serde_json::from_str(next_msg(&mut ws).await.to_text().unwrap()).unwrap();
    assert_eq!(reply, Move { x: 2, y: 4 });
    ws.send(TsMessage::text("not json")).await.unwrap();
    assert_eq!(
        next_msg(&mut ws).await,
        TsMessage::text("error:ws_invalid_json")
    );
}

#[derive(Clone)]
struct Chat(broadcast::Sender<String>);

async fn chat(ws: WebSocketUpgrade, State(chat): State<Chat>) -> Response {
    ws.on_upgrade(move |socket| async move {
        let (mut sink, mut stream) = socket.split();
        let mut rx = chat.0.subscribe();
        let mut forward = tokio::spawn(async move {
            while let Ok(text) = rx.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
        });
        let tx = chat.0.clone();
        let mut read = tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                if let Some(text) = msg.as_text() {
                    let _ = tx.send(text.to_owned());
                }
            }
        });
        tokio::select! {
            _ = &mut forward => read.abort(),
            _ = &mut read => forward.abort(),
        }
    })
}

#[tokio::test]
async fn concurrent_clients_broadcast() {
    let (tx, _) = broadcast::channel(64);
    let addr = serve(App::new().state(Chat(tx.clone())).get("/chat", chat)).await;

    let mut clients = Vec::new();
    for _ in 0..4 {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/chat"))
            .await
            .unwrap();
        clients.push(ws);
    }
    // Wait until every connection has subscribed.
    for _ in 0..100 {
        if tx.receiver_count() == 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(tx.receiver_count(), 4);

    for (i, ws) in clients.iter_mut().enumerate() {
        ws.send(TsMessage::text(format!("hello from {i}")))
            .await
            .unwrap();
    }
    for ws in clients.iter_mut() {
        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(next_msg(ws).await.into_text().unwrap().to_string());
        }
        got.sort();
        assert_eq!(
            got,
            (0..4)
                .map(|i| format!("hello from {i}"))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn message_helpers() {
    let m = Message::text("hi");
    assert!(m.is_text() && !m.is_binary());
    assert_eq!(m.as_text(), Some("hi"));
    assert_eq!(m.as_bytes(), b"hi");
    assert_eq!(m.clone().into_text().as_deref(), Some("hi"));
    assert_eq!(m.into_bytes(), Bytes::from_static(b"hi"));

    let b = Message::from(vec![1u8, 2]);
    assert!(b.is_binary());
    assert_eq!(b.as_text(), None);
    assert_eq!(
        Message::from(&b"ab"[..]),
        Message::binary(Bytes::from_static(b"ab"))
    );
    assert_eq!(Message::from(String::from("s")), Message::text("s"));
    assert!(Message::ping("p").is_ping());
    assert!(Message::pong("p").is_pong());

    let c = Message::close(close_code::POLICY, "nope");
    assert!(c.is_close());
    assert_eq!(c.as_bytes(), b"nope");
    assert_eq!(Message::Close(None).into_bytes(), Bytes::new());

    let j = Message::json(&serde_json::json!({"a": 1})).unwrap();
    assert_eq!(j.as_text(), Some(r#"{"a":1}"#));
    let v: serde_json::Value = j.parse_json().unwrap();
    assert_eq!(v["a"], 1);
    let err = Message::text("{")
        .parse_json::<serde_json::Value>()
        .unwrap_err();
    assert_eq!(err.status(), 422);
}
