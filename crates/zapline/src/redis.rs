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
//! It speaks RESP2 over plain TCP (`redis://`), keeps a few idle
//! connections for reuse, and reconnects on its own. TLS (`rediss://`) is
//! not supported yet: run Redis on a private network, or behind a TLS
//! tunnel. Anything else, from `INCR` to Lua scripts, goes through
//! [`Redis::cmd`].

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
    idle: Mutex<Vec<Conn>>,
}

impl fmt::Debug for Redis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redis")
            .field("addr", &self.inner.addr)
            .field("db", &self.inner.db)
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
    /// A client for `url`: `redis://[[user]:password@]host[:port][/db]`.
    /// Nothing connects until the first command.
    ///
    /// # Errors
    ///
    /// When `url` is not a `redis://` URL.
    ///
    /// ```
    /// use zapline::redis::Redis;
    /// let redis = Redis::new("redis://:s3cret@cache.internal:6380/2")?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn new(url: &str) -> Result<Self> {
        let bad = |why: &str| Error::internal(format!("bad Redis URL: {why}"));
        let rest = match url.split_once("://") {
            Some(("redis", rest)) => rest,
            Some(("rediss", _)) => {
                return Err(bad(
                    "rediss:// (TLS) is not supported yet; use redis:// on a private network",
                ))
            }
            _ => return Err(bad("expected redis://host:port")),
        };
        let (auth, rest) = match rest.rsplit_once('@') {
            Some((auth, rest)) => (Some(auth), rest),
            None => (None, rest),
        };
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
        Ok(Redis {
            inner: Arc::new(Inner {
                addr,
                username,
                password,
                db,
                idle: Mutex::default(),
            }),
        })
    }

    /// A client for `REDIS_URL`.
    ///
    /// # Errors
    ///
    /// When `REDIS_URL` is unset or not a `redis://` URL.
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

    async fn connect(&self) -> Result<Conn> {
        let addr = &self.inner.addr;
        let unavailable = |e: std::io::Error| {
            Error::service_unavailable(format!("cannot reach Redis at {addr}")).with_source(e)
        };
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                Error::service_unavailable(format!("timed out connecting to Redis at {addr}"))
            })?
            .map_err(unavailable)?;
        let _ = stream.set_nodelay(true);
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
    stream: BufReader<TcpStream>,
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
        for bad in ["http://x", "rediss://x", "redis://", "redis://x/db"] {
            assert!(Redis::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn frames() {
        assert_eq!(encode(["GET", "k"]), b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n");
        assert_eq!(glob_escape("a*b?[c]\\"), "a\\*b\\?\\[c\\]\\\\");
    }
}
