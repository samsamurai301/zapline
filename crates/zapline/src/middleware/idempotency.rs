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
//! | Same key, different method, path, query, content type or body | `422`, code `idempotency_key_reused` |
//! | No key on a [`required`](Idempotency::required) route | `400`, code `idempotency_key_missing` |
//!
//! Keys belong to a client: by default its `Authorization` header, or
//! whatever [`scope`](Idempotency::scope) returns. Requests with neither
//! skip idempotency, and so do bodies whose `Content-Length` is over the
//! app's body limit (they stream, under their route's own limit). Other
//! bodies, uploads included, are buffered within the app's body limit, so
//! leave routes taking larger chunked uploads out of this middleware.
//!
//! ```
//! use zapline::middleware::Idempotency;
//! use zapline::prelude::*;
//!
//! let app = App::new()
//!     .wrap(Idempotency::new())
//!     .post("/payments", || async { (StatusCode::CREATED, "charged") });
//! ```

use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, DATE, SET_COOKIE};
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

/// Responses larger than this (body and headers) are passed through but
/// not stored.
const MAX_STORED: usize = 1024 * 1024;
const MAX_KEY_LEN: usize = 255;
const DEFAULT_MAX_KEYS: usize = 10_000;
/// Default memory budget for stored responses (memory mode).
const DEFAULT_MAX_MEMORY: usize = 64 * 1024 * 1024;
/// The shortest lock timeout: renewal runs every third of it.
const MIN_LOCK: Duration = Duration::from_millis(30);

type ScopeFn = dyn Fn(&Request) -> Option<String> + Send + Sync;

