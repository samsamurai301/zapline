//! Idempotency keys: a retried `POST` or `PATCH` runs once.
//!
//! A client sends `Idempotency-Key: <unique value>` with an unsafe request.
//! The first request with that key runs the handler and its response is
//! stored; retries with the same key get the stored response back (with
//! `Idempotent-Replayed: true`) instead of running the handler again. This
//! follows the IETF `Idempotency-Key` header draft:
//!
//! | Situation | Answer |
//! |---|---|
//! | First request with a key | runs, response stored for [`ttl`](Idempotency::ttl) |
//! | Retry, same request | the stored response, `Idempotent-Replayed: true` |
//! | Retry while the first still runs | `409`, code `idempotency_key_in_use` |
//! | Same key, different method, path or body | `422`, code `idempotency_key_reused` |
//! | No key on a [`required`](Idempotency::required) route | `400`, code `idempotency_key_missing` |
//!
//! ```
//! use zapline::middleware::Idempotency;
//! use zapline::prelude::*;
//!
//! let app = App::new()
//!     .wrap(Idempotency::new())
//!     .post("/payments", || async { (StatusCode::CREATED, "charged") });
//! ```

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{HeaderName, HeaderValue, AUTHORIZATION, DATE, SET_COOKIE};
use http::{Method, StatusCode};

use crate::body::{Body, ReqBody};
use crate::error::Error;
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::{IntoResponse, Response};

/// The request header carrying the key.
pub const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
/// Set to `true` on a replayed response.
pub const IDEMPOTENT_REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");

/// Responses with larger bodies are passed through but not stored.
const MAX_STORED_BODY: usize = 1024 * 1024;
const MAX_KEY_LEN: usize = 255;
const DEFAULT_MAX_KEYS: usize = 10_000;

type ScopeFn = dyn Fn(&Request) -> Option<String> + Send + Sync;

/// Make retried `POST`/`PATCH` requests run once (see the
/// [module docs](self) for the full behavior).
///
/// Keys are kept in memory by default (bounded by
/// [`max_keys`](Self::max_keys)); use [`redis`](Self::redis) when several
/// instances serve the app. Clones share state.
///
/// What is stored: status, headers (except `Set-Cookie` and `Date`) and
/// body, for complete responses up to 1 MiB. Server errors (`5xx`), `408`,
/// `409`, `425` and `429`, streamed bodies and larger bodies are not stored,
/// so a retry runs the handler again. Wrap [`Compression`] *outside* this
/// middleware so stored bodies stay uncompressed.
///
/// [`Compression`]: crate::middleware::Compression
#[derive(Clone)]
pub struct Idempotency {
    required: bool,
    ttl: Duration,
    lock_timeout: Duration,
    scope: Option<Arc<ScopeFn>>,
    store: Arc<MemStore>,
    #[cfg(feature = "redis")]
    redis: Option<crate::redis::Redis>,
}

impl Default for Idempotency {
    fn default() -> Self {
        Self::new()
    }
}

impl Idempotency {
    /// Idempotency for `POST` and `PATCH` requests that send a key: keys are
    /// optional, responses kept for 24 hours, keys scoped by the
    /// `Authorization` header.
    ///
    /// ```
    /// use zapline::middleware::Idempotency;
    /// let mw = Idempotency::new();
    /// ```
    pub fn new() -> Self {
        Idempotency {
            required: false,
            ttl: Duration::from_secs(24 * 60 * 60),
            lock_timeout: Duration::from_secs(5 * 60),
            scope: None,
            store: Arc::new(MemStore::new(DEFAULT_MAX_KEYS)),
            #[cfg(feature = "redis")]
            redis: None,
        }
    }

