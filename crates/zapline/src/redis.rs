//! A small Redis client, and the pieces that use it to share state between
//! instances of an app: [`Cache::redis`](crate::cache::Cache::redis),
//! [`Live::redis`](crate::live::Live::redis) and [`RedisStore`] for
//! sessions.
//!
//! ```no_run
//! use zapline::cache::Cache;
//! use zapline::live::Live;
//! use zapline::prelude::*;
//! use zapline::redis::Redis;
//!
//! # fn main() -> zapline::Result<()> {
//! let redis = Redis::from_env()?; // REDIS_URL, e.g. redis://localhost:6379
//! let app = App::new()
//!     .cache(Cache::redis(redis.clone()))
//!     .live(Live::new().redis(redis.clone()))
//!     .redis(redis);
//! # Ok(()) }
//! ```
//!
//! It speaks RESP2 over TCP (`redis://`) or, with the `redis-tls` feature,
//! over TLS (`rediss://`: rustls with the Mozilla root store or your own
//! CA, and client certificates for mutual TLS). It keeps a few idle
//! connections for reuse, and reconnects on its own. Anything else, from
//! `INCR` to Lua scripts, goes through [`Redis::cmd`].

use std::fmt;
#[cfg(feature = "redis-tls")]
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(feature = "redis-tls")]
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(feature = "redis-tls")]
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::extract::FromRequestParts;
#[cfg(feature = "cookies")]
use crate::session::{SessionRecord, SessionStore};
use crate::{App, Error, Request, Result};

/// Idle connections kept for reuse.
const MAX_IDLE: usize = 16;
/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A Redis server. Cheap to clone (clones share connections); a handler
/// argument once registered with [`App::redis`].
#[derive(Clone)]
pub struct Redis {
    inner: Arc<Inner>,
}

struct Inner {
    addr: String,
    username: Option<String>,
    password: Option<String>,
    db: u32,
    #[cfg(feature = "redis-tls")]
    tls: Option<tls::Tls>,
    idle: Mutex<Vec<Conn>>,
}

impl fmt::Debug for Redis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redis")
            .field("addr", &self.inner.addr)
            .field("db", &self.inner.db)
            .field("tls", &self.is_tls())
            .finish_non_exhaustive()
    }
}

/// A reply from Redis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// A missing value (`nil`).
    Nil,
    /// A status like `OK`.
    Status(String),
    /// An integer.
    Int(i64),
    /// A string (binary-safe).
    Bulk(Vec<u8>),
    /// A list of replies.
    Array(Vec<Reply>),
}

impl Reply {
    /// The reply as text, for strings and statuses.
    ///
    /// ```
    /// use zapline::redis::Reply;
    /// assert_eq!(Reply::Bulk(b"hi".to_vec()).text(), Some("hi"));
    /// assert_eq!(Reply::Nil.text(), None);
    /// ```
    pub fn text(&self) -> Option<&str> {
        match self {
            Reply::Bulk(b) => std::str::from_utf8(b).ok(),
            Reply::Status(s) => Some(s),
            _ => None,
        }
    }

    /// The reply as an integer, for integers and numeric strings.
    ///
    /// ```
    /// use zapline::redis::Reply;
    /// assert_eq!(Reply::Int(3).int(), Some(3));
    /// assert_eq!(Reply::Bulk(b"42".to_vec()).int(), Some(42));
    /// ```
    pub fn int(&self) -> Option<i64> {
        match self {
            Reply::Int(n) => Some(*n),
            other => other.text()?.parse().ok(),
        }
    }
}

