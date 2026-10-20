//! TLS: real handshakes with an rcgen self-signed certificate and a
//! tokio-rustls client.
#![cfg(feature = "tls")]

mod server_support;

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use server_support::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use zapline::prelude::*;
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

fn tls_app(cert: &Cert) -> App {
    App::new()
        .get(
            "/",
            |c: ConnInfo| async move { format!("secure={}", c.secure) },
        )
        .get("/slow", || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "done"
        })
        .tls(TlsConfig::from_pem(&cert.cert_pem, &cert.key_pem).unwrap())
}

async fn connect(
    addr: std::net::SocketAddr,
    cert: &Cert,
    alpn: &[&[u8]],
) -> io::Result<TlsStream<TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der.clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let tcp = TcpStream::connect(addr).await?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
}

#[tokio::test]
async fn https_round_trip_marks_the_connection_secure() {
    let cert = cert();
    let server = TestServer::spawn(tls_app(&cert)).await;
    let mut tls = connect(server.addr, &cert, &[b"http/1.1"]).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    // Keep-alive: two requests on one TLS connection.
    for _ in 0..2 {
        let out = get_on(&mut tls, "/").await;
        assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
        assert!(out.ends_with("secure=true"), "{out}");
    }
}

#[tokio::test]
async fn clients_without_alpn_get_http1() {
    let cert = cert();
    let server = TestServer::spawn(tls_app(&cert)).await;
    let mut tls = connect(server.addr, &cert, &[]).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), None);
    assert!(get_on(&mut tls, "/").await.ends_with("secure=true"));
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn alpn_h2_serves_http2_over_tls() {
    use http_body_util::{BodyExt, Empty};
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let cert = cert();
    let server = TestServer::spawn(tls_app(&cert)).await;
    let tls = connect(server.addr, &cert, &[b"h2", b"http/1.1"])
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));

    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);
    let req = http::Request::get("https://localhost/")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.version(), http::Version::HTTP_2);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, "secure=true");
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn alpn_offers_only_http1_when_http2_is_disabled() {
    let cert = cert();
    let server = TestServer::spawn(tls_app(&cert).http2(false)).await;
    let tls = connect(server.addr, &cert, &[b"h2", b"http/1.1"])
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
}

#[tokio::test]
async fn graceful_shutdown_finishes_in_flight_tls_requests() {
    let cert = cert();
    let mut server = TestServer::spawn(tls_app(&cert)).await;
    let mut tls = connect(server.addr, &cert, &[b"http/1.1"]).await.unwrap();
    tls.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.stop();
    let out = read_to_close(&mut tls).await;
    assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
    assert!(out.ends_with("done"), "{out}");
    server.join().await.unwrap();
}

#[tokio::test]
async fn stalled_handshakes_time_out() {
    let cert = cert();
    let mut app = tls_app(&cert);
    app.server_config_mut().tls_handshake_timeout = Duration::from_millis(200);
    let server = TestServer::spawn(app).await;

    // Connect but never send a ClientHello.
    let mut s = TcpStream::connect(server.addr).await.unwrap();
    let start = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), read_to_close(&mut s))
        .await
        .expect("stalled handshake was never closed");
    assert!(start.elapsed() >= Duration::from_millis(150));

    // The server is unaffected.
    let mut tls = connect(server.addr, &cert, &[]).await.unwrap();
    assert!(get_on(&mut tls, "/").await.ends_with("secure=true"));
}

#[tokio::test]
async fn shutdown_drops_pending_handshakes_without_waiting() {
    let cert = cert();
    let mut server = TestServer::spawn(tls_app(&cert)).await;
    let mut stalled = TcpStream::connect(server.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let start = Instant::now();
    server.stop();
    server.join().await.unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(read_to_close(&mut stalled).await, "");
}

#[tokio::test]
async fn plaintext_http_on_a_tls_port_is_rejected() {
    let cert = cert();
    let server = TestServer::spawn(tls_app(&cert)).await;
    let out = raw(server.addr, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(!out.contains("HTTP/1.1 200"), "{out}");
    let mut tls = connect(server.addr, &cert, &[]).await.unwrap();
    assert!(get_on(&mut tls, "/").await.ends_with("secure=true"));
}

#[tokio::test]
async fn untrusted_clients_fail_the_handshake() {
    let cert = cert();
    let other = self::cert();
    let server = TestServer::spawn(tls_app(&cert)).await;
    // The client only trusts a different certificate.
    let err = connect(server.addr, &other, &[]).await.unwrap_err();
    assert!(err.to_string().contains("certificate"), "{err}");
}

#[test]
fn tls_config_from_pem_files() {
    let cert = cert();
    let dir = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, &cert.cert_pem).unwrap();
    std::fs::write(&key_path, &cert.key_pem).unwrap();
    let config = TlsConfig::from_pem_files(&cert_path, &key_path).unwrap();
    assert!(config.rustls_config().alpn_protocols.is_empty());

    let err = TlsConfig::from_pem_files(dir.path().join("missing.pem"), &key_path).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(err.to_string().contains("missing.pem"), "{err}");
}

#[test]
fn tls_config_rejects_bad_pem() {
    let cert = cert();
    let other = self::cert();
    let bad = |c: &str, k: &str| TlsConfig::from_pem(c, k).unwrap_err();

    let err = bad("not a certificate", &cert.key_pem);
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("no certificate"), "{err}");

    let err = bad(&cert.cert_pem, "not a key");
    assert!(err.to_string().contains("private key"), "{err}");

    // The key must belong to the certificate.
    let err = bad(&cert.cert_pem, &other.key_pem);
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");

    // The key must not leak into Debug output.
    let ok = TlsConfig::from_pem(&cert.cert_pem, &cert.key_pem).unwrap();
    let debug = format!("{ok:?}");
    assert!(debug.starts_with("TlsConfig"), "{debug}");
    assert!(!debug.contains("PRIVATE"), "{debug}");
}

#[tokio::test]
async fn custom_rustls_config_keeps_its_alpn() {
    let cert = cert();
    let base = TlsConfig::from_pem(&cert.cert_pem, &cert.key_pem).unwrap();
    let mut config = (**base.rustls_config()).clone();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let app = App::new()
        .get("/", || async { "custom" })
        .tls(TlsConfig::from_rustls(Arc::new(config)));
    let server = TestServer::spawn(app).await;
    let mut tls = connect(server.addr, &cert, &[b"h2", b"http/1.1"])
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    assert!(get_on(&mut tls, "/").await.ends_with("custom"));
}

#[test]
fn run_with_tls_on_worker_threads() {
    let cert = cert();
    let port = free_port();
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut app = tls_app(&cert).workers(2);
    app.server_config_mut().banner = false;
    let server = std::thread::spawn(move || {
        app.run_with_shutdown(addr, async move {
            let _ = stopped.await;
        })
    });
    client_runtime().block_on(async {
        wait_ready(addr).await;
        let mut tls = connect(addr, &cert, &[]).await.unwrap();
        assert!(get_on(&mut tls, "/").await.ends_with("secure=true"));
        drop(tls);
        stop.send(()).unwrap();
    });
    server.join().unwrap().unwrap();
}
