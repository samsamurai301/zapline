//! HTTP/3 (experimental): real QUIC connections with an rcgen self-signed
//! certificate and an h3-quinn client, next to HTTPS on TCP.
#![cfg(feature = "http3")]

mod server_support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Buf;
use futures_util::stream;
use serde::{Deserialize, Serialize};
use server_support::*;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use zapline::http3::Http3Config;
use zapline::prelude::*;
use zapline::stream::{BodyStream, StreamBody};
use zapline::tls::rustls::{self, pki_types::ServerName};
use zapline::tls::TlsConfig;

struct Cert {
    cert_pem: String,
    key_pem: String,
    der: rustls::pki_types::CertificateDer<'static>,
}

fn cert() -> Cert {
    let c = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    Cert {
        cert_pem: c.cert.pem(),
        key_pem: c.signing_key.serialize_pem(),
        der: c.cert.der().clone(),
    }
}

fn roots(cert: &Cert) -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der.clone()).unwrap();
    roots
}

#[derive(Deserialize)]
struct NewUser {
    name: String,
    age: u32,
}

#[derive(Serialize)]
struct Created {
    id: u64,
    name: String,
    adult: bool,
}

/// Chunks for `/live`, sent by the test one at a time.
type Feed = Arc<Mutex<Option<mpsc::Receiver<Bytes>>>>;

fn app(cert: &Cert, feed: Feed) -> App {
    App::new()
        .get("/users/{id}", |Path(id): Path<u64>| async move {
            format!("user {id}")
        })
        .post("/users", |Json(u): Json<NewUser>| async move {
            Json(Created {
                id: 7,
                name: u.name,
                adult: u.age >= 18,
            })
        })
        .post("/count", |mut body: BodyStream| async move {
            let mut n = 0;
            while let Some(chunk) = body.chunk().await? {
                n += chunk.len();
            }
            Ok::<_, Error>(n.to_string())
        })
        .get("/info", |req: Request| async move {
            format!(
                "{:?} secure={} remote={}",
                req.head().version,
                req.conn().secure,
                req.conn().remote_addr.is_some()
            )
        })
        .get("/live", |State(feed): State<Feed>| async move {
            let rx = feed.lock().await.take().expect("one /live request");
            let chunks = stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|c| (Ok::<_, std::io::Error>(c), rx))
            });
            StreamBody::new(chunks).content_type("text/plain")
        })
        .get("/slow", || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "done"
        })
        .get("/custom-alt-svc", || async {
            (
                [(header::ALT_SVC, HeaderValue::from_static("clear"))],
                "mine",
            )
        })
        .state(feed)
        .body_limit(1024)
        .tls(TlsConfig::from_pem(&cert.cert_pem, &cert.key_pem).unwrap())
}

fn h3_app(cert: &Cert) -> App {
    app(cert, Feed::default()).http3(Http3Config::new())
}

/// An HTTP/3 client connection. Keep `_endpoint` alive while using it.
struct H3Client {
    send: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    _endpoint: quinn::Endpoint,
}

async fn h3_connect(addr: SocketAddr, cert: &Cert) -> H3Client {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots(cert))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic)));
    let conn = tokio::time::timeout(
        Duration::from_secs(5),
        endpoint.connect(addr, "localhost").unwrap(),
    )
    .await
    .expect("QUIC handshake timed out")
    .expect("QUIC handshake failed");
    let (mut driver, send) = h3::client::new(h3_quinn::Connection::new(conn))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });
    H3Client {
        send,
        _endpoint: endpoint,
    }
}

struct H3Response {
    status: StatusCode,
    version: http::Version,
    headers: HeaderMap,
    body: String,
}

impl H3Client {
    async fn send(&mut self, method: Method, path: &str, body: &[u8]) -> H3Response {
        let mut req = http::Request::builder()
            .method(method)
            .uri(format!("https://localhost{path}"));
        if !body.is_empty() {
            req = req
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, body.len());
        }
        let mut stream = self.send.send_request(req.body(()).unwrap()).await.unwrap();
        if !body.is_empty() {
            stream
                .send_data(Bytes::copy_from_slice(body))
                .await
                .unwrap();
        }
        stream.finish().await.unwrap();
        let res = stream.recv_response().await.unwrap();
        let mut out = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.unwrap() {
            out.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
        }
        H3Response {
            status: res.status(),
            version: res.version(),
            headers: res.headers().clone(),
            body: String::from_utf8_lossy(&out).into_owned(),
        }
    }

    async fn get(&mut self, path: &str) -> H3Response {
        self.send(Method::GET, path, b"").await
    }
}