impl Redis {
    /// A client for `url`:
    /// `redis[s]://[[user]:password@]host[:port][/db][?param=value&...]`.
    /// Nothing connects until the first command, but a CA or client
    /// certificate file named in the URL is read (and checked) here.
    ///
    /// `rediss://` (TLS, with the `redis-tls` feature) verifies the server
    /// against the Mozilla root store unless the URL names a CA. Its
    /// parameters, with percent-encoded values:
    ///
    /// | Parameter | Meaning |
    /// |---|---|
    /// | `ca_file` | A PEM file of CA certificates to trust instead of the Mozilla roots (a private CA or a self-signed server) |
    /// | `sni` | The name to send and to verify the server's certificate for, when it differs from the host (connecting by IP, through a tunnel) |
    /// | `cert_file`, `key_file` | A PEM client certificate chain and its key, for servers that require mutual TLS |
    ///
    /// # Errors
    ///
    /// When `url` is not a `redis://` or `rediss://` URL, when it is
    /// `rediss://` and the `redis-tls` feature is off, or when a file it
    /// names can't be read or parsed.
    ///
    /// ```
    /// use zapline::redis::Redis;
    /// let redis = Redis::new("redis://:s3cret@cache.internal:6380/2")?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    ///
    /// Over TLS, with a private CA and the name on the server's certificate:
    ///
    /// ```no_run
    /// # #[cfg(feature = "redis-tls")] {
    /// use zapline::redis::Redis;
    /// let redis = Redis::new(
    ///     "rediss://:s3cret@10.0.0.5:6380/0?ca_file=/etc/redis/ca.pem&sni=redis.internal",
    /// )?;
    /// # }
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn new(url: &str) -> Result<Self> {
        let bad = |why: &str| Error::internal(format!("bad Redis URL: {why}"));
        let (secure, rest) = match url.split_once("://") {
            Some(("redis", rest)) => (false, rest),
            Some(("rediss", rest)) => (true, rest),
            _ => return Err(bad("expected redis://host:port or rediss://host:port")),
        };
        #[cfg(not(feature = "redis-tls"))]
        if secure {
            return Err(bad("rediss:// (TLS) needs zapline's `redis-tls` feature: \
                 add features = [\"redis-tls\"] to zapline in Cargo.toml"));
        }
        let (auth, rest) = match rest.rsplit_once('@') {
            Some((auth, rest)) => (Some(auth), rest),
            None => (None, rest),
        };
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (host, db) = match rest.split_once('/') {
            Some((host, "")) => (host, 0),
            Some((host, db)) => (
                host,
                db.parse()
                    .map_err(|_| bad("the database must be a number"))?,
            ),
            None => (rest, 0),
        };
        if host.is_empty() {
            return Err(bad("no host"));
        }
        let addr = if host
            .rsplit_once(':')
            .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
            && !host.ends_with(']')
        {
            host.to_owned()
        } else {
            format!("{host}:6379")
        };
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        };
        let (username, password) = match auth {
            Some(auth) => match auth.split_once(':') {
                Some((user, pass)) => (
                    Some(decode(user)).filter(|u| !u.is_empty()),
                    Some(decode(pass)),
                ),
                None => (None, Some(decode(auth))),
            },
            None => (None, None),
        };
        let mut params = TlsParams::default();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let slot = match key {
                "ca_file" => &mut params.ca_file,
                "sni" => &mut params.sni,
                "cert_file" => &mut params.cert_file,
                "key_file" => &mut params.key_file,
                other => {
                    return Err(bad(&format!(
                        "unknown parameter `{other}` (expected ca_file, sni, cert_file or key_file)"
                    )))
                }
            };
            let value = decode(value);
            if value.is_empty() {
                return Err(bad(&format!("`{key}` needs a value")));
            }
            *slot = Some(value);
        }
        if !secure && !params.is_empty() {
            return Err(bad(
                "ca_file, sni, cert_file and key_file only apply to rediss:// (TLS)",
            ));
        }
        #[cfg(feature = "redis-tls")]
        let tls = if secure {
            Some(tls::Tls::from_params(&addr, params)?)
        } else {
            None
        };
        Ok(Redis {
            inner: Arc::new(Inner {
                addr,
                username,
                password,
                db,
                #[cfg(feature = "redis-tls")]
                tls,
                idle: Mutex::default(),
            }),
        })
    }

    /// A client for `REDIS_URL`.
    ///
    /// # Errors
    ///
    /// When `REDIS_URL` is unset or not a valid URL for [`Redis::new`].
    ///
    /// ```no_run
    /// let redis = zapline::redis::Redis::from_env()?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("REDIS_URL")
            .map_err(|_| Error::internal("REDIS_URL is not set (e.g. redis://localhost:6379)"))?;
        Self::new(&url)
    }

    /// Trust the CA certificates in `ca_pem` instead of the Mozilla roots
    /// (or a `ca_file` from the URL), for a CA that lives in a secret
    /// rather than a file. Only for `rediss://` clients, before they are
    /// cloned.
    ///
    /// # Errors
    ///
    /// When this is not a `rediss://` client or `ca_pem` holds no valid
    /// certificate.
    ///
    /// ```no_run
    /// use zapline::redis::Redis;
    /// let ca = std::env::var("REDIS_CA_PEM").unwrap_or_default();
    /// let redis = Redis::new("rediss://:s3cret@redis.internal:6380")?.with_ca_pem(ca)?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[cfg(feature = "redis-tls")]
    pub fn with_ca_pem(self, ca_pem: impl AsRef<[u8]>) -> Result<Self> {
        let ca = ca_pem.as_ref().to_vec();
        self.with_tls(|settings| settings.ca = Some(ca))
    }

    /// Present the client certificate chain `cert_pem` (leaf first) and its
    /// private key `key_pem` (PKCS#8, PKCS#1 or SEC1), for servers that
    /// require mutual TLS; the in-memory form of the `cert_file` and
    /// `key_file` URL parameters. Only for `rediss://` clients, before they
    /// are cloned.
    ///
    /// # Errors
    ///
    /// When this is not a `rediss://` client or the PEM can't be parsed.
    ///
    /// ```no_run
    /// use zapline::redis::Redis;
    /// let cert = std::env::var("REDIS_CLIENT_CERT").unwrap_or_default();
    /// let key = std::env::var("REDIS_CLIENT_KEY").unwrap_or_default();
    /// let redis = Redis::new("rediss://redis.internal:6380")?.with_client_cert_pem(cert, key)?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[cfg(feature = "redis-tls")]
    pub fn with_client_cert_pem(
        self,
        cert_pem: impl AsRef<[u8]>,
        key_pem: impl AsRef<[u8]>,
    ) -> Result<Self> {
        let client = (cert_pem.as_ref().to_vec(), key_pem.as_ref().to_vec());
        self.with_tls(|settings| settings.client = Some(client))
    }

    /// The same client with changed TLS settings (and no shared idle
    /// connections).
    #[cfg(feature = "redis-tls")]
    fn with_tls(self, change: impl FnOnce(&mut tls::Settings)) -> Result<Self> {
        let Some(tls) = &self.inner.tls else {
            return Err(Error::internal(
                "Redis TLS settings only apply to rediss:// URLs",
            ));
        };
        let mut settings = tls.settings.clone();
        change(&mut settings);
        let inner = &self.inner;
        Ok(Redis {
            inner: Arc::new(Inner {
                addr: inner.addr.clone(),
                username: inner.username.clone(),
                password: inner.password.clone(),
                db: inner.db,
                tls: Some(tls::Tls::new(settings)?),
                idle: Mutex::default(),
            }),
        })
    }

    /// Run one command. A Redis error reply becomes an `Err`.
    ///
    /// # Errors
    ///
    /// When Redis can't be reached (`503`) or answers with an error.
    ///
    /// ```no_run
    /// # async fn run(redis: zapline::redis::Redis) -> zapline::Result<()> {
    /// let visits = redis.cmd(["INCR", "visits"]).await?.int();
    /// redis.cmd(["SET", "greeting", "hello", "EX", "60"]).await?;
    /// # Ok(()) }
    /// ```
    pub async fn cmd<I, A>(&self, args: I) -> Result<Reply>
    where
        I: IntoIterator<Item = A>,
        A: AsRef<[u8]>,
    {
        let frame = encode(args);
        let reused = self.take_idle();
        let fresh = reused.is_none();
        let mut conn = match reused {
            Some(conn) => conn,
            None => self.connect().await?,
        };
        let reply = match conn.send(&frame).await {
            Ok(reply) => reply,
            // An idle connection the server closed: try once more on a new one.
            Err(Failure::Io(_)) if !fresh => {
                conn = self.connect().await?;
                conn.send(&frame)
                    .await
                    .map_err(|f| f.into_error(&self.inner.addr))?
            }
            Err(f) => return Err(f.into_error(&self.inner.addr)),
        };
        self.put_idle(conn);
        reply.map_err(|msg| Error::internal(format!("Redis: {msg}")))
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Conn>> {
        self.inner.idle.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn take_idle(&self) -> Option<Conn> {
        self.lock().pop()
    }

    fn put_idle(&self, conn: Conn) {
        let mut idle = self.lock();
        if idle.len() < MAX_IDLE {
            idle.push(conn);
        }
    }

    /// Whether this client connects over TLS (`rediss://`).
    ///
    /// ```
    /// use zapline::redis::Redis;
    /// assert!(!Redis::new("redis://localhost")?.is_tls());
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn is_tls(&self) -> bool {
        #[cfg(feature = "redis-tls")]
        return self.inner.tls.is_some();
        #[cfg(not(feature = "redis-tls"))]
        false
    }

    /// TCP, then the TLS handshake for `rediss://`.
    async fn open(&self) -> Result<Stream> {
        let addr = &self.inner.addr;
        let tcp = TcpStream::connect(addr).await.map_err(|e| {
            Error::service_unavailable(format!("cannot reach Redis at {addr}")).with_source(e)
        })?;
        let _ = tcp.set_nodelay(true);
        #[cfg(feature = "redis-tls")]
        return match &self.inner.tls {
            Some(tls) => tls.connect(addr, tcp).await,
            None => Ok(Stream::Plain(tcp)),
        };
        #[cfg(not(feature = "redis-tls"))]
        Ok(tcp)
    }

    async fn connect(&self) -> Result<Conn> {
        let addr = &self.inner.addr;
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, self.open())
            .await
            .map_err(|_| {
                Error::service_unavailable(format!("timed out connecting to Redis at {addr}"))
            })??;
        let mut conn = Conn {
            stream: BufReader::new(stream),
        };
        let mut setup: Vec<Vec<Vec<u8>>> = Vec::new();
        if let Some(password) = &self.inner.password {
            let mut auth = vec![b"AUTH".to_vec()];
            if let Some(user) = &self.inner.username {
                auth.push(user.as_bytes().to_vec());
            }
            auth.push(password.as_bytes().to_vec());
            setup.push(auth);
        }
        if self.inner.db != 0 {
            setup.push(vec![
                b"SELECT".to_vec(),
                self.inner.db.to_string().into_bytes(),
            ]);
        }
        for args in setup {
            match conn.send(&encode(&args)).await {
                Ok(Ok(_)) => {}
                Ok(Err(msg)) => {
                    return Err(Error::service_unavailable(format!(
                        "Redis refused the connection: {msg}"
                    )))
                }
                Err(f) => return Err(f.into_error(addr)),
            }
        }
        Ok(conn)
    }

    /// A dedicated connection subscribed to `pattern` (`PSUBSCRIBE`).
    pub(crate) async fn psubscribe(&self, pattern: &str) -> Result<PubSub> {
        let mut conn = self.connect().await?;
        conn.write(&encode(["PSUBSCRIBE", pattern]))
            .await
            .map_err(|f| f.into_error(&self.inner.addr))?;
        Ok(PubSub { conn })
    }
}