/// Make retried `POST`/`PATCH` requests run once (see the
/// [module docs](self) for the full behavior).
///
/// Keys are kept in memory by default (bounded by
/// [`max_keys`](Self::max_keys)); use [`redis`](Self::redis) when several
/// instances serve the app. Clones share state.
///
/// What is stored: status, headers (except `Date` and `Set-Cookie`) and
/// body, for complete responses up to 1 MiB (headers included). Cookies reach the first
/// response only: a replay never hands a session to whoever sends the key
/// again. Not stored, so a retry runs the handler again: server errors
/// (`5xx`); `401`, `403`, `404`, `405`, `408`, `409`, `425` and `429`;
/// streamed and larger bodies.
/// Wrap [`Compression`] *outside* this middleware so stored bodies stay
/// uncompressed.
///
/// A replay is answered before anything inside this middleware runs,
/// handlers' own auth checks included: it returns what the same client
/// already received. To stop replays once a credential is revoked, wrap
/// authentication *outside* `Idempotency`. Wrap [`SetRequestId`] and
/// [`Cors`] outside too: inside, a replay would repeat the first request's
/// id and allowed origin.
///
/// The default scope, the `Authorization` header, is whatever the client
/// sends. Check it (authentication outside this middleware) or scope by an
/// authenticated identity, or a client sending a new bogus header per
/// request can fill the memory store.
///
/// [`Compression`]: crate::middleware::Compression
/// [`SetRequestId`]: crate::middleware::SetRequestId
/// [`Cors`]: crate::middleware::Cors
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
            store: Arc::new(MemStore::new(DEFAULT_MAX_KEYS, DEFAULT_MAX_MEMORY)),
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

    /// How long a key stays locked after its instance stops renewing it
    /// (default 5 minutes). While a handler runs, the lock is renewed every
    /// third of this, so slow handlers keep their key; this only matters
    /// when an instance dies mid-request. At least 30 ms.
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::middleware::Idempotency;
    /// let mw = Idempotency::new().lock_timeout(Duration::from_secs(30));
    /// ```
    #[must_use]
    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout.max(MIN_LOCK);
        self
    }

    /// Who a key belongs to: two clients sending the same key never see
    /// each other's responses. The default is the `Authorization` header,
    /// and requests without one skip idempotency; apps with cookie sessions
    /// should scope by user. Returning `None` skips idempotency for that
    /// request.
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

    /// Most keys kept in memory (default 10 000), and a tenth of that per
    /// scope, so one client can't push out the others' responses. When
    /// full, expired keys are dropped first, then the oldest stored
    /// responses (the client's own first). A client whose share is all
    /// requests still running, or that finds the whole store running, gets
    /// `429` (code `idempotency_keys_exhausted`) for new keys. Memory mode
    /// is best effort:
    /// use [`redis`](Self::redis) where a second run would cost money.
    ///
    /// ```
    /// use zapline::middleware::Idempotency;
    /// let mw = Idempotency::new().max_keys(100_000);
    /// ```
    #[must_use]
    pub fn max_keys(mut self, n: usize) -> Self {
        self.store = Arc::new(MemStore::new(n.max(1), self.store.max_bytes));
        self
    }

    /// Most memory the stored responses may use in memory mode (default
    /// 64 MiB), and a tenth of that per scope. Responses are counted by
    /// body and header size; the oldest are dropped to make room, and a
    /// response that doesn't fit isn't stored (a retry runs the handler
    /// again).
    ///
    /// ```
    /// use zapline::middleware::Idempotency;
    /// let mw = Idempotency::new().max_memory(256 * 1024 * 1024);
    /// ```
    #[must_use]
    pub fn max_memory(mut self, bytes: usize) -> Self {
        self.store = Arc::new(MemStore::new(self.store.max_keys, bytes.max(1)));
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
        // A body declared over the app's limit may be a route's own larger
        // upload: buffering it here would answer `413`. (Skipping is safe:
        // a route without a larger limit rejects it unread.)
        if over_body_limit(&req) {
            return next.run(req);
        }
        // Without a scope there's no owner to keep keys apart, so clients
        // would share keys: skip rather than let them see each other's
        // responses.
        let scope = match &self.scope {
            Some(f) => f(&req).map(String::into_bytes),
            None => req
                .headers()
                .get(AUTHORIZATION)
                .map(|v| v.as_bytes().to_vec()),
        };
        let Some(scope) = scope else {
            return next.run(req);
        };
        let scope = scope_hash(&scope);
        let id = slot_id(scope, &key);
        let (backend, ttl, lock_timeout) = (self.backend(), self.ttl, self.lock_timeout);
        Box::pin(async move {
            let body = match req.body_bytes().await {
                Ok(b) => b,
                Err(e) => return e.into_response(),
            };
            let fp = fingerprint(&req, &body);
            req.body = Some(ReqBody::Full(body));

            let token = new_token();
            // Armed before the key is taken: if this request is cancelled
            // while `begin` waits on Redis, the lock it may hold is
            // released. Releasing only ever frees this token's own lock, and
            // the guard stays armed until the key is stored or released.
            let mut guard = Guard {
                backend: Some(backend.clone()),
                id,
                token,
            };
            let begin = backend
                .begin(&guard.id, scope, fp, token, lock_timeout)
                .await;
            if !matches!(begin, Begin::Run | Begin::Unavailable) {
                // Someone else holds the key: nothing of ours to free.
                guard.backend = None;
            }
            match begin {
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
                Begin::Exhausted => {
                    return Error::too_many_requests()
                        .with_code("idempotency_keys_exhausted")
                        .into_response();
                }
                Begin::Unavailable => {
                    return Error::service_unavailable("idempotency keys are unavailable")
                        .with_code("idempotency_unavailable")
                        .into_response();
                }
            }

            // Renew the lock while the handler runs, so a handler slower
            // than `lock_timeout` doesn't let a retry run alongside it, and
            // until the response is stored or the key released, so slow
            // storage doesn't either. In its own task: a slow renewal
            // (Redis) never holds up the handler.
            let renewer = {
                let (backend, id) = (backend.clone(), guard.id.clone());
                let every = lock_timeout / 3;
                AbortOnDrop(tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(every).await;
                        backend.renew(&id, token, lock_timeout).await;
                    }
                }))
            };
            let res = next.run(req).await;
            let (res, stored) = capture(res, fp);
            let settled = match stored {
                Some(stored) => {
                    let done = backend.finish(&guard.id, token, stored, ttl).await;
                    if !done {
                        crate::tasks::log_warn(format_args!(
                            "idempotency: the response to key {} was not stored; \
                             a retry may run the handler again",
                            guard.id
                        ));
                    }
                    done
                }
                // Not storable: free the key before answering, so an
                // immediate retry runs instead of seeing `409`.
                None => backend.release(&guard.id, token).await,
            };
            drop(renewer);
            if settled {
                guard.backend = None;
            }
            // Otherwise the guard tries the release again in the background.
            res
        })
    }
}

