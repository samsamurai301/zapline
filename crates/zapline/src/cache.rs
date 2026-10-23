//! Caching: a key-value [`Cache`] for computed values, [`ResponseCache`]
//! for whole `GET` responses, and [`ETag`] for conditional requests.
//!
//! ```
//! use std::time::Duration;
//! use zapline::cache::{Cache, ResponseCache};
//! use zapline::prelude::*;
//!
//! #[model]
//! struct Stats { users: u64 }
//!
//! #[get("/stats")]
//! async fn stats(cache: Cache) -> Result<Json<Stats>> {
//!     // Computed at most once a minute, however many requests arrive.
//!     let stats = cache
//!         .get_or_set("stats", Duration::from_secs(60), || async {
//!             Ok(Stats { users: 42 }) // an expensive query
//!         })
//!         .await?;
//!     Ok(Json(stats))
//! }
//!
//! #[get("/about")]
//! async fn about() -> Html<&'static str> { Html("<h1>About</h1>") }
//!
//! let app = App::new()
//!     .cache(Cache::memory())
//!     .mount(stats)
//!     .nest("/pages", Routes::new().mount(about).wrap(ResponseCache::new(Duration::from_secs(300))));
//! ```

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{CACHE_CONTROL, COOKIE, ETAG, IF_NONE_MATCH, SET_COOKIE, VARY};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::body::Body;
use crate::extract::FromRequestParts;
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::response::{IntoResponse, Response};
use crate::{App, Error, Request, Result};

/// Default capacity of a [`Cache::memory`] (entries).
const DEFAULT_ENTRIES: usize = 10_000;

/// A key-value cache with expiry. Values are stored as JSON, so any
/// `Serialize` + `Deserialize` type works. Cheap to clone; a handler
/// argument once registered with [`App::cache`].
#[derive(Clone)]
pub struct Cache {
    inner: Arc<Inner>,
}

struct Inner {
    entries: Mutex<HashMap<String, Entry>>,
    max_entries: usize,
    /// One lock per key being computed by `get_or_set`, so concurrent
    /// misses compute once.
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Shared storage instead of `entries`.
    #[cfg(feature = "redis")]
    redis: Option<crate::redis::Redis>,
}

/// Where Redis keeps cache entries.
#[cfg(feature = "redis")]
const REDIS_PREFIX: &str = "zapline:cache:";

struct Entry {
    value: Bytes,
    expires: Instant,
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cache")
            .field("entries", &lock(&self.inner.entries).len())
            .field("max_entries", &self.inner.max_entries)
            .finish()
    }
}

impl Cache {
    /// An in-process cache holding up to 10,000 entries.
    ///
    /// ```
    /// let cache = zapline::cache::Cache::memory();
    /// ```
    pub fn memory() -> Self {
        Cache {
            inner: Arc::new(Inner {
                entries: Mutex::default(),
                max_entries: DEFAULT_ENTRIES,
                inflight: Mutex::default(),
                #[cfg(feature = "redis")]
                redis: None,
            }),
        }
    }

    /// A cache in Redis, shared by every instance of the app (feature
    /// `redis`). Keys live under `zapline:cache:` and expire on their own;
    /// if Redis is down, reads miss and writes are skipped (with a warning)
    /// rather than failing requests.
    ///
    /// ```
    /// use zapline::cache::Cache;
    /// use zapline::redis::Redis;
    ///
    /// let cache = Cache::redis(Redis::new("redis://localhost")?);
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[cfg(feature = "redis")]
    pub fn redis(redis: crate::redis::Redis) -> Self {
        Cache {
            inner: Arc::new(Inner {
                entries: Mutex::default(),
                max_entries: DEFAULT_ENTRIES,
                inflight: Mutex::default(),
                redis: Some(redis),
            }),
        }
    }

    /// Hold at most `n` entries (at least 1). When full, expired entries go
    /// first, then the ones closest to expiring.
    ///
    /// ```
    /// let cache = zapline::cache::Cache::memory().max_entries(1_000);
    /// ```
    #[must_use]
    pub fn max_entries(self, n: usize) -> Self {
        Cache {
            inner: Arc::new(Inner {
                entries: Mutex::default(),
                max_entries: n.max(1),
                inflight: Mutex::default(),
                #[cfg(feature = "redis")]
                redis: self.inner.redis.clone(),
            }),
        }
    }