/// Messages from a pattern subscription.
pub(crate) struct PubSub {
    conn: Conn,
}

impl PubSub {
    /// The next `(channel, payload)`, skipping subscription confirmations.
    pub(crate) async fn next(&mut self) -> std::io::Result<(String, Vec<u8>)> {
        loop {
            if let Reply::Array(parts) = self.conn.read().await?.unwrap_or(Reply::Nil) {
                if let [Reply::Bulk(kind), _, Reply::Bulk(channel), Reply::Bulk(payload)] =
                    &parts[..]
                {
                    if kind == b"pmessage" {
                        return Ok((
                            String::from_utf8_lossy(channel).into_owned(),
                            payload.clone(),
                        ));
                    }
                }
            }
        }
    }
}

struct Conn {
    stream: BufReader<Stream>,
}

/// The `ca_file`, `sni`, `cert_file` and `key_file` URL parameters.
#[derive(Default)]
struct TlsParams {
    ca_file: Option<String>,
    sni: Option<String>,
    cert_file: Option<String>,
    key_file: Option<String>,
}

impl TlsParams {
    fn is_empty(&self) -> bool {
        self.ca_file.is_none()
            && self.sni.is_none()
            && self.cert_file.is_none()
            && self.key_file.is_none()
    }
}

#[cfg(not(feature = "redis-tls"))]
type Stream = TcpStream;