    /// Answer `400` (code `idempotency_key_missing`) to a `POST` or `PATCH`
    /// without a key. Use it on route groups whose clients must send one.
    ///
    /// ```
    /// use zapline::middleware::Idempotency;
    /// use zapline::prelude::*;
    ///
    /// let app = App::new()
    ///     .wrap_route(Idempotency::new().required())
    ///     .post("/payments", || async { "charged" });
    /// ```
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// How long a stored response is replayed (default 24 hours).
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::middleware::Idempotency;
    /// let mw = Idempotency::new().ttl(Duration::from_secs(60 * 60));
    /// ```
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl.max(Duration::from_millis(1));
        self
    }

    /// How long a key stays locked by a request that never finishes, e.g.
    /// a crashed instance (default 5 minutes). Keep it above your slowest
    /// handler, or a retry can run while the first request still does.
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::middleware::Idempotency;
    /// let mw = Idempotency::new().lock_timeout(Duration::from_secs(30));
    /// ```
    #[must_use]
    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Who a key belongs to: two clients sending the same key never see
    /// each other's responses. The default is the `Authorization` header;
    /// apps with cookie sessions should scope by user. Returning `None`
    /// skips idempotency for that request.
    ///
    /// ```
    /// use zapline::middleware::Idempotency;
    /// let mw = Idempotency::new().scope(|req| {
    ///     req.headers().get("x-account")?.to_str().ok().map(str::to_owned)
    /// });
    /// ```
    #[must_use]
    pub fn scope<F>(mut self, f: F) -> Self
    where
        F: Fn(&Request) -> Option<String> + Send + Sync + 'static,
    {
        self.scope = Some(Arc::new(f));
        self
    }

    /// Most keys kept in memory (default 10 000). When full, expired keys
    /// are dropped first, then the oldest stored responses.
    ///
    /// ```
    /// use zapline::middleware::Idempotency;
    /// let mw = Idempotency::new().max_keys(100_000);
    /// ```
    #[must_use]
    pub fn max_keys(mut self, n: usize) -> Self {
        self.store = Arc::new(MemStore::new(n.max(1)));
        self
    }

    /// Keep keys and responses in Redis (feature `redis`), so a retry that
    /// reaches another instance is still answered once. If Redis can't be
    /// reached, keyed requests get `503` (code `idempotency_unavailable`)
    /// rather than risking a second run.
    ///
    /// ```
    /// use zapline::middleware::Idempotency;
    /// use zapline::redis::Redis;
    ///
    /// let mw = Idempotency::new().redis(Redis::new("redis://localhost")?);
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[cfg(feature = "redis")]
    #[must_use]
    pub fn redis(mut self, redis: crate::redis::Redis) -> Self {
        self.redis = Some(redis);
        self
    }

    fn backend(&self) -> Backend {
        #[cfg(feature = "redis")]
        if let Some(redis) = &self.redis {
            return Backend::Redis(redis.clone());
        }
        Backend::Memory(self.store.clone())
    }
}