/// Stops the renewal task when the request ends, however it ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
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

/// A hasher with fixed keys, so every instance hashes alike. Only `write`
/// is used, with lengths as fixed-size integers: `Hash` impls may change
/// between Rust versions and platforms (`usize` lengths), `write` doesn't.
#[allow(deprecated)]
fn stable_hasher() -> std::hash::SipHasher {
    std::hash::SipHasher::new()
}

/// One length-prefixed field, the same on every platform.
fn write_part(h: &mut impl Hasher, bytes: &[u8]) {
    h.write(&(bytes.len() as u64).to_le_bytes());
    h.write(bytes);
}

fn scope_hash(scope: &[u8]) -> u64 {
    let mut h = stable_hasher();
    write_part(&mut h, scope);
    h.finish()
}

fn slot_id(scope: u64, key: &str) -> String {
    format!("zapline:idempotency:{scope:016x}:{key}")
}

/// A body known (from its `Content-Length`) to be larger than the app's
/// body limit: buffering it would answer `413` where its route may stream
/// it under a larger limit.
fn over_body_limit(req: &Request) -> bool {
    req.body
        .as_ref()
        .is_some_and(|b| b.size_hint().lower() > req.body_limit() as u64)
}

/// What makes two requests "the same": method, path, query, content type
/// (extractors accept or reject a body by it) and body.
fn fingerprint(req: &Request, body: &[u8]) -> u64 {
    let mut h = stable_hasher();
    write_part(&mut h, req.method().as_str().as_bytes());
    write_part(&mut h, req.path().as_bytes());
    write_part(&mut h, req.query_string().unwrap_or("").as_bytes());
    let content_type = req.headers().get(CONTENT_TYPE);
    write_part(&mut h, content_type.map_or(&b""[..], HeaderValue::as_bytes));
    write_part(&mut h, body);
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
    /// Roughly what it takes in memory (and in its encoded form).
    fn size(&self) -> usize {
        let headers: usize = self
            .headers
            .iter()
            .map(|(n, v)| 8 + n.as_str().len() + v.len())
            .sum();
        64 + headers + self.body.len()
    }

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
    // Final answers only. Not stored: server errors; rejections a retry may
    // not get (401/403: credentials change; 404/405: often routing, and
    // cheap for anyone to fill the store with); and answers that ask for a
    // retry (408/409/425/429).
    let storable_status = !status.is_server_error()
        && !matches!(
            status.as_u16(),
            401 | 403 | 404 | 405 | 408 | 409 | 425 | 429
        )
        && !status.is_informational();
    if !storable_status {
        return (res, None);
    }
    let (parts, body) = res.into_parts();
    let bytes = match body {
        Body::Empty | Body::Full(None) => Bytes::new(),
        Body::Full(Some(b)) if b.len() <= MAX_STORED => b,
        other => return (Response::from_parts(parts, other), None),
    };
    let headers = parts
        .headers
        .iter()
        // Cookies go to the first response only: replaying them could hand
        // a session to someone else.
        .filter(|(name, _)| **name != DATE && **name != SET_COOKIE)
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    let stored = Stored {
        fp,
        status: status.as_u16(),
        headers,
        body: bytes.clone(),
    };
    let fits = stored.size() <= MAX_STORED;
    (
        Response::from_parts(parts, Body::from(bytes)),
        fits.then_some(stored),
    )
}

enum Begin {
    /// The key is ours: run the handler.
    Run,
    Replay(Arc<Stored>),
    InFlight,
    Mismatch,
    /// This scope's share of the store is all requests still running.
    Exhausted,
    /// The shared store can't be reached (Redis only).
    #[cfg_attr(not(feature = "redis"), allow(dead_code))]
    Unavailable,
}

#[derive(Clone)]
enum Backend {
    Memory(Arc<MemStore>),
    #[cfg(feature = "redis")]
    Redis(crate::redis::Redis),
}

impl Backend {
    async fn begin(&self, id: &str, scope: u64, fp: u64, token: u128, lock: Duration) -> Begin {
        match self {
            Backend::Memory(m) => m.begin(id, scope, fp, token, lock),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => {
                let _ = scope; // Redis bounds its own memory.
                redis_store::begin(r, id, fp, token, lock).await
            }
        }
    }