/// A connection to Redis: plain TCP, or TLS for `rediss://`.
#[cfg(feature = "redis-tls")]
enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

#[cfg(feature = "redis-tls")]
impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

#[cfg(feature = "redis-tls")]
impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

enum Failure {
    Io(std::io::Error),
}

impl Failure {
    fn into_error(self, addr: &str) -> Error {
        let Failure::Io(e) = self;
        Error::service_unavailable(format!("lost the connection to Redis at {addr}")).with_source(e)
    }
}

impl Conn {
    /// Send a frame and read its reply: `Ok(Err(msg))` is an error reply.
    async fn send(
        &mut self,
        frame: &[u8],
    ) -> std::result::Result<std::result::Result<Reply, String>, Failure> {
        self.write(frame).await?;
        self.read().await.map_err(Failure::Io)
    }

    async fn write(&mut self, frame: &[u8]) -> std::result::Result<(), Failure> {
        let stream = self.stream.get_mut();
        stream.write_all(frame).await.map_err(Failure::Io)?;
        stream.flush().await.map_err(Failure::Io)
    }

    /// One reply. Error replies nested in arrays become `Nil`.
    async fn read(&mut self) -> std::io::Result<std::result::Result<Reply, String>> {
        let mut line = String::new();
        if self.stream.read_line(&mut line).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad Redis reply");
        let (kind, rest) = line.split_at_checked(1).ok_or_else(invalid)?;
        let number = || rest.parse::<i64>().map_err(|_| invalid());
        Ok(Ok(match kind {
            "+" => Reply::Status(rest.to_owned()),
            "-" => return Ok(Err(rest.to_owned())),
            ":" => Reply::Int(number()?),
            "$" => match number()? {
                -1 => Reply::Nil,
                n => {
                    let n = usize::try_from(n).map_err(|_| invalid())?;
                    let mut buf = vec![0; n + 2];
                    self.stream.read_exact(&mut buf).await?;
                    buf.truncate(n);
                    Reply::Bulk(buf)
                }
            },
            "*" => match number()? {
                -1 => Reply::Nil,
                n => {
                    let mut items = Vec::with_capacity(n.clamp(0, 1024) as usize);
                    for _ in 0..n {
                        items.push(Box::pin(self.read()).await?.unwrap_or(Reply::Nil));
                    }
                    Reply::Array(items)
                }
            },
            _ => return Err(invalid()),
        }))
    }
}