impl Middleware for Idempotency {
    fn handle(&self, mut req: Request, next: Next) -> BoxFuture<Response> {
        if !matches!(*req.method(), Method::POST | Method::PATCH) {
            return next.run(req);
        }
        let key = match req.headers().get(&IDEMPOTENCY_KEY) {
            None if self.required => {
                return ready(
                    Error::bad_request("an Idempotency-Key header is required")
                        .with_code("idempotency_key_missing"),
                );
            }
            None => return next.run(req),
            Some(v) => match parse_key(v) {
                Some(k) => k.to_owned(),
                None => {
                    return ready(
                        Error::bad_request(
                            "Idempotency-Key must be 1 to 255 printable ASCII characters",
                        )
                        .with_code("idempotency_key_invalid"),
                    );
                }
            },
        };
        let scope = match &self.scope {
            Some(f) => match f(&req) {
                Some(s) => s.into_bytes(),
                None => return next.run(req),
            },
            None => req
                .headers()
                .get(AUTHORIZATION)
                .map(|v| v.as_bytes().to_vec())
                .unwrap_or_default(),
        };
        let id = slot_id(&scope, &key);
        let (backend, ttl, lock_timeout) = (self.backend(), self.ttl, self.lock_timeout);
        Box::pin(async move {
            let body = match req.body_bytes().await {
                Ok(b) => b,
                Err(e) => return e.into_response(),
            };
            let fp = fingerprint(&req, &body);
            req.body = Some(ReqBody::Full(body));

            let token = new_token();
            match backend.begin(&id, fp, token, lock_timeout).await {
                Begin::Run => {}
                Begin::Replay(stored) => return stored.replay(),
                Begin::InFlight => {
                    return Error::conflict("a request with this Idempotency-Key is in progress")
                        .with_code("idempotency_key_in_use")
                        .into_response();
                }
                Begin::Mismatch => {
                    return Error::unprocessable(
                        "this Idempotency-Key was used for a different request",
                    )
                    .with_code("idempotency_key_reused")
                    .into_response();
                }
                Begin::Unavailable => {
                    return Error::service_unavailable("idempotency keys are unavailable")
                        .with_code("idempotency_unavailable")
                        .into_response();
                }
            }

            // Releases the key if the handler is cancelled or panics.
            let mut guard = Guard {
                backend: Some(backend),
                id,
                token,
            };
            let res = next.run(req).await;
            let (res, stored) = capture(res, fp);
            if let Some(backend) = guard.backend.take() {
                match stored {
                    Some(stored) => backend.finish(&guard.id, token, stored, ttl).await,
                    // Not storable: free the key before answering, so an
                    // immediate retry runs instead of seeing `409`.
                    None => backend.release(&guard.id, token).await,
                }
            }
            res
        })
    }
}

fn ready(err: Error) -> BoxFuture<Response> {
    Box::pin(std::future::ready(err.into_response()))
}

/// The key from the header: a token, or a quoted string as the draft
/// specifies (`"8e03978e-..."`).
fn parse_key(v: &HeaderValue) -> Option<&str> {
    let s = v.to_str().ok()?.trim();
    let s = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s);
    let ok = (1..=MAX_KEY_LEN).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
    ok.then_some(s)
}

/// A hasher with fixed keys, so every instance hashes alike.
#[allow(deprecated)]
fn stable_hasher() -> std::hash::SipHasher {
    std::hash::SipHasher::new()
}

fn slot_id(scope: &[u8], key: &str) -> String {
    let mut h = stable_hasher();
    scope.hash(&mut h);
    format!("zapline:idempotency:{:016x}:{key}", h.finish())
}

/// What makes two requests "the same": method, path, query and body.
fn fingerprint(req: &Request, body: &[u8]) -> u64 {
    let mut h = stable_hasher();
    req.method().as_str().hash(&mut h);
    req.path().hash(&mut h);
    req.query_string().unwrap_or("").hash(&mut h);
    body.hash(&mut h);
    h.finish()
}

/// A value no other request (on any instance) holds.
fn new_token() -> u128 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static SEED: std::sync::OnceLock<std::collections::hash_map::RandomState> =
        std::sync::OnceLock::new();
    let seed = SEED.get_or_init(Default::default);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let half = |salt: u8| seed.hash_one((salt, n, std::time::SystemTime::now()));
    (u128::from(half(0)) << 64) | u128::from(half(1))
}

/// A response kept for replay.
#[derive(Debug)]
struct Stored {
    fp: u64,
    status: u16,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Bytes,
}

impl Stored {
    fn replay(&self) -> Response {
        let mut res = Response::new(Body::from(self.body.clone()));
        *res.status_mut() = StatusCode::from_u16(self.status).unwrap_or(StatusCode::OK);
        let h = res.headers_mut();
        for (name, value) in &self.headers {
            h.append(name.clone(), value.clone());
        }
        h.insert(IDEMPOTENT_REPLAYED, HeaderValue::from_static("true"));
        res
    }