/// An HTTP/1.1-over-TLS request; returns the raw response.
async fn https_get(addr: SocketAddr, cert: &Cert, path: &str) -> String {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots(cert))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    get_on(&mut tls, path).await
}

fn header_line<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    raw.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

#[tokio::test]
async fn get_with_path_params_over_h3() {
    let cert = cert();
    let server = TestServer::spawn(h3_app(&cert)).await;
    let mut client = h3_connect(server.addr, &cert).await;

    let res = client.get("/users/42").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.version, http::Version::HTTP_3);
    assert_eq!(res.body, "user 42");
    assert_eq!(res.headers[header::CONTENT_LENGTH], "7");
    assert!(
        !res.headers.contains_key(header::ALT_SVC),
        "HTTP/3 responses are not advertised again"
    );

    // Same connection, more requests; unknown routes are the usual 404.
    let res = client.get("/info").await;
    assert_eq!(res.body, "HTTP/3.0 secure=true remote=true");
    let res = client.get("/nope").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = client.send(Method::HEAD, "/users/1", b"").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body, "", "HEAD has no body");
}

#[tokio::test]
async fn post_json_body_over_h3() {
    let cert = cert();
    let server = TestServer::spawn(h3_app(&cert)).await;
    let mut client = h3_connect(server.addr, &cert).await;

    let res = client
        .send(Method::POST, "/users", br#"{"name":"ada","age":36}"#)
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(res.body, r#"{"id":7,"name":"ada","adult":true}"#);

    // Extractor rejections and the body limit work as over TCP.
    let res = client.send(Method::POST, "/users", br#"{"name":1}"#).await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", res.body);
    let big = format!(r#"{{"name":"{}","age":1}}"#, "x".repeat(2000));
    let res = client.send(Method::POST, "/users", big.as_bytes()).await;
    assert_eq!(res.status, StatusCode::PAYLOAD_TOO_LARGE);

    // Streaming request bodies.
    let res = client.send(Method::POST, "/count", &[b'a'; 1000]).await;
    assert_eq!(res.body, "1000");
}

#[tokio::test]
async fn streaming_response_over_h3() {
    let cert = cert();
    let feed = Feed::default();
    let (tx, rx) = mpsc::channel::<Bytes>(4);
    *feed.lock().await = Some(rx);
    let server = TestServer::spawn(app(&cert, feed).http3(Http3Config::new())).await;
    let client = h3_connect(server.addr, &cert).await;
    let mut send = client.send.clone();

    let req = http::Request::get("https://localhost/live")
        .body(())
        .unwrap();
    let mut stream = send.send_request(req).await.unwrap();
    stream.finish().await.unwrap();
    let res = stream.recv_response().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/plain");
    assert!(!res.headers().contains_key(header::CONTENT_LENGTH));

    // Each chunk arrives while the handler is still producing the body.
    for chunk in ["one", "two", "three"] {
        tx.send(Bytes::from_static(chunk.as_bytes())).await.unwrap();
        let mut data = tokio::time::timeout(Duration::from_secs(5), stream.recv_data())
            .await
            .expect("chunk not streamed")
            .unwrap()
            .expect("stream ended early");
        assert_eq!(data.copy_to_bytes(data.remaining()), chunk);
    }
    drop(tx);
    let end = tokio::time::timeout(Duration::from_secs(5), stream.recv_data())
        .await
        .unwrap()
        .unwrap();
    assert!(end.is_none(), "the body ends when the stream does");
}

#[tokio::test]
async fn alt_svc_advertises_h3_on_tcp_responses() {
    let cert = cert();
    let server = TestServer::spawn(h3_app(&cert)).await;
    let port = server.addr.port();

    let raw = https_get(server.addr, &cert, "/users/1").await;
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    let expected = format!("h3=\":{port}\"; ma=86400");
    assert_eq!(header_line(&raw, "alt-svc"), Some(expected.as_str()));
    // Error responses (404 from the router) are advertised too.
    let raw = https_get(server.addr, &cert, "/missing").await;
    assert_eq!(header_line(&raw, "alt-svc"), Some(expected.as_str()));
    // A handler's own Alt-Svc wins.
    let raw = https_get(server.addr, &cert, "/custom-alt-svc").await;
    assert_eq!(header_line(&raw, "alt-svc"), Some("clear"));

    // The advertised port really speaks HTTP/3.
    let mut client = h3_connect(server.addr, &cert).await;
    assert_eq!(client.get("/users/9").await.body, "user 9");
}

#[tokio::test]
async fn alt_svc_respects_port_max_age_and_advertise() {
    let cert = cert();
    let udp_port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let h3 = Http3Config::new()
        .port(udp_port)
        .alt_svc_max_age(Duration::from_secs(60));
    let server = TestServer::spawn(app(&cert, Feed::default()).http3(h3)).await;
    let raw = https_get(server.addr, &cert, "/users/1").await;
    let expected = format!("h3=\":{udp_port}\"; ma=60");
    assert_eq!(header_line(&raw, "alt-svc"), Some(expected.as_str()));
    let mut client = h3_connect(SocketAddr::from(([127, 0, 0, 1], udp_port)), &cert).await;
    assert_eq!(client.get("/users/3").await.body, "user 3");

    // advertise(false): HTTP/3 is served, but not announced.
    let quiet = Http3Config::new().advertise(false);
    let server = TestServer::spawn(app(&cert, Feed::default()).http3(quiet)).await;
    let raw = https_get(server.addr, &cert, "/users/1").await;
    assert_eq!(header_line(&raw, "alt-svc"), None, "{raw}");
    let mut client = h3_connect(server.addr, &cert).await;
    assert_eq!(client.get("/users/4").await.body, "user 4");

    // Without HTTP/3 there is nothing to advertise.
    let server = TestServer::spawn(app(&cert, Feed::default())).await;
    let raw = https_get(server.addr, &cert, "/users/1").await;
    assert_eq!(header_line(&raw, "alt-svc"), None, "{raw}");
}

#[tokio::test]
async fn http3_requires_tls() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = App::new()
        .get("/", || async { "x" })
        .http3(Http3Config::new())
        .serve_with_shutdown(listener, std::future::pending())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("TLS"), "{err}");
}

#[tokio::test]
async fn graceful_shutdown_finishes_in_flight_h3_requests() {
    let cert = cert();
    let mut server = TestServer::spawn(h3_app(&cert)).await;
    let mut client = h3_connect(server.addr, &cert).await;
    let mut send = client.send.clone();
    let slow = tokio::spawn(async move {
        let req = http::Request::get("https://localhost/slow")
            .body(())
            .unwrap();
        let mut stream = send.send_request(req).await.unwrap();
        stream.finish().await.unwrap();
        let res = stream.recv_response().await.unwrap();
        let mut data = stream.recv_data().await.unwrap().unwrap();
        (res.status(), data.copy_to_bytes(data.remaining()))
    });
    // Make sure the request is in flight, then stop the server.
    assert_eq!(client.get("/users/1").await.body, "user 1");
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.stop();
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), slow)
        .await
        .expect("in-flight request was not completed")
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "done");
    server.join().await.unwrap();
}

#[test]
fn app_run_serves_h3_next_to_https() {
    let cert = cert();
    let port = free_port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let app = h3_app(&cert).workers(2);
    let server = std::thread::spawn(move || {
        app.run_with_shutdown(addr, async move {
            let _ = stopped.await;
        })
    });
    client_runtime().block_on(async {
        wait_ready(addr).await;
        let raw = https_get(addr, &cert, "/users/5").await;
        let expected = format!("h3=\":{port}\"; ma=86400");
        assert_eq!(header_line(&raw, "alt-svc"), Some(expected.as_str()));
        let mut client = h3_connect(addr, &cert).await;
        let res = client.get("/users/5").await;
        assert_eq!(res.version, http::Version::HTTP_3);
        assert_eq!(res.body, "user 5");
    });
    stop.send(()).unwrap();
    server.join().unwrap().unwrap();
}
