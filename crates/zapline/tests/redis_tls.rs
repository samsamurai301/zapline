//! Redis over TLS (`rediss://`): a private CA, SNI and mutual TLS against a
//! rustls server that answers a few RESP commands, and the cache and live
//! updates against a real `redis-server --tls-port` when one is installed.
#![cfg(feature = "redis-tls")]

use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::rustls::{self, pki_types::pem::PemObject, pki_types::CertificateDer};
use tokio_rustls::rustls::{pki_types::PrivateKeyDer, server::WebPkiClientVerifier};
#[cfg(feature = "cache")]
use zapline::cache::Cache;
use zapline::live::Live;
use zapline::prelude::*;
use zapline::redis::{Redis, Reply};

/// A CA, a server certificate and a client certificate it signed, as PEM
/// files in a temporary directory.
struct Pki {
    dir: tempfile::TempDir,
    ca_pem: String,
}

impl Pki {
    fn new(server_names: &[&str]) -> Pki {
        let dir = tempfile::tempdir().unwrap();
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        ca.distinguished_name
            .push(DnType::CommonName, "zapline test CA");
        let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
        let issuer = Issuer::new(ca, ca_key);

        let names: Vec<String> = server_names.iter().map(|n| n.to_string()).collect();
        let leaf = |names: Vec<String>, usage: ExtendedKeyUsagePurpose, file: &str| {
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(names).unwrap();
            params.extended_key_usages = vec![usage];
            params.distinguished_name.push(DnType::CommonName, file);
            let cert = params.signed_by(&key, &issuer).unwrap();
            std::fs::write(dir.path().join(format!("{file}.crt")), cert.pem()).unwrap();
            std::fs::write(dir.path().join(format!("{file}.key")), key.serialize_pem()).unwrap();
        };
        leaf(names, ExtendedKeyUsagePurpose::ServerAuth, "server");
        leaf(
            vec!["client".into()],
            ExtendedKeyUsagePurpose::ClientAuth,
            "client",
        );
        std::fs::write(dir.path().join("ca.crt"), &ca_pem).unwrap();
        Pki { dir, ca_pem }
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.path().join(file)
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.path(file)).unwrap()
    }

    /// `?ca_file=...` plus `extra`.
    fn query(&self, extra: &str) -> String {
        format!("?ca_file={}{extra}", self.path("ca.crt").display())
    }

    fn client_params(&self) -> String {
        format!(
            "&cert_file={}&key_file={}",
            self.path("client.crt").display(),
            self.path("client.key").display()
        )
    }
}

// ---------------------------------------------------------------------------
// A rustls server speaking just enough RESP

#[derive(Default)]
struct Seen {
    /// The SNI of each handshake.
    sni: Vec<Option<String>>,
    /// Every command, as text.
    commands: Vec<Vec<String>>,
}

/// Serves TLS on a local port and answers `AUTH` (password `pw`), `SELECT`,
/// `PING` and `ECHO`.
async fn resp_server(pki: &Pki, require_client_cert: bool) -> (String, Arc<Mutex<Seen>>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs = CertificateDer::pem_slice_iter(pki.read("server.crt").as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(pki.read("server.key").as_bytes()).unwrap();
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = if require_client_cert {
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(pki.ca_pem.as_bytes()) {
            roots.add(cert.unwrap()).unwrap();
        }
        let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
            .build()
            .unwrap();
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let config = builder.with_single_cert(certs, key).unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let shared = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, seen) = (acceptor.clone(), shared.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let sni = tls.get_ref().1.server_name().map(str::to_owned);
                seen.lock().unwrap().sni.push(sni);
                let _ = serve(BufReader::new(tls), &seen).await;
            });
        }
    });
    (addr, seen)
}

async fn serve<S>(mut stream: BufReader<S>, seen: &Mutex<Seen>) -> io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let n: usize = line.trim_end()[1..].parse().unwrap();
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            line.clear();
            stream.read_line(&mut line).await?;
            let len: usize = line.trim_end()[1..].parse().unwrap();
            let mut buf = vec![0; len + 2];
            stream.read_exact(&mut buf).await?;
            buf.truncate(len);
            args.push(String::from_utf8(buf).unwrap());
        }
        let reply = match args[0].to_ascii_uppercase().as_str() {
            "AUTH" if args.last().map(String::as_str) == Some("pw") => "+OK\r\n".to_owned(),
            "AUTH" => "-WRONGPASS invalid password\r\n".to_owned(),
            "SELECT" => "+OK\r\n".to_owned(),
            "PING" => "+PONG\r\n".to_owned(),
            "ECHO" => format!("${}\r\n{}\r\n", args[1].len(), args[1]),
            other => format!("-ERR unknown command '{other}'\r\n"),
        };
        seen.lock().unwrap().commands.push(args);
        stream.get_mut().write_all(reply.as_bytes()).await?;
        stream.get_mut().flush().await?;
    }
}