    /// Extend our lock on the key while the handler still runs.
    async fn renew(&self, id: &str, token: u128, lock: Duration) {
        match self {
            Backend::Memory(m) => m.renew(id, token, lock),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => redis_store::renew(r, id, token, lock).await,
        }
    }

    /// Store the response; `false` when it could not be stored.
    async fn finish(&self, id: &str, token: u128, stored: Stored, ttl: Duration) -> bool {
        match self {
            Backend::Memory(m) => m.finish(id, token, stored, ttl),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => redis_store::finish(r, id, token, &stored, ttl).await,
        }
    }

    /// Free our key; `false` when that may not have happened.
    async fn release(&self, id: &str, token: u128) -> bool {
        match self {
            Backend::Memory(m) => {
                m.release(id, token);
                true
            }
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
                    rt.spawn(async move {
                        // One more try, a little later, if Redis failed.
                        if !redis_store::release(&r, &id, token).await {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            redis_store::release(&r, &id, token).await;
                        }
                    });
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
    scope: u64,
}

impl Entry {
    fn bytes(&self) -> usize {
        match &self.slot {
            Slot::Done(s) => s.size(),
            Slot::Running { .. } => 0,
        }
    }
}

/// `now + d`, or far in the future when that overflows.
fn later(now: Instant, d: Duration) -> Instant {
    const FAR: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);
    now.checked_add(d)
        .or_else(|| now.checked_add(FAR))
        .unwrap_or(now)
}

/// Keys in memory, with the stored responses ordered by expiry (overall
/// and per scope), so making room never scans the whole store.
#[derive(Default)]
struct Keys {
    map: HashMap<String, Entry>,
    scopes: HashMap<u64, ScopeKeys>,
    done: BTreeSet<(Instant, String)>,
    /// Memory used by stored responses.
    bytes: usize,
    /// When expired locks (from cancelled renewals) were last looked for.
    swept: Option<Instant>,
}

#[derive(Default)]
struct ScopeKeys {
    count: usize,
    bytes: usize,
    done: BTreeSet<(Instant, String)>,
}

impl Keys {
    fn count(&self, scope: u64) -> usize {
        self.scopes.get(&scope).map_or(0, |s| s.count)
    }

    fn scope_bytes(&self, scope: u64) -> usize {
        self.scopes.get(&scope).map_or(0, |s| s.bytes)
    }

    /// Add a running request's key (replacing nothing: callers remove first).
    fn insert(&mut self, id: String, entry: Entry) {
        self.scopes.entry(entry.scope).or_default().count += 1;
        if let Some(old) = self.map.insert(id.clone(), entry) {
            self.forget(&id, &old);
        }
    }

    fn remove(&mut self, id: &str) {
        if let Some(old) = self.map.remove(id) {
            self.forget(id, &old);
        }
    }

    fn forget(&mut self, id: &str, old: &Entry) {
        let is_done = matches!(old.slot, Slot::Done(_));
        let bytes = old.bytes();
        if is_done {
            self.done.remove(&(old.until, id.to_owned()));
            self.bytes -= bytes;
        }
        if let Some(s) = self.scopes.get_mut(&old.scope) {
            if is_done {
                s.done.remove(&(old.until, id.to_owned()));
                s.bytes -= bytes;
            }
            s.count -= 1;
            if s.count == 0 {
                self.scopes.remove(&old.scope);
            }
        }
    }

    /// Store a response under a running key.
    fn mark_done(&mut self, id: &str, stored: Stored, until: Instant) {
        let Some(e) = self.map.get_mut(id) else {
            return;
        };
        let bytes = stored.size();
        e.slot = Slot::Done(Arc::new(stored));
        e.until = until;
        let scope = e.scope;
        self.done.insert((until, id.to_owned()));
        self.bytes += bytes;
        let s = self.scopes.entry(scope).or_default();
        s.done.insert((until, id.to_owned()));
        s.bytes += bytes;
    }

    /// The stored response that expires first, among `scope`'s or all.
    fn oldest_done(&self, scope: Option<u64>) -> Option<String> {
        let set = match scope {
            Some(s) => &self.scopes.get(&s)?.done,
            None => &self.done,
        };
        set.first().map(|(_, id)| id.clone())
    }