    /// The value under `key`, if present, not expired, and of type `T`.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// use std::time::Duration;
    ///
    /// let cache = zapline::cache::Cache::memory();
    /// cache.set("answer", &42, Duration::from_secs(60)).await?;
    /// assert_eq!(cache.get::<u32>("answer").await, Some(42));
    /// assert_eq!(cache.get::<u32>("question").await, None);
    /// # Ok(()) }
    /// ```
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        #[cfg(feature = "redis")]
        if let Some(redis) = &self.inner.redis {
            let reply = redis.cmd(["GET", &format!("{REDIS_PREFIX}{key}")]).await;
            return match reply {
                Ok(crate::redis::Reply::Bulk(bytes)) => serde_json::from_slice(&bytes).ok(),
                Ok(_) => None,
                Err(e) => {
                    redis_trouble(&e);
                    None
                }
            };
        }
        let bytes = {
            let mut entries = lock(&self.inner.entries);
            match entries.get(key) {
                Some(e) if e.expires > Instant::now() => e.value.clone(),
                Some(_) => {
                    entries.remove(key);
                    return None;
                }
                None => return None,
            }
        };
        serde_json::from_slice(&bytes).ok()
    }

    /// Store `value` under `key` for `ttl`.
    ///
    /// # Errors
    ///
    /// If `value` can't be serialized to JSON.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// use std::time::Duration;
    ///
    /// let cache = zapline::cache::Cache::memory();
    /// cache.set("user:1:name", &"Ada", Duration::from_secs(300)).await?;
    /// # Ok(()) }
    /// ```
    pub async fn set<T: Serialize + ?Sized>(
        &self,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> Result<()> {
        let value = serde_json::to_vec(value)
            .map_err(|e| Error::internal("cannot cache a value").with_source(e))?;
        #[cfg(feature = "redis")]
        if let Some(redis) = &self.inner.redis {
            let ms = ttl.as_millis().max(1).to_string();
            let key = format!("{REDIS_PREFIX}{key}");
            let set = [
                b"SET".as_slice(),
                key.as_bytes(),
                &value,
                b"PX",
                ms.as_bytes(),
            ];
            if let Err(e) = redis.cmd(set).await {
                redis_trouble(&e);
            }
            return Ok(());
        }
        let now = Instant::now();
        let mut entries = lock(&self.inner.entries);
        if entries.len() >= self.inner.max_entries && !entries.contains_key(key) {
            evict(&mut entries, now, self.inner.max_entries);
        }
        entries.insert(
            key.to_owned(),
            Entry {
                value: value.into(),
                expires: now + ttl,
            },
        );
        Ok(())
    }

    /// Remove `key`.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let cache = zapline::cache::Cache::memory();
    /// cache.delete("user:1:name").await;
    /// # }
    /// ```
    pub async fn delete(&self, key: &str) {
        #[cfg(feature = "redis")]
        if let Some(redis) = &self.inner.redis {
            if let Err(e) = redis.cmd(["UNLINK", &format!("{REDIS_PREFIX}{key}")]).await {
                redis_trouble(&e);
            }
            return;
        }
        lock(&self.inner.entries).remove(key);
    }

    /// Remove every key that starts with `prefix`, e.g. `"posts:"` after a
    /// post changes.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let cache = zapline::cache::Cache::memory();
    /// cache.delete_prefix("posts:").await;
    /// # }
    /// ```
    pub async fn delete_prefix(&self, prefix: &str) {
        #[cfg(feature = "redis")]
        if let Some(redis) = &self.inner.redis {
            let pattern = format!("{REDIS_PREFIX}{}*", crate::redis::glob_escape(prefix));
            if let Err(e) = crate::redis::delete_matching(redis, &pattern).await {
                redis_trouble(&e);
            }
            return;
        }
        lock(&self.inner.entries).retain(|k, _| !k.starts_with(prefix));
    }

    /// The cached value, or else the result of `compute`, cached for `ttl`.
    /// Concurrent callers with the same key wait for one computation
    /// instead of all running it. Errors are not cached.
    ///
    /// # Errors
    ///
    /// Whatever `compute` returns.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// use std::time::Duration;
    ///
    /// let cache = zapline::cache::Cache::memory();
    /// let n: u64 = cache
    ///     .get_or_set("count", Duration::from_secs(10), || async { Ok(7) })
    ///     .await?;
    /// assert_eq!(n, 7);
    /// # Ok(()) }
    /// ```
    pub async fn get_or_set<T, F, Fut>(&self, key: &str, ttl: Duration, compute: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        if let Some(hit) = self.get(key).await {
            return Ok(hit);
        }
        let gate = lock(&self.inner.inflight)
            .entry(key.to_owned())
            .or_default()
            .clone();
        let result = {
            let _turn = gate.lock().await;
            // Someone else may have filled it while we waited.
            if let Some(hit) = self.get(key).await {
                Ok(hit)
            } else {
                match compute().await {
                    Ok(value) => {
                        self.set(key, &value, ttl).await?;
                        Ok(value)
                    }
                    Err(e) => Err(e),
                }
            }
        };
        let mut inflight = lock(&self.inner.inflight);
        // Two references: the map's and ours. Drop the entry if nobody else
        // is waiting on it.
        if Arc::strong_count(&gate) <= 2 {
            inflight.remove(key);
        }
        result
    }

    /// Remove everything.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// zapline::cache::Cache::memory().clear().await;
    /// # }
    /// ```
    pub async fn clear(&self) {
        #[cfg(feature = "redis")]
        if self.inner.redis.is_some() {
            return self.delete_prefix("").await;
        }
        lock(&self.inner.entries).clear();
    }
}