fn pong() -> Reply {
    Reply::Status("PONG".into())
}

#[tokio::test]
async fn a_private_ca_and_sni() {
    // The certificate is for `redis.test` only, and we connect by IP.
    let pki = Pki::new(&["redis.test"]);
    let (addr, seen) = resp_server(&pki, false).await;
    let url = format!("rediss://:pw@{addr}/2{}", pki.query("&sni=redis.test"));
    let redis = Redis::new(&url).unwrap();
    assert!(redis.is_tls());
    assert_eq!(redis.cmd(["PING"]).await.unwrap(), pong());
    assert_eq!(
        redis.cmd(["ECHO", "héllo over tls"]).await.unwrap().text(),
        Some("héllo over tls")
    );
    let seen = seen.lock().unwrap();
    // One connection, reused; the name was sent as SNI; AUTH and SELECT
    // ran first.
    assert_eq!(seen.sni, [Some("redis.test".to_owned())]);
    assert_eq!(seen.commands[0], ["AUTH", "pw"]);
    assert_eq!(seen.commands[1], ["SELECT", "2"]);
    assert_eq!(seen.commands[2], ["PING"]);
}

#[tokio::test]
async fn the_ca_can_come_from_memory() {
    let pki = Pki::new(&["redis.test"]);
    let (addr, _) = resp_server(&pki, false).await;
    let redis = Redis::new(&format!("rediss://:pw@{addr}?sni=redis.test"))
        .unwrap()
        .with_ca_pem(&pki.ca_pem)
        .unwrap();
    assert_eq!(redis.cmd(["PING"]).await.unwrap(), pong());
}

#[tokio::test]
async fn an_untrusted_or_misnamed_server_is_refused() {
    let pki = Pki::new(&["redis.test"]);
    let (addr, seen) = resp_server(&pki, false).await;
    let cases = [
        // The Mozilla roots don't know our CA.
        format!("rediss://:pw@{addr}?sni=redis.test"),
        // Trusted CA, but the certificate is not for this name.
        format!("rediss://:pw@{addr}{}", pki.query("&sni=other.test")),
        // Nor for the IP address.
        format!("rediss://:pw@{addr}{}", pki.query("")),
    ];
    for url in cases {
        let err = Redis::new(&url).unwrap().cmd(["PING"]).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE, "{url}");
        assert!(err.message().contains("TLS handshake"), "{url}: {err:?}");
    }
    // Not one command reached the server.
    assert!(seen.lock().unwrap().commands.is_empty());
}

#[tokio::test]
async fn plain_redis_to_a_tls_port_fails_cleanly() {
    let pki = Pki::new(&["redis.test"]);
    let (addr, _) = resp_server(&pki, false).await;
    let err = Redis::new(&format!("redis://{addr}"))
        .unwrap()
        .cmd(["PING"])
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn mutual_tls() {
    let pki = Pki::new(&["redis.test"]);
    let (addr, _) = resp_server(&pki, true).await;
    let base = format!("rediss://:pw@{addr}");

    let without = Redis::new(&format!("{base}{}", pki.query("&sni=redis.test"))).unwrap();
    let err = without.cmd(["PING"]).await.unwrap_err();
    assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);

    let from_files = format!(
        "{base}{}",
        pki.query(&format!("&sni=redis.test{}", pki.client_params()))
    );
    let redis = Redis::new(&from_files).unwrap();
    assert_eq!(redis.cmd(["PING"]).await.unwrap(), pong());

    let from_memory = without
        .with_client_cert_pem(pki.read("client.crt"), pki.read("client.key"))
        .unwrap();
    assert_eq!(from_memory.cmd(["PING"]).await.unwrap(), pong());
}

// ---------------------------------------------------------------------------
// A real redis-server with TLS

/// `redis-server` on a TLS port, requiring a client certificate and the
/// password `s3cret`; killed on drop.
struct RedisServer {
    child: Child,
    port: u16,
}