    /// `D`, fingerprint, status, header count, headers, body.
    #[cfg_attr(not(feature = "redis"), allow(dead_code))]
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.body.len());
        out.push(b'D');
        out.extend_from_slice(&self.fp.to_be_bytes());
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&(self.headers.len() as u32).to_be_bytes());
        for (name, value) in &self.headers {
            for part in [name.as_str().as_bytes(), value.as_bytes()] {
                out.extend_from_slice(&(part.len() as u32).to_be_bytes());
                out.extend_from_slice(part);
            }
        }
        out.extend_from_slice(&self.body);
        out
    }

    #[cfg_attr(not(feature = "redis"), allow(dead_code))]
    fn decode(mut b: &[u8]) -> Option<Stored> {
        fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            let (head, rest) = b.split_at_checked(n)?;
            *b = rest;
            Some(head)
        }
        fn u32_at(b: &mut &[u8]) -> Option<usize> {
            Some(u32::from_be_bytes(take(b, 4)?.try_into().ok()?) as usize)
        }
        if take(&mut b, 1)? != b"D" {
            return None;
        }
        let fp = u64::from_be_bytes(take(&mut b, 8)?.try_into().ok()?);
        let status = u16::from_be_bytes(take(&mut b, 2)?.try_into().ok()?);
        let count = u32_at(&mut b)?;
        let mut headers = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            let n = u32_at(&mut b)?;
            let name = HeaderName::from_bytes(take(&mut b, n)?).ok()?;
            let n = u32_at(&mut b)?;
            let value = HeaderValue::from_bytes(take(&mut b, n)?).ok()?;
            headers.push((name, value));
        }
        Some(Stored {
            fp,
            status,
            headers,
            body: Bytes::copy_from_slice(b),
        })
    }
}

/// Split off a copy of `res` to store, when it may be replayed.
fn capture(res: Response, fp: u64) -> (Response, Option<Stored>) {
    let status = res.status();
    let storable_status = !status.is_server_error()
        && !matches!(status.as_u16(), 408 | 409 | 425 | 429)
        && !status.is_informational();
    if !storable_status {
        return (res, None);
    }
    let (parts, body) = res.into_parts();
    let bytes = match body {
        Body::Empty | Body::Full(None) => Bytes::new(),
        Body::Full(Some(b)) if b.len() <= MAX_STORED_BODY => b,
        other => return (Response::from_parts(parts, other), None),
    };
    let headers = parts
        .headers
        .iter()
        .filter(|(name, _)| **name != SET_COOKIE && **name != DATE)
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    let stored = Stored {
        fp,
        status: status.as_u16(),
        headers,
        body: bytes.clone(),
    };
    (Response::from_parts(parts, Body::from(bytes)), Some(stored))
}

enum Begin {
    /// The key is ours: run the handler.
    Run,
    Replay(Arc<Stored>),
    InFlight,
    Mismatch,
    Unavailable,
}

#[derive(Clone)]
enum Backend {
    Memory(Arc<MemStore>),
    #[cfg(feature = "redis")]
    Redis(crate::redis::Redis),
}

impl Backend {
    async fn begin(&self, id: &str, fp: u64, token: u128, lock: Duration) -> Begin {
        match self {
            Backend::Memory(m) => m.begin(id, fp, token, lock),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => redis_store::begin(r, id, fp, token, lock).await,
        }
    }

    async fn finish(&self, id: &str, token: u128, stored: Stored, ttl: Duration) {
        match self {
            Backend::Memory(m) => m.finish(id, token, stored, ttl),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => redis_store::finish(r, id, token, &stored, ttl).await,
        }
    }

    async fn release(&self, id: &str, token: u128) {
        match self {
            Backend::Memory(m) => m.release(id, token),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => redis_store::release(r, id, token).await,
        }
    }

    /// [`release`](Self::release) from `Drop`, where nothing can be awaited.
    fn release_detached(self, id: String, token: u128) {
        match self {
            Backend::Memory(m) => m.release(&id, token),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(async move { redis_store::release(&r, &id, token).await });
                }
            }
        }
    }
}