/// Redis being down degrades the cache to misses; say so in the logs.
#[cfg(feature = "redis")]
fn redis_trouble(e: &Error) {
    crate::tasks::log_warn(format_args!("cache: {}", e.message()));
}

/// Make room for one entry: drop the expired ones, then the soonest to
/// expire.
fn evict(entries: &mut HashMap<String, Entry>, now: Instant, max: usize) {
    entries.retain(|_, e| e.expires > now);
    if entries.len() < max {
        return;
    }
    // Free about a tenth at once so a full cache doesn't scan on every insert.
    let mut by_expiry: Vec<(Instant, String)> = entries
        .iter()
        .map(|(k, e)| (e.expires, k.clone()))
        .collect();
    let drop = (max / 10).max(1).min(by_expiry.len());
    by_expiry.select_nth_unstable_by_key(drop - 1, |(t, _)| *t);
    for (_, key) in by_expiry.into_iter().take(drop) {
        entries.remove(&key);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FromRequestParts for Cache {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Cache>().cloned().ok_or_else(|| {
            Error::internal("no cache registered: add `.cache(Cache::memory())` to the app")
                .diagnosed("ZL0071", None)
        })
    }
}

impl App {
    /// Register `cache` for handlers ([`Cache`] extractor), jobs and
    /// startup hooks.
    ///
    /// ```
    /// let app = zapline::App::new().cache(zapline::cache::Cache::memory());
    /// ```
    pub fn cache(self, cache: Cache) -> Self {
        self.state(cache)
    }
}

// ---------------------------------------------------------------------------
// Response caching

/// Route middleware that keeps whole `GET` responses for `ttl` and answers
/// repeats from memory, with `x-cache: HIT` (or `MISS`).
///
/// Only anonymous, shareable responses are kept: the request has no
/// `Authorization` or `Cookie` header, and the response is a `200` with a
/// buffered body, no `Set-Cookie`, and no `Cache-Control: private` or
/// `no-store`. The query string and the `Accept` and `HX-Request` headers
/// are part of the key, so htmx fragments and full pages don't mix. A
/// `Cache-Control: public, max-age=..` header is added when the handler set
/// none, so browsers and CDNs cache too.
///
/// ```
/// use std::time::Duration;
/// use zapline::cache::ResponseCache;
/// use zapline::prelude::*;
///
/// #[get("/")]
/// async fn home() -> Html<&'static str> { Html("<h1>Hi</h1>") }
///
/// let routes = Routes::new().mount(home).wrap(ResponseCache::new(Duration::from_secs(60)));
/// ```
#[derive(Clone)]
pub struct ResponseCache {
    ttl: Duration,
    store: Arc<Mutex<HashMap<String, Stored>>>,
    max_entries: usize,
}

struct Stored {
    headers: HeaderMap,
    body: Bytes,
    expires: Instant,
}

const X_CACHE: HeaderName = HeaderName::from_static("x-cache");
const HX_REQUEST: HeaderName = HeaderName::from_static("hx-request");

impl ResponseCache {
    /// Keep responses for `ttl` (up to 10,000 of them).
    ///
    /// ```
    /// let mw = zapline::cache::ResponseCache::new(std::time::Duration::from_secs(30));
    /// ```
    pub fn new(ttl: Duration) -> Self {
        ResponseCache {
            ttl,
            store: Arc::default(),
            max_entries: DEFAULT_ENTRIES,
        }
    }

    /// Keep at most `n` responses.
    ///
    /// ```
    /// let mw = zapline::cache::ResponseCache::new(std::time::Duration::from_secs(30)).max_entries(500);
    /// ```
    #[must_use]
    pub fn max_entries(mut self, n: usize) -> Self {
        self.max_entries = n.max(1);
        self
    }
}

fn cache_key(req: &Request) -> Option<String> {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return None;
    }
    let headers = req.headers();
    if headers.contains_key(http::header::AUTHORIZATION) || headers.contains_key(COOKIE) {
        return None;
    }
    let header = |name: &HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    Some(format!(
        "{}\n{}\n{}",
        req.uri(),
        header(&http::header::ACCEPT),
        header(&HX_REQUEST)
    ))
}