    /// Drop expired responses (cheap: they're ordered), and at most once a
    /// second, expired locks too.
    fn sweep(&mut self, now: Instant) {
        while let Some((until, id)) = self.done.first().cloned() {
            if until > now {
                break;
            }
            self.remove(&id);
        }
        if self
            .swept
            .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(1))
        {
            return;
        }
        self.swept = Some(now);
        let expired: Vec<String> = self
            .map
            .iter()
            .filter(|(_, e)| e.until <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for id in expired {
            self.remove(&id);
        }
    }
}

struct MemStore {
    keys: Mutex<Keys>,
    max_keys: usize,
    /// Most keys one scope may hold, so one client can't flush the
    /// others' stored responses.
    per_scope: usize,
    /// Most memory stored responses may use, overall and per scope.
    max_bytes: usize,
    per_scope_bytes: usize,
}

impl MemStore {
    fn new(max_keys: usize, max_bytes: usize) -> Self {
        MemStore {
            keys: Mutex::new(Keys::default()),
            max_keys,
            per_scope: (max_keys / 10).max(1),
            max_bytes,
            per_scope_bytes: (max_bytes / 10).max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Keys> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn begin(&self, id: &str, scope: u64, fp: u64, token: u128, lock: Duration) -> Begin {
        let now = Instant::now();
        let mut keys = self.lock();
        match keys.map.get(id) {
            Some(e) if e.until > now => {
                return match &e.slot {
                    Slot::Running { fp: f, .. } if *f != fp => Begin::Mismatch,
                    Slot::Running { .. } => Begin::InFlight,
                    Slot::Done(s) if s.fp != fp => Begin::Mismatch,
                    Slot::Done(s) => Begin::Replay(s.clone()),
                };
            }
            Some(_) => keys.remove(id),
            None => {}
        }
        let full = |k: &Keys| k.map.len() >= self.max_keys;
        let scope_full = |k: &Keys| k.count(scope) >= self.per_scope;
        if full(&keys) || scope_full(&keys) {
            keys.sweep(now);
        }
        // Make room from this scope's own responses first, then (when the
        // whole store is full) from anyone's.
        if scope_full(&keys) {
            match keys.oldest_done(Some(scope)) {
                Some(k) => keys.remove(&k),
                // All this client's own requests, still running.
                None => return Begin::Exhausted,
            }
        }
        if full(&keys) {
            match keys.oldest_done(None) {
                Some(k) => keys.remove(&k),
                // Every key is a running request: this client waits, like
                // one that filled its own share.
                None => return Begin::Exhausted,
            }
        }
        keys.insert(
            id.to_owned(),
            Entry {
                slot: Slot::Running { fp, token },
                until: later(now, lock),
                scope,
            },
        );
        Begin::Run
    }

    fn renew(&self, id: &str, token: u128, lock: Duration) {
        let mut keys = self.lock();
        if let Some(e) = keys.map.get_mut(id) {
            if matches!(e.slot, Slot::Running { token: t, .. } if t == token) {
                e.until = later(Instant::now(), lock);
            }
        }
    }

    fn finish(&self, id: &str, token: u128, stored: Stored, ttl: Duration) -> bool {
        let mut keys = self.lock();
        let ours = matches!(
            keys.map.get(id),
            Some(Entry { slot: Slot::Running { token: t, .. }, .. }) if *t == token
        );
        if !ours {
            return false;
        }
        // Make room in this scope's budget, then in the whole store's, from
        // the oldest stored responses. One that can't fit isn't stored.
        let (size, scope) = (stored.size(), keys.map[id].scope);
        let mut fits = size <= self.per_scope_bytes;
        while fits && keys.scope_bytes(scope) + size > self.per_scope_bytes {
            match keys.oldest_done(Some(scope)) {
                Some(k) => keys.remove(&k),
                None => fits = false,
            }
        }
        while fits && keys.bytes + size > self.max_bytes {
            match keys.oldest_done(None) {
                Some(k) => keys.remove(&k),
                None => fits = false,
            }
        }
        if !fits {
            keys.remove(id);
            return false;
        }
        keys.mark_done(id, stored, later(Instant::now(), ttl));
        true
    }

    fn release(&self, id: &str, token: u128) {
        let mut keys = self.lock();
        if matches!(keys.map.get(id), Some(Entry { slot: Slot::Running { token: t, .. }, .. }) if *t == token)
        {
            keys.remove(id);
        }
    }
}

#[cfg(feature = "redis")]
mod redis_store {
    use super::*;
    use crate::redis::{Redis, Reply};

    /// Longest wait for one Redis call: a stalled Redis is treated like
    /// one that is down instead of holding requests.
    const DEADLINE: Duration = Duration::from_secs(5);

    async fn call(redis: &Redis, args: [&[u8]; 6]) -> crate::Result<Reply> {
        call_args(redis, &args).await
    }

    async fn call_args(redis: &Redis, args: &[&[u8]]) -> crate::Result<Reply> {
        match tokio::time::timeout(DEADLINE, redis.cmd(args.iter().copied())).await {
            Ok(reply) => reply,
            Err(_) => Err(Error::service_unavailable("Redis did not answer in time")),
        }
    }

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
    /// Extend our lock (matched like `RELEASE`) by `ARGV[2]` ms.
    const RENEW: &str = r#"
local v = redis.call('GET', KEYS[1])
if v and string.sub(v, 1, 1) == 'L' and string.sub(v, 10) == ARGV[1] then
  return redis.call('PEXPIRE', KEYS[1], ARGV[2])
end
return 0
"#;

    pub(super) async fn renew(redis: &Redis, id: &str, token: u128, lock: Duration) {
        let ms = millis(lock);
        let reply = call(
            redis,
            [
                "EVAL".as_bytes(),
                RENEW.as_bytes(),
                b"1",
                id.as_bytes(),
                &token.to_be_bytes(),
                ms.as_bytes(),
            ],
        )
        .await;
        if let Err(e) = reply {
            crate::tasks::log_warn(format_args!(
                "idempotency: renewing a key's lock failed: {}",
                e.message()
            ));
        }
    }

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
            let reply = call(
                redis,
                [
                    "EVAL".as_bytes(),
                    BEGIN.as_bytes(),
                    b"1",
                    id.as_bytes(),
                    &value,
                    ms.as_bytes(),
                ],
            )
            .await;
            let held = match reply {
                Ok(Reply::Int(1)) => return Begin::Run,
                // Our own lock: an earlier attempt took the key but its
                // reply was lost (the connection is retried).
                Ok(Reply::Bulk(b)) if b == value => return Begin::Run,
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

    /// `true` once the response is stored. A failed call is tried once
    /// more (the script is idempotent); `0` means the lock was lost.
    pub(super) async fn finish(
        redis: &Redis,
        id: &str,
        token: u128,
        s: &Stored,
        ttl: Duration,
    ) -> bool {
        let (ms, lock, encoded) = (millis(ttl), lock_value(s.fp, token), s.encode());
        for _ in 0..2 {
            let reply = call_args(
                redis,
                &[
                    "EVAL".as_bytes(),
                    FINISH.as_bytes(),
                    b"1",
                    id.as_bytes(),
                    &lock,
                    &encoded,
                    ms.as_bytes(),
                ],
            )
            .await;
            match reply {
                Ok(Reply::Int(1)) => return true,
                Ok(_) => {
                    // Not our lock any more: someone else holds the key.
                    crate::tasks::log_warn(format_args!(
                        "idempotency: lost the lock on {id} before storing the response"
                    ));
                    return false;
                }
                Err(e) => crate::tasks::log_warn(format_args!(
                    "idempotency: storing the response failed: {}",
                    e.message()
                )),
            }
        }
        false
    }

    /// `false` when Redis failed, so the key may still be locked.
    pub(super) async fn release(redis: &Redis, id: &str, token: u128) -> bool {
        let reply = call_args(
            redis,
            &[
                "EVAL".as_bytes(),
                RELEASE.as_bytes(),
                b"1",
                id.as_bytes(),
                &token.to_be_bytes(),
            ],
        )
        .await;
        match reply {
            Ok(_) => true,
            Err(e) => {
                crate::tasks::log_warn(format_args!(
                    "idempotency: releasing a key failed: {}",
                    e.message()
                ));
                false
            }
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