fn encode<I, A>(args: I) -> Vec<u8>
where
    I: IntoIterator<Item = A>,
    A: AsRef<[u8]>,
{
    let args: Vec<A> = args.into_iter().collect();
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for arg in &args {
        let arg = arg.as_ref();
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

impl FromRequestParts for Redis {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Redis>().cloned().ok_or_else(|| {
            Error::internal("no Redis registered: add `.redis(Redis::from_env()?)` to the app")
                .diagnosed("ZL0072", None)
        })
    }
}

impl App {
    /// Register `redis` for handlers ([`Redis`] extractor), jobs and
    /// startup hooks.
    ///
    /// ```
    /// # use zapline::redis::Redis;
    /// let app = zapline::App::new().redis(Redis::new("redis://localhost")?);
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn redis(self, redis: Redis) -> Self {
        self.state(redis)
    }
}

// ---------------------------------------------------------------------------
// TLS

#[cfg(feature = "redis-tls")]
mod tls {
    use std::sync::Arc;

    use tokio::net::TcpStream;
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};
    use tokio_rustls::TlsConnector;

    use super::{Stream, TlsParams};
    use crate::{Error, Result};

    fn invalid(why: String) -> Error {
        Error::internal(format!("Redis TLS: {why}"))
    }

    /// What a `rediss://` client trusts and presents.
    #[derive(Clone)]
    pub(super) struct Settings {
        /// Sent as SNI (for names; not for IP addresses) and checked
        /// against the server's certificate.
        pub(super) server_name: ServerName<'static>,
        /// PEM CA certificates replacing the Mozilla roots.
        pub(super) ca: Option<Vec<u8>>,
        /// PEM client certificate chain and key, for mutual TLS.
        pub(super) client: Option<(Vec<u8>, Vec<u8>)>,
    }

    /// Settings and the rustls connector built from them once.
    pub(super) struct Tls {
        pub(super) settings: Settings,
        connector: TlsConnector,
    }

    impl Tls {
        /// From the URL's host and its `ca_file`, `sni`, `cert_file` and
        /// `key_file` parameters; files are read now.
        pub(super) fn from_params(addr: &str, params: TlsParams) -> Result<Self> {
            let name = params.sni.unwrap_or_else(|| {
                let host = addr.rsplit_once(':').map_or(addr, |(host, _)| host);
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_owned()
            });
            let server_name = ServerName::try_from(name.clone())
                .map_err(|_| invalid(format!("`{name}` is not a valid server name")))?;
            let read = |path: &str, what: &str| {
                std::fs::read(path).map_err(|e| {
                    invalid(format!("cannot read the {what} {path}: {e}")).with_source(e)
                })
            };
            let ca = match &params.ca_file {
                Some(path) => Some(read(path, "CA file")?),
                None => None,
            };
            let client = match (&params.cert_file, &params.key_file) {
                (Some(cert), Some(key)) => {
                    Some((read(cert, "client certificate")?, read(key, "client key")?))
                }
                (None, None) => None,
                _ => return Err(invalid("cert_file and key_file go together".into())),
            };
            Self::new(Settings {
                server_name,
                ca,
                client,
            })
        }

        pub(super) fn new(settings: Settings) -> Result<Self> {
            let roots = match &settings.ca {
                Some(pem) => {
                    let mut roots = RootCertStore::empty();
                    for cert in CertificateDer::pem_slice_iter(pem) {
                        let cert = cert.map_err(|e| invalid(format!("invalid CA PEM: {e}")))?;
                        roots
                            .add(cert)
                            .map_err(|e| invalid(format!("invalid CA certificate: {e}")))?;
                    }
                    if roots.is_empty() {
                        return Err(invalid("no certificate found in the CA PEM".into()));
                    }
                    roots
                }
                None => RootCertStore {
                    roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                },
            };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let builder = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| invalid(e.to_string()))?
                .with_root_certificates(roots);
            let config = match &settings.client {
                Some((cert_pem, key_pem)) => {
                    let certs = CertificateDer::pem_slice_iter(cert_pem)
                        .collect::<std::result::Result<Vec<_>, _>>()
                        .map_err(|e| invalid(format!("invalid client certificate PEM: {e}")))?;
                    if certs.is_empty() {
                        return Err(invalid(
                            "no certificate found in the client certificate PEM".into(),
                        ));
                    }
                    let key = PrivateKeyDer::from_pem_slice(key_pem)
                        .map_err(|e| invalid(format!("invalid client key PEM: {e}")))?;
                    builder
                        .with_client_auth_cert(certs, key)
                        .map_err(|e| invalid(format!("unusable client certificate: {e}")))?
                }
                None => builder.with_no_client_auth(),
            };
            Ok(Tls {
                settings,
                connector: TlsConnector::from(Arc::new(config)),
            })
        }

        /// The TLS handshake over `tcp`.
        pub(super) async fn connect(&self, addr: &str, tcp: TcpStream) -> Result<Stream> {
            match self
                .connector
                .connect(self.settings.server_name.clone(), tcp)
                .await
            {
                Ok(stream) => Ok(Stream::Tls(Box::new(stream))),
                Err(e) => Err(Error::service_unavailable(format!(
                    "TLS handshake with Redis at {addr} failed: {e}"
                ))
                .with_source(e)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sessions

#[cfg(feature = "cookies")]
pub use sessions::RedisStore;

#[cfg(feature = "cookies")]
mod sessions {
    use std::time::SystemTime;

    use super::*;

    /// Sessions in Redis, so every instance of the app sees them and they
    /// survive restarts. Keys are `zapline:session:{id}` and expire with the
    /// session.
    ///
    /// ```
    /// use zapline::redis::{Redis, RedisStore};
    /// use zapline::session::SessionLayer;
    ///
    /// let layer = SessionLayer::new(RedisStore::new(Redis::new("redis://localhost")?));
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[derive(Clone, Debug)]
    pub struct RedisStore {
        redis: Redis,
    }

    impl RedisStore {
        /// Store sessions in `redis`.
        pub fn new(redis: Redis) -> Self {
            RedisStore { redis }
        }
    }

    const SESSION_PREFIX: &str = "zapline:session:";

    impl SessionStore for RedisStore {
        async fn load(&self, id: &str) -> Result<Option<SessionRecord>> {
            let reply = self
                .redis
                .cmd(["GET".as_bytes(), format!("{SESSION_PREFIX}{id}").as_bytes()])
                .await?;
            let Reply::Bulk(bytes) = reply else {
                return Ok(None);
            };
            let Ok((data, expires)) =
                serde_json::from_slice::<(serde_json::Map<String, serde_json::Value>, u64)>(&bytes)
            else {
                return Ok(None);
            };
            let expires_at = SystemTime::UNIX_EPOCH + Duration::from_secs(expires);
            Ok((expires_at > SystemTime::now()).then_some(SessionRecord { data, expires_at }))
        }

        async fn save(&self, id: &str, record: &SessionRecord) -> Result<()> {
            let expires = record
                .expires_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let ttl = record
                .expires_at
                .duration_since(SystemTime::now())
                .unwrap_or_default()
                .as_millis()
                .max(1);
            let value = serde_json::to_vec(&(&record.data, expires.as_secs()))
                .map_err(|e| Error::internal("cannot store the session").with_source(e))?;
            self.redis
                .cmd([
                    b"SET".as_slice(),
                    format!("{SESSION_PREFIX}{id}").as_bytes(),
                    &value,
                    b"PX",
                    ttl.to_string().as_bytes(),
                ])
                .await?;
            Ok(())
        }

        async fn delete(&self, id: &str) -> Result<()> {
            self.redis
                .cmd(["DEL".as_bytes(), format!("{SESSION_PREFIX}{id}").as_bytes()])
                .await?;
            Ok(())
        }
    }
}

/// Delete every key matching `pattern` with `SCAN` + `UNLINK`, in batches.
#[cfg(feature = "cache")]
pub(crate) async fn delete_matching(redis: &Redis, pattern: &str) -> Result<()> {
    let mut cursor = "0".to_owned();
    loop {
        let reply = redis
            .cmd(["SCAN", cursor.as_str(), "MATCH", pattern, "COUNT", "500"])
            .await?;
        let Reply::Array(parts) = reply else {
            return Ok(());
        };
        let [next, Reply::Array(keys)] = &parts[..] else {
            return Ok(());
        };
        let keys: Vec<&[u8]> = keys
            .iter()
            .filter_map(|k| match k {
                Reply::Bulk(b) => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        if !keys.is_empty() {
            redis
                .cmd(std::iter::once(b"UNLINK".as_slice()).chain(keys))
                .await?;
        }
        cursor = next.text().unwrap_or("0").to_owned();
        if cursor == "0" {
            return Ok(());
        }
    }
}

/// `pattern` with Redis glob characters escaped, for `MATCH`.
#[cfg(any(feature = "cache", test))]
pub(crate) fn glob_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let r = Redis::new("redis://localhost").unwrap();
        assert_eq!((r.inner.addr.as_str(), r.inner.db), ("localhost:6379", 0));
        let r = Redis::new("redis://bob:p%40ss@10.0.0.5:6380/3").unwrap();
        assert_eq!(r.inner.addr, "10.0.0.5:6380");
        assert_eq!(r.inner.username.as_deref(), Some("bob"));
        assert_eq!(r.inner.password.as_deref(), Some("p@ss"));
        assert_eq!(r.inner.db, 3);
        let r = Redis::new("redis://:secret@cache/").unwrap();
        assert_eq!(
            (r.inner.username.as_deref(), r.inner.password.as_deref()),
            (None, Some("secret"))
        );
        assert_eq!(r.inner.addr, "cache:6379");
        let r = Redis::new("redis://[::1]:7000").unwrap();
        assert_eq!(r.inner.addr, "[::1]:7000");
        let r = Redis::new("redis://:pw@cache:6380/4?").unwrap();
        assert_eq!((r.inner.addr.as_str(), r.inner.db), ("cache:6380", 4));
        assert!(!r.is_tls());
        for bad in [
            "http://x",
            "redis://",
            "redis://x/db",
            "rediss://",
            "redis://x?sni=y",
            "redis://x?ca_file=/ca.pem",
            "redis://x?verify=no",
        ] {
            assert!(Redis::new(bad).is_err(), "{bad}");
        }
        let err = Redis::new("redis://x?sni=y").unwrap_err();
        assert!(err.message().contains("only apply to rediss://"), "{err:?}");
        let err = Redis::new("redis://x?verify=no").unwrap_err();
        assert!(
            err.message().contains("unknown parameter `verify`"),
            "{err:?}"
        );
    }

    #[cfg(not(feature = "redis-tls"))]
    #[test]
    fn rediss_names_the_feature() {
        let err = Redis::new("rediss://:pw@cache.example.com:6380").unwrap_err();
        assert!(err.message().contains("`redis-tls` feature"), "{err:?}");
    }

    #[cfg(feature = "redis-tls")]
    #[test]
    fn rediss_urls() {
        use tokio_rustls::rustls::pki_types::ServerName;

        let name = |r: &Redis| {
            r.inner
                .tls
                .as_ref()
                .unwrap()
                .settings
                .server_name
                .to_str()
                .into_owned()
        };
        let r = Redis::new("rediss://bob:p%40ss@cache.example.com:6380/2").unwrap();
        assert!(r.is_tls());
        assert_eq!(
            (r.inner.addr.as_str(), r.inner.db),
            ("cache.example.com:6380", 2)
        );
        assert_eq!(r.inner.password.as_deref(), Some("p@ss"));
        assert_eq!(name(&r), "cache.example.com");
        let tls = r.inner.tls.as_ref().unwrap();
        assert!(tls.settings.ca.is_none() && tls.settings.client.is_none());
        assert!(format!("{r:?}").contains("tls: true"));

        // The host without its port, IPv6 brackets removed.
        assert_eq!(name(&Redis::new("rediss://cache").unwrap()), "cache");
        let r = Redis::new("rediss://[::1]:7000").unwrap();
        assert!(matches!(
            r.inner.tls.as_ref().unwrap().settings.server_name,
            ServerName::IpAddress(_)
        ));
        // `sni` overrides it, percent-decoded.
        let r = Redis::new("rediss://10.0.0.5:6380/0?sni=redis%2Einternal").unwrap();
        assert_eq!(name(&r), "redis.internal");

        // Files are read when the client is made.
        let dir = tempfile::tempdir().unwrap();
        let ca = rcgen::generate_simple_self_signed(vec!["redis.internal".to_owned()]).unwrap();
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&ca_path, ca.cert.pem()).unwrap();
        let key_path = dir.path().join("key.pem");
        std::fs::write(&key_path, ca.signing_key.serialize_pem()).unwrap();
        let url = format!(
            "rediss://10.0.0.5?ca_file={}&sni=redis.internal&cert_file={}&key_file={}",
            ca_path.display(),
            ca_path.display(),
            key_path.display()
        );
        let r = Redis::new(&url).unwrap();
        let tls = r.inner.tls.as_ref().unwrap();
        assert_eq!(tls.settings.ca.as_deref(), Some(ca.cert.pem().as_bytes()));
        assert!(tls.settings.client.is_some());

        let missing = dir.path().join("missing.pem");
        let err = Redis::new(&format!("rediss://x?ca_file={}", missing.display())).unwrap_err();
        assert!(err.message().contains("cannot read the CA file"), "{err:?}");
        let not_pem = dir.path().join("not.pem");
        std::fs::write(&not_pem, "hello").unwrap();
        let err = Redis::new(&format!("rediss://x?ca_file={}", not_pem.display())).unwrap_err();
        assert!(err.message().contains("no certificate"), "{err:?}");
        let err = Redis::new(&format!("rediss://x?cert_file={}", ca_path.display())).unwrap_err();
        assert!(err.message().contains("go together"), "{err:?}");
        for bad in [
            "rediss://x?sni=",
            "rediss://x?sni=bad%20name",
            "rediss://x?ca_file",
        ] {
            assert!(Redis::new(bad).is_err(), "{bad}");
        }
    }

    #[cfg(feature = "redis-tls")]
    #[test]
    fn tls_builders() {
        let ca = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let r = Redis::new("rediss://:pw@localhost:6380/3?sni=redis.internal")
            .unwrap()
            .with_ca_pem(ca.cert.pem())
            .unwrap()
            .with_client_cert_pem(ca.cert.pem(), ca.signing_key.serialize_pem())
            .unwrap();
        // Everything else is kept.
        assert_eq!((r.inner.addr.as_str(), r.inner.db), ("localhost:6380", 3));
        assert_eq!(r.inner.password.as_deref(), Some("pw"));
        let tls = r.inner.tls.as_ref().unwrap();
        assert_eq!(tls.settings.server_name.to_str(), "redis.internal");
        assert!(tls.settings.ca.is_some() && tls.settings.client.is_some());

        let plain = Redis::new("redis://localhost").unwrap();
        let err = plain.with_ca_pem(ca.cert.pem()).unwrap_err();
        assert!(err.message().contains("only apply to rediss://"), "{err:?}");
        let tls = || Redis::new("rediss://localhost").unwrap();
        assert!(tls().with_ca_pem("not a pem").is_err());
        assert!(tls().with_client_cert_pem(ca.cert.pem(), "nope").is_err());
    }

    #[test]
    fn frames() {
        assert_eq!(encode(["GET", "k"]), b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n");
        assert_eq!(glob_escape("a*b?[c]\\"), "a\\*b\\?\\[c\\]\\\\");
    }
}