/// Frees the key unless the response was stored.
struct Guard {
    backend: Option<Backend>,
    id: String,
    token: u128,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(backend) = self.backend.take() {
            backend.release_detached(std::mem::take(&mut self.id), self.token);
        }
    }
}

enum Slot {
    Running { fp: u64, token: u128 },
    Done(Arc<Stored>),
}

struct Entry {
    slot: Slot,
    until: Instant,
}

struct MemStore {
    map: Mutex<HashMap<String, Entry>>,
    max_keys: usize,
}

impl MemStore {
    fn new(max_keys: usize) -> Self {
        MemStore {
            map: Mutex::new(HashMap::new()),
            max_keys,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn begin(&self, id: &str, fp: u64, token: u128, lock: Duration) -> Begin {
        let now = Instant::now();
        let mut map = self.lock();
        match map.get(id) {
            Some(e) if e.until > now => {
                return match &e.slot {
                    Slot::Running { fp: f, .. } if *f != fp => Begin::Mismatch,
                    Slot::Running { .. } => Begin::InFlight,
                    Slot::Done(s) if s.fp != fp => Begin::Mismatch,
                    Slot::Done(s) => Begin::Replay(s.clone()),
                };
            }
            Some(_) => {
                map.remove(id);
            }
            None => {}
        }
        if map.len() >= self.max_keys {
            map.retain(|_, e| e.until > now);
        }
        if map.len() >= self.max_keys {
            let oldest = map
                .iter()
                .filter(|(_, e)| matches!(e.slot, Slot::Done(_)))
                .min_by_key(|(_, e)| e.until)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    map.remove(&k);
                }
                None => return Begin::Unavailable,
            }
        }
        map.insert(
            id.to_owned(),
            Entry {
                slot: Slot::Running { fp, token },
                until: now + lock,
            },
        );
        Begin::Run
    }

    fn finish(&self, id: &str, token: u128, stored: Stored, ttl: Duration) {
        let mut map = self.lock();
        if let Some(e) = map.get_mut(id) {
            if matches!(e.slot, Slot::Running { token: t, .. } if t == token) {
                e.slot = Slot::Done(Arc::new(stored));
                e.until = Instant::now() + ttl;
            }
        }
    }

    fn release(&self, id: &str, token: u128) {
        let mut map = self.lock();
        if matches!(map.get(id), Some(Entry { slot: Slot::Running { token: t, .. }, .. }) if *t == token)
        {
            map.remove(id);
        }
    }
}

#[cfg(feature = "redis")]
mod redis_store {
    use super::*;
    use crate::redis::{Redis, Reply};

    /// Take the key, or return what holds it. `1` = taken.
    const BEGIN: &str = r#"
if redis.call('SET', KEYS[1], ARGV[1], 'NX', 'PX', ARGV[2]) then return 1 end
return redis.call('GET', KEYS[1])
"#;
    /// Store the response if the key is still ours.
    const FINISH: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  redis.call('SET', KEYS[1], ARGV[2], 'PX', ARGV[3])
  return 1
end
return 0
"#;
    /// Delete the key if our lock still holds it. The lock value is `L`,
    /// 8 fingerprint bytes, then the token: compare from byte 10 on.
    const RELEASE: &str = r#"
local v = redis.call('GET', KEYS[1])
if v and string.sub(v, 1, 1) == 'L' and string.sub(v, 10) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
end
return 0
"#;

    /// `L`, fingerprint, token.
    fn lock_value(fp: u64, token: u128) -> Vec<u8> {
        let mut v = Vec::with_capacity(25);
        v.push(b'L');
        v.extend_from_slice(&fp.to_be_bytes());
        v.extend_from_slice(&token.to_be_bytes());
        v
    }

    fn millis(d: Duration) -> String {
        d.as_millis().clamp(1, u128::from(u64::MAX)).to_string()
    }