impl Drop for RedisServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn redis_server(pki: &Pki) -> Option<RedisServer> {
    let port = free_port();
    let path = |f: &str| pki.path(f).display().to_string();
    let child = Command::new("redis-server")
        .args(["--port", "0", "--tls-port", &port.to_string()])
        .args(["--bind", "127.0.0.1", "--save", "", "--appendonly", "no"])
        .args(["--requirepass", "s3cret", "--tls-auth-clients", "yes"])
        .args(["--tls-cert-file", &path("server.crt")])
        .args(["--tls-key-file", &path("server.key")])
        .args(["--tls-ca-cert-file", &path("ca.crt")])
        .args(["--dir", &pki.dir.path().display().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(child) = child else {
        eprintln!("skipped: redis-server is not installed");
        return None;
    };
    let mut server = RedisServer { child, port };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = server.child.try_wait() {
            eprintln!("skipped: redis-server exited ({status}); is it built with TLS?");
            return None;
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Some(server);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("redis-server did not listen on {port} within 10 s");
}

fn real_url(pki: &Pki, server: &RedisServer, db: u32) -> String {
    format!(
        "rediss://:s3cret@127.0.0.1:{}/{db}{}",
        server.port,
        pki.query(&format!("&sni=localhost{}", pki.client_params()))
    )
}

#[tokio::test]
async fn real_redis_over_tls() {
    let pki = Pki::new(&["localhost"]);
    let Some(server) = redis_server(&pki).await else {
        return;
    };
    let redis = Redis::new(&real_url(&pki, &server, 3)).unwrap();

    // Commands, binary-safe values, database selection.
    assert_eq!(
        redis.cmd(["SET", "k", "héllo"]).await.unwrap(),
        Reply::Status("OK".into())
    );
    assert_eq!(redis.cmd(["GET", "k"]).await.unwrap().text(), Some("héllo"));
    let big = "x".repeat(256 * 1024);
    redis.cmd(["SET", "big", big.as_str()]).await.unwrap();
    assert_eq!(
        redis.cmd(["GET", "big"]).await.unwrap().text(),
        Some(big.as_str())
    );
    let other_db = Redis::new(&real_url(&pki, &server, 4)).unwrap();
    assert_eq!(other_db.cmd(["GET", "k"]).await.unwrap(), Reply::Nil);

    // Concurrent commands each take a pooled TLS connection.
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let redis = redis.clone();
            tokio::spawn(async move { redis.cmd(["INCR", "n"]).await.unwrap() })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(redis.cmd(["GET", "n"]).await.unwrap().int(), Some(8));

    // A killed idle connection is replaced by a fresh handshake.
    let id = redis.cmd(["CLIENT", "ID"]).await.unwrap().int().unwrap();
    other_db
        .cmd(["CLIENT", "KILL", "ID", &id.to_string()])
        .await
        .unwrap();
    assert_eq!(redis.cmd(["PING"]).await.unwrap(), pong());

    // The cache works the same over TLS.
    #[cfg(feature = "cache")]
    {
        let cache = Cache::redis(redis.clone());
        cache
            .set("post:1", &"one", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(
            Cache::redis(other_db.clone()).get::<String>("post:1").await,
            None
        );
        assert_eq!(
            Cache::redis(redis.clone())
                .get::<String>("post:1")
                .await
                .as_deref(),
            Some("one")
        );
    }

    // A wrong password or a missing client certificate is refused.
    let wrong = real_url(&pki, &server, 0).replace("s3cret", "nope");
    let err = Redis::new(&wrong).unwrap().cmd(["PING"]).await.unwrap_err();
    assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(err.message().contains("refused"), "{err:?}");
    let no_cert = format!(
        "rediss://:s3cret@127.0.0.1:{}{}",
        server.port,
        pki.query("&sni=localhost")
    );
    let err = Redis::new(&no_cert)
        .unwrap()
        .cmd(["PING"])
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
}

async fn next(sub: &mut zapline::live::Subscription) -> zapline::live::Update {
    tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("an update within 5 s")
        .expect("the hub is open")
}

#[tokio::test]
async fn live_updates_over_tls() {
    let pki = Pki::new(&["localhost"]);
    let Some(server) = redis_server(&pki).await else {
        return;
    };
    let redis = Redis::new(&real_url(&pki, &server, 0)).unwrap();
    let (hub_a, hub_b) = (
        Live::new().redis(redis.clone()),
        Live::new().redis(redis.clone()),
    );
    let a = App::new().live(hub_a.clone()).test();
    let b = App::new().live(hub_b.clone()).test();
    a.start().await;
    b.start().await;
    let mut on_b = hub_b.subscribe("orders");
    // Give the listeners time to subscribe in Redis.
    tokio::time::sleep(Duration::from_millis(300)).await;
    hub_a.publish("orders", "created", "42");
    let update = next(&mut on_b).await;
    assert_eq!(
        (update.event.as_str(), update.data.as_str()),
        ("created", "42")
    );
    a.shutdown().await;
    b.shutdown().await;
}