fn shareable(res: &Response) -> bool {
    if res.status() != StatusCode::OK || res.headers().contains_key(SET_COOKIE) {
        return false;
    }
    let private = res.headers().get_all(CACHE_CONTROL).iter().any(|v| {
        let v = v.to_str().unwrap_or("").to_ascii_lowercase();
        v.contains("private") || v.contains("no-store") || v.contains("no-cache")
    });
    let vary_all = res
        .headers()
        .get_all(VARY)
        .iter()
        .any(|v| v.as_bytes().trim_ascii() == b"*");
    !private && !vary_all
}

impl Middleware for ResponseCache {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let Some(key) = cache_key(&req) else {
            return next.run(req);
        };
        let now = Instant::now();
        let hit = {
            let mut store = lock(&self.store);
            match store.get(&key) {
                Some(s) if s.expires > now => Some((s.headers.clone(), s.body.clone())),
                Some(_) => {
                    store.remove(&key);
                    None
                }
                None => None,
            }
        };
        if let Some((headers, body)) = hit {
            let mut res = Response::new(Body::from(body));
            *res.headers_mut() = headers;
            res.headers_mut()
                .insert(X_CACHE, HeaderValue::from_static("HIT"));
            return Box::pin(std::future::ready(res));
        }

        // A HEAD response has no body to keep: it may read what a GET
        // stored, never store.
        let head = *req.method() == Method::HEAD;
        let this = self.clone();
        Box::pin(async move {
            let mut res = next.run(req).await;
            if head || !shareable(&res) {
                return res;
            }
            let body = match std::mem::take(res.body_mut()) {
                Body::Full(Some(bytes)) => bytes,
                Body::Empty | Body::Full(None) => Bytes::new(),
                // Streams pass through untouched.
                stream @ Body::Stream(_) => {
                    *res.body_mut() = stream;
                    return res;
                }
            };
            if !res.headers().contains_key(CACHE_CONTROL) {
                let value = format!("public, max-age={}", this.ttl.as_secs());
                if let Ok(value) = HeaderValue::try_from(value) {
                    res.headers_mut().insert(CACHE_CONTROL, value);
                }
            }
            {
                let mut store = lock(&this.store);
                if store.len() >= this.max_entries && !store.contains_key(&key) {
                    store.retain(|_, s| s.expires > now);
                    if store.len() >= this.max_entries {
                        if let Some(k) = store.keys().next().cloned() {
                            store.remove(&k);
                        }
                    }
                }
                store.insert(
                    key,
                    Stored {
                        headers: res.headers().clone(),
                        body: body.clone(),
                        expires: now + this.ttl,
                    },
                );
            }
            *res.body_mut() = Body::from(body);
            res.headers_mut()
                .insert(X_CACHE, HeaderValue::from_static("MISS"));
            res
        })
    }
}

// ---------------------------------------------------------------------------
// ETags

/// Middleware that tags `200` responses to `GET`/`HEAD` with an `ETag`
/// (a hash of the body) and answers `304 Not Modified` when the client
/// already has that version, so unchanged pages cost no bandwidth.
/// Streamed bodies and responses that already have an `ETag` are left
/// alone.
///
/// ```
/// use zapline::cache::ETag;
///
/// let app = zapline::App::new().wrap(ETag);
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct ETag;

impl Middleware for ETag {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        if !matches!(*req.method(), Method::GET | Method::HEAD) {
            return next.run(req);
        }
        let if_none_match = req.headers().get(IF_NONE_MATCH).cloned();
        Box::pin(async move {
            let mut res = next.run(req).await;
            if res.status() != StatusCode::OK || res.headers().contains_key(ETAG) {
                return res;
            }
            let tag = match res.body() {
                Body::Full(Some(bytes)) => fnv1a(bytes),
                Body::Empty | Body::Full(None) => fnv1a(&[]),
                Body::Stream(_) => return res,
            };
            let Ok(tag) = HeaderValue::try_from(format!("W/\"{tag:016x}\"")) else {
                return res;
            };
            let matches = if_none_match.is_some_and(|v| {
                v.to_str().is_ok_and(|list| {
                    list.split(',').any(|t| {
                        let t = t.trim();
                        t == "*"
                            || t.trim_start_matches("W/")
                                == tag.to_str().unwrap_or("").trim_start_matches("W/")
                    })
                })
            });
            if matches {
                let mut not_modified = StatusCode::NOT_MODIFIED.into_response();
                for name in [
                    CACHE_CONTROL,
                    VARY,
                    http::header::CONTENT_LOCATION,
                    http::header::EXPIRES,
                ] {
                    if let Some(v) = res.headers().get(&name) {
                        not_modified.headers_mut().insert(name, v.clone());
                    }
                }
                not_modified.headers_mut().insert(ETAG, tag);
                return not_modified;
            }
            res.headers_mut().insert(ETAG, tag);
            res
        })
    }
}

/// 64-bit FNV-1a: stable across builds, so tags survive restarts.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}