    pub(super) async fn begin(
        redis: &Redis,
        id: &str,
        fp: u64,
        token: u128,
        lock: Duration,
    ) -> Begin {
        let value = lock_value(fp, token);
        let ms = millis(lock);
        // A key that expires between SET and GET answers nil: try again.
        for _ in 0..3 {
            let reply = redis
                .cmd([
                    "EVAL".as_bytes(),
                    BEGIN.as_bytes(),
                    b"1",
                    id.as_bytes(),
                    &value,
                    ms.as_bytes(),
                ])
                .await;
            let held = match reply {
                Ok(Reply::Int(1)) => return Begin::Run,
                Ok(Reply::Nil) => continue,
                Ok(Reply::Bulk(b)) => b,
                Ok(other) => {
                    crate::tasks::log_warn(format_args!(
                        "idempotency: unexpected Redis reply {other:?}"
                    ));
                    return Begin::Unavailable;
                }
                Err(e) => {
                    crate::tasks::log_warn(format_args!("idempotency: {}", e.message()));
                    return Begin::Unavailable;
                }
            };
            return match held.first() {
                Some(b'L') => match held.get(1..9).map(|f| f == fp.to_be_bytes()) {
                    Some(true) => Begin::InFlight,
                    _ => Begin::Mismatch,
                },
                Some(b'D') => match Stored::decode(&held) {
                    Some(s) if s.fp == fp => Begin::Replay(Arc::new(s)),
                    Some(_) => Begin::Mismatch,
                    None => {
                        crate::tasks::log_warn(format_args!(
                            "idempotency: unreadable stored response under {id}"
                        ));
                        Begin::Unavailable
                    }
                },
                _ => Begin::Unavailable,
            };
        }
        Begin::Unavailable
    }

    pub(super) async fn finish(redis: &Redis, id: &str, token: u128, s: &Stored, ttl: Duration) {
        let ms = millis(ttl);
        let reply = redis
            .cmd([
                "EVAL".as_bytes(),
                FINISH.as_bytes(),
                b"1",
                id.as_bytes(),
                &lock_value(s.fp, token),
                &s.encode(),
                ms.as_bytes(),
            ])
            .await;
        if let Err(e) = reply {
            crate::tasks::log_warn(format_args!(
                "idempotency: storing the response failed: {}",
                e.message()
            ));
        }
    }

    pub(super) async fn release(redis: &Redis, id: &str, token: u128) {
        let reply = redis
            .cmd([
                "EVAL".as_bytes(),
                RELEASE.as_bytes(),
                b"1",
                id.as_bytes(),
                &token.to_be_bytes(),
            ])
            .await;
        if let Err(e) = reply {
            crate::tasks::log_warn(format_args!(
                "idempotency: releasing a key failed: {}",
                e.message()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_tokens_or_quoted() {
        let p = |s: &str| parse_key(&HeaderValue::from_str(s).unwrap()).map(str::to_owned);
        assert_eq!(p("abc-123").as_deref(), Some("abc-123"));
        assert_eq!(p("\"8e03978e\"").as_deref(), Some("8e03978e"));
        assert_eq!(p(""), None);
        assert_eq!(p("\"\""), None);
        assert_eq!(p("has space"), None);
        assert_eq!(p(&"x".repeat(256)), None);
        assert!(p(&"x".repeat(255)).is_some());
    }

    #[test]
    fn stored_round_trips() {
        let s = Stored {
            fp: 7,
            status: 201,
            headers: vec![(
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            )],
            body: Bytes::from_static(b"{\"id\":1}"),
        };
        let back = Stored::decode(&s.encode()).unwrap();
        assert_eq!((back.fp, back.status, back.body), (7, 201, s.body.clone()));
        assert_eq!(back.headers, s.headers);
        assert!(Stored::decode(&s.encode()[..12]).is_none());
    }

    #[test]
    fn tokens_differ() {
        assert_ne!(new_token(), new_token());
    }
}
