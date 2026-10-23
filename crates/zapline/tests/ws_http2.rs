//! WebSockets over HTTP/2 (RFC 8441 extended CONNECT), with hyper's HTTP/2
//! client (h2c prior knowledge and ALPN `h2` over TLS) and tungstenite.
#![cfg(all(feature = "ws", feature = "http2"))]

mod server_support;

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo};
use server_support::*;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message as TsMessage;
use tokio_tungstenite::WebSocketStream;
use zapline::prelude::*;
use zapline::ws::{Message, WebSocket, WebSocketUpgrade};

type Sender = SendRequest<Empty<Bytes>>;

fn app() -> App {
    App::new()
        .get("/ws", |ws: WebSocketUpgrade, req: Request| async move {
            let method = req.method().clone();
            ws.protocols(["chat.v2", "chat.v1"]).on_upgrade(
                move |mut socket: WebSocket| async move {
                    let hello = format!("hello {method} {:?}", socket.protocol());
                    if socket.send(Message::text(hello)).await.is_err() {
                        return;
                    }
                    while let Some(Ok(msg)) = socket.recv().await {
                        if (msg.is_text() || msg.is_binary()) && socket.send(msg).await.is_err() {
                            break;
                        }
                    }
                },
            )
        })
        .get("/plain", || async { "plain" })
}

async fn handshake<I>(io: I) -> Sender
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(io))
            .await
            .unwrap();
    tokio::spawn(conn);
    // One ordinary request first, so the server's SETTINGS (with
    // ENABLE_CONNECT_PROTOCOL) have arrived before we use extended CONNECT.
    let res = sender
        .send_request(
            http::Request::get("http://localhost/plain")
                .body(Empty::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.into_body().collect().await.unwrap().to_bytes(), "plain");
    sender
}

/// Open a WebSocket on its own HTTP/2 stream.
async fn connect_ws(
    sender: &mut Sender,
    scheme: &str,
    protocols: Option<&str>,
) -> (
    http::Response<hyper::body::Incoming>,
    Option<WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>>,
) {
    let mut req = http::Request::builder()
        .method(Method::CONNECT)
        .uri(format!("{scheme}://localhost/ws"))
        .header("sec-websocket-version", "13");
    if let Some(p) = protocols {
        req = req.header("sec-websocket-protocol", p);
    }
    let mut req = req.body(Empty::new()).unwrap();
    req.extensions_mut()
        .insert(hyper::ext::Protocol::from_static("websocket"));
    let mut res = sender.send_request(req).await.unwrap();
    if res.status() != StatusCode::OK {
        return (res, None);
    }
    let upgraded = hyper::upgrade::on(&mut res).await.unwrap();
    let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, None).await;
    (res, Some(ws))
}

async fn next_text<S>(ws: &mut S) -> String
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
        if let TsMessage::Text(t) = msg {
            return t.to_string();
        }
    }
}

async fn serve() -> (TestServer, SocketAddr) {
    let server = TestServer::spawn(app()).await;
    let addr = server.addr;
    (server, addr)
}

#[tokio::test]
async fn websocket_over_h2c_echoes() {
    let (_server, addr) = serve().await;
    let mut sender = handshake(TcpStream::connect(addr).await.unwrap()).await;
    let (res, ws) = connect_ws(&mut sender, "http", Some("chat.v1, chat.v2")).await;
    assert_eq!(res.version(), http::Version::HTTP_2);
    assert_eq!(res.headers()["sec-websocket-protocol"], "chat.v2");
    assert!(!res.headers().contains_key("sec-websocket-accept"));
    let mut ws = ws.expect("upgraded");

    // Handlers see the same GET as over HTTP/1.1.
    assert_eq!(next_text(&mut ws).await, "hello GET Some(\"chat.v2\")");
    ws.send(TsMessage::text("over h2")).await.unwrap();
    assert_eq!(next_text(&mut ws).await, "over h2");
    ws.send(TsMessage::binary(vec![1u8, 2, 3])).await.unwrap();
    let bin = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(bin, TsMessage::binary(vec![1u8, 2, 3]));
    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn many_websockets_share_one_h2_connection() {
    let (_server, addr) = serve().await;
    let mut sender = handshake(TcpStream::connect(addr).await.unwrap()).await;
    let mut sockets = Vec::new();
    for _ in 0..3 {
        let (_, ws) = connect_ws(&mut sender, "http", None).await;
        let mut ws = ws.expect("upgraded");
        assert_eq!(next_text(&mut ws).await, "hello GET None");
        sockets.push(ws);
    }
    for (i, ws) in sockets.iter_mut().enumerate().rev() {
        ws.send(TsMessage::text(format!("socket {i}")))
            .await
            .unwrap();
    }
    for (i, ws) in sockets.iter_mut().enumerate() {
        assert_eq!(next_text(ws).await, format!("socket {i}"));
    }
    // Ordinary requests still flow on the same connection.
    let res = sender
        .send_request(
            http::Request::get("http://localhost/plain")
                .body(Empty::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn extended_connect_is_validated() {
    let (_server, addr) = serve().await;
    let mut sender = handshake(TcpStream::connect(addr).await.unwrap()).await;

    // Wrong version: rejected like over HTTP/1.1.
    let mut req = http::Request::builder()
        .method(Method::CONNECT)
        .uri("http://localhost/ws")
        .header("sec-websocket-version", "8")
        .body(Empty::new())
        .unwrap();
    req.extensions_mut()
        .insert(hyper::ext::Protocol::from_static("websocket"));
    let res = sender.send_request(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UPGRADE_REQUIRED);

    // A route that is not a WebSocket handler sees an ordinary GET.
    let mut req = http::Request::builder()
        .method(Method::CONNECT)
        .uri("http://localhost/plain")
        .header("sec-websocket-version", "13")
        .body(Empty::new())
        .unwrap();
    req.extensions_mut()
        .insert(hyper::ext::Protocol::from_static("websocket"));
    let res = sender.send_request(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn websocket_over_h2_with_tls() {
    use std::sync::Arc;
    use zapline::tls::rustls::{self, pki_types::ServerName};
    use zapline::tls::TlsConfig;

    let c = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let tls = TlsConfig::from_pem(c.cert.pem(), c.signing_key.serialize_pem()).unwrap();
    let server = TestServer::spawn(app().tls(tls)).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(c.cert.der().clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));

    let mut sender = handshake(stream).await;
    let (_, ws) = connect_ws(&mut sender, "https", None).await;
    let mut ws = ws.expect("upgraded");
    assert_eq!(next_text(&mut ws).await, "hello GET None");
    ws.send(TsMessage::text("secure")).await.unwrap();
    assert_eq!(next_text(&mut ws).await, "secure");
}
