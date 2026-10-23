//! Remote JWK Sets (feature `jwks`): verify tokens from an identity
//! provider with the keys it publishes at its `jwks_uri`.
//!
//! Keys are fetched once, cached for a TTL and refreshed in the background
//! when they go stale, while the cached keys keep serving (also when the
//! endpoint fails). A token naming an unknown `kid` triggers a refresh, at
//! most once per [`min_refresh_interval`](Jwks::min_refresh_interval), so
//! made-up key ids can't hammer the provider. No request waits longer than
//! the [`timeout`](Jwks::timeout) for a fetch.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bytes::Bytes;
use http::header::{ACCEPT, USER_AGENT};
use http::{Method, Request as HttpRequest};
use http_body_util::Full;
use serde_json::Value;
use tokio::time::{timeout_at, Instant};

use super::jwt::{jwk_key, Algorithm, Jwk, VerifyKey, ASYMMETRIC};
use crate::http_client::fetch;
use crate::{Error, Result};

/// Largest JWKS (or discovery) document accepted, in bytes.
const MAX_BODY: usize = 1024 * 1024;

/// A remote JWK Set: where to fetch verification keys and how long to keep
/// them. Use it with [`JwtConfig::from_jwks`](super::JwtConfig::from_jwks)
/// or [`JwtConfig::jwks`](super::JwtConfig::jwks). Clones share the cache.
///
/// Defaults: keys are cached for 10 minutes, refreshed at most every 30 s
/// for unknown `kid`s, and a fetch may take 5 s. RSA (`RS*`, `PS*`), EC
/// (`ES256`, `ES384`) and Ed25519 (`EdDSA`) keys are used; symmetric and
/// encryption keys are ignored.
///
/// ```
/// use std::time::Duration;
/// use zapline::auth::{Algorithm, Jwks, JwtConfig};
///
/// let jwks = Jwks::new("https://idp.example.com/.well-known/jwks.json")
///     .ttl(Duration::from_secs(15 * 60))
///     .min_refresh_interval(Duration::from_secs(60))
///     .timeout(Duration::from_secs(2))
///     .algorithms([Algorithm::RS256, Algorithm::ES256]);
/// let jwt = JwtConfig::from_jwks(jwks).issuer("https://idp.example.com").audience("api");
/// ```
#[derive(Clone)]
pub struct Jwks {
    settings: Settings,
    shared: Arc<Shared>,
}

#[derive(Clone)]
struct Settings {
    ttl: Duration,
    min_refresh: Duration,
    timeout: Duration,
    algorithms: Arc<[Algorithm]>,
}

enum Source {
    Url(String),
    /// OpenID Connect discovery: `jwks_uri` is read from the issuer's
    /// `/.well-known/openid-configuration` on the first fetch.
    Discovery {
        issuer: String,
        jwks_uri: Mutex<Option<String>>,
    },
}

struct Shared {
    source: Source,
    cache: RwLock<Cache>,
    /// Single flight: one fetch at a time.
    refresh: tokio::sync::Mutex<()>,
    /// A background refresh is running.
    background: AtomicBool,
}

#[derive(Default)]
struct Cache {
    keys: Arc<[Arc<VerifyKey>]>,
    /// When the keys were last fetched successfully.
    fetched: Option<Instant>,
    /// When a fetch last started (rate limit).
    attempted: Option<Instant>,
    /// Whether the last fetch failed.
    failed: bool,
}

impl fmt::Debug for Jwks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Jwks");
        match &self.shared.source {
            Source::Url(url) => d.field("url", url),
            Source::Discovery { issuer, .. } => d.field("issuer", issuer),
        };
        d.field("ttl", &self.settings.ttl)
            .field("key_ids", &self.key_ids())
            .finish_non_exhaustive()
    }
}

impl Jwks {
    /// Keys from the JWK Set at `url` (`https://` in production; `http://`
    /// works for local test doubles).
    ///
    /// ```
    /// let google = zapline::auth::Jwks::new("https://www.googleapis.com/oauth2/v3/certs");
    /// ```
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_source(Source::Url(url.into()))
    }

    /// Keys of an OpenID Connect provider, found through discovery: the
    /// `jwks_uri` of `{issuer}/.well-known/openid-configuration`, whose
    /// `issuer` must match. Pair it with `.issuer(issuer)` on the config.
    ///
    /// ```
    /// use zapline::auth::{Jwks, JwtConfig};
    ///
    /// let issuer = "https://accounts.google.com";
    /// let jwt = JwtConfig::from_jwks(Jwks::discover(issuer)).issuer(issuer).audience("my-client-id");
    /// ```
    pub fn discover(issuer: impl Into<String>) -> Self {
        Self::with_source(Source::Discovery {
            issuer: issuer.into(),
            jwks_uri: Mutex::new(None),
        })
    }

    fn with_source(source: Source) -> Self {
        Jwks {
            settings: Settings {
                ttl: Duration::from_secs(600),
                min_refresh: Duration::from_secs(30),
                timeout: Duration::from_secs(5),
                algorithms: ASYMMETRIC.into(),
            },
            shared: Arc::new(Shared {
                source,
                cache: RwLock::new(Cache::default()),
                refresh: tokio::sync::Mutex::new(()),
                background: AtomicBool::new(false),
            }),
        }
    }

    /// How long fetched keys are fresh (default 10 minutes). Stale keys
    /// keep verifying while a background refresh runs, and for as long as
    /// the endpoint fails.
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.settings.ttl = ttl;
        self
    }

    /// Fetch at most this often (default 30 s), however many tokens name
    /// unknown keys or however long the endpoint has been failing.
    pub fn min_refresh_interval(mut self, interval: Duration) -> Self {
        self.settings.min_refresh = interval;
        self
    }

    /// The longest a request waits for a fetch (default 5 s). Past it, the
    /// request is answered with what the cache has.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.settings.timeout = timeout;
        self
    }

    /// Accept only keys for these algorithms (default: every RSA, EC and
    /// Ed25519 algorithm). HMAC algorithms are never accepted from a JWKS.
    pub fn algorithms(mut self, algorithms: impl IntoIterator<Item = Algorithm>) -> Self {
        self.settings.algorithms = algorithms
            .into_iter()
            .filter(|a| ASYMMETRIC.contains(a))
            .collect();
        self
    }

    /// The `kid` of every cached key.
    ///
    /// ```
    /// let jwks = zapline::auth::Jwks::new("https://idp.example.com/jwks.json");
    /// assert!(jwks.key_ids().is_empty()); // nothing fetched yet
    /// ```
    pub fn key_ids(&self) -> Vec<String> {
        self.read()
            .keys
            .iter()
            .filter_map(|k| k.kid.clone())
            .collect()
    }

    /// Fetch the keys now (waiting up to the timeout, ignoring the refresh
    /// interval) and return how many usable keys the set has. On failure
    /// the cached keys are kept.
    ///
    /// ```no_run
    /// # async fn run() -> zapline::Result<()> {
    /// let jwks = zapline::auth::Jwks::new("https://idp.example.com/jwks.json");
    /// let n = jwks.refresh().await?;
    /// # Ok(()) }
    /// ```
    pub async fn refresh(&self) -> Result<usize> {
        let deadline = Instant::now() + self.settings.timeout;
        let _guard = timeout_at(deadline, self.shared.refresh.lock())
            .await
            .map_err(|_| unavailable())?;
        self.fetch_locked(deadline).await
    }

    /// Fill an empty cache at startup; failures are logged by the fetch.
    pub(crate) async fn warm(&self) {
        if self.read().fetched.is_none() {
            let _ = self.refresh().await;
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Cache> {
        self.shared.cache.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Cache> {
        self.shared.cache.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The key for `kid` (`None`: the set's only key), fetching the set if
    /// the key is unknown and the rate limit allows. `Err` (503) when the
    /// key is unknown and the last fetch failed.
    pub(crate) async fn key(&self, kid: Option<&str>) -> Result<Option<Arc<VerifyKey>>> {
        let now = Instant::now();
        if let Some(key) = self.cached_key(kid, now) {
            return Ok(Some(key));
        }
        // Wait for a fetch already running (it may bring the key), or start
        // one if the rate limit allows.
        let in_flight = self.shared.refresh.try_lock().is_err();
        if in_flight || self.may_refresh(now) {
            let deadline = now + self.settings.timeout;
            let Ok(_guard) = timeout_at(deadline, self.shared.refresh.lock()).await else {
                // A fetch is still running past the timeout.
                return Err(unavailable());
            };
            // Unless another request fetched while this one waited.
            let fetched_since = self.read().attempted.is_some_and(|t| t >= now);
            if !fetched_since && self.may_refresh(now) {
                let _ = self.fetch_locked(deadline).await;
            }
            if let Some(key) = find(&self.read().keys, kid) {
                return Ok(Some(key));
            }
        }
        self.miss()
    }

    /// The key for `kid` from the cache only; an unknown or stale key
    /// starts a background refresh (rate limited).
    pub(crate) fn cached(&self, kid: Option<&str>) -> Result<Option<Arc<VerifyKey>>> {
        let now = Instant::now();
        if let Some(key) = self.cached_key(kid, now) {
            return Ok(Some(key));
        }
        self.refresh_in_background(now);
        self.miss()
    }

    fn cached_key(&self, kid: Option<&str>, now: Instant) -> Option<Arc<VerifyKey>> {
        let (key, stale) = {
            let cache = self.read();
            let key = find(&cache.keys, kid)?;
            let stale = cache
                .fetched
                .is_none_or(|t| now.saturating_duration_since(t) >= self.settings.ttl);
            (key, stale)
        };
        if stale {
            self.refresh_in_background(now);
        }
        Some(key)
    }

    fn miss(&self) -> Result<Option<Arc<VerifyKey>>> {
        if self.read().failed {
            Err(unavailable())
        } else {
            Ok(None)
        }
    }

    fn may_refresh(&self, now: Instant) -> bool {
        self.read()
            .attempted
            .is_none_or(|t| now.saturating_duration_since(t) >= self.settings.min_refresh)
    }

    fn refresh_in_background(&self, now: Instant) {
        if !self.may_refresh(now) || self.shared.background.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.shared.background.store(false, Ordering::Release);
            return;
        };
        let this = self.clone();
        rt.spawn(async move {
            let deadline = Instant::now() + this.settings.timeout;
            if let Ok(_guard) = timeout_at(deadline, this.shared.refresh.lock()).await {
                if this.may_refresh(Instant::now()) {
                    let _ = this.fetch_locked(deadline).await;
                }
            }
            this.shared.background.store(false, Ordering::Release);
        });
    }

    /// Fetch and swap in the set; the caller holds the refresh lock. On any
    /// failure (including the deadline) the cached keys stay.
    async fn fetch_locked(&self, deadline: Instant) -> Result<usize> {
        self.write().attempted = Some(Instant::now());
        let result = match timeout_at(deadline, self.download()).await {
            Ok(Ok(keys)) if keys.is_empty() => Err(Error::service_unavailable(
                "the JWKS has no usable signature keys",
            )),
            Ok(result) => result,
            Err(_) => Err(Error::service_unavailable("the JWKS fetch timed out")),
        };
        let mut cache = self.write();
        match result {
            Ok(keys) => {
                let n = keys.len();
                cache.keys = keys.into();
                cache.fetched = Some(Instant::now());
                cache.failed = false;
                Ok(n)
            }
            Err(e) => {
                cache.failed = true;
                drop(cache);
                crate::tasks::log_warn(format_args!(
                    "jwks: cannot refresh {}: {}; keeping {} cached key(s)",
                    self.location(),
                    e.message(),
                    self.read().keys.len()
                ));
                Err(e)
            }
        }
    }

    fn location(&self) -> String {
        match &self.shared.source {
            Source::Url(url) => url.clone(),
            Source::Discovery { issuer, .. } => format!("the JWKS of {issuer}"),
        }
    }

    async fn download(&self) -> Result<Vec<Arc<VerifyKey>>> {
        let url = self.jwks_uri().await?;
        let doc = get_json(&url, "the JWKS endpoint").await?;
        let keys = doc
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::service_unavailable("the JWKS has no `keys` array"))?;
        Ok(keys
            .iter()
            .filter_map(|k| serde_json::from_value::<Jwk>(k.clone()).ok())
            .filter_map(|jwk| {
                let (alg, key) = jwk_key(&jwk).ok()?;
                self.settings.algorithms.contains(&alg).then(|| {
                    Arc::new(VerifyKey {
                        kid: jwk.common.key_id,
                        alg,
                        key,
                    })
                })
            })
            .collect())
    }

    async fn jwks_uri(&self) -> Result<String> {
        let (issuer, slot) = match &self.shared.source {
            Source::Url(url) => return Ok(url.clone()),
            Source::Discovery { issuer, jwks_uri } => (issuer, jwks_uri),
        };
        if let Some(uri) = slot.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(uri);
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let doc = get_json(&url, "the OpenID discovery endpoint").await?;
        // OpenID Connect Discovery 1.0 §4.3: the issuer must match exactly.
        if doc.get("issuer").and_then(Value::as_str) != Some(issuer.as_str()) {
            return Err(Error::service_unavailable(
                "the OpenID configuration names another issuer",
            ));
        }
        let uri = doc
            .get("jwks_uri")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::service_unavailable("the OpenID configuration has no jwks_uri"))?
            .to_owned();
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(uri.clone());
        Ok(uri)
    }
}

/// The key for `kid`, or with no `kid` the set's only key.
fn find(keys: &[Arc<VerifyKey>], kid: Option<&str>) -> Option<Arc<VerifyKey>> {
    match kid {
        Some(kid) => keys.iter().find(|k| k.kid.as_deref() == Some(kid)).cloned(),
        None => match keys {
            [only] => Some(only.clone()),
            _ => None,
        },
    }
}

async fn get_json(url: &str, what: &str) -> Result<Value> {
    let req = HttpRequest::builder()
        .method(Method::GET)
        .uri(url)
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, concat!("zapline/", env!("CARGO_PKG_VERSION")))
        .body(Full::new(Bytes::new()))
        .map_err(|e| Error::internal(format!("invalid JWKS URL {url:?}")).with_source(e))?;
    let res = fetch(req, what).await?;
    if !res.status.is_success() {
        return Err(Error::service_unavailable(format!(
            "{what} answered {}",
            res.status
        )));
    }
    if res.body.len() > MAX_BODY {
        return Err(Error::service_unavailable(format!(
            "{what} sent more than {MAX_BODY} bytes"
        )));
    }
    serde_json::from_slice(&res.body)
        .map_err(|e| Error::service_unavailable(format!("{what} sent invalid JSON")).with_source(e))
}

fn unavailable() -> Error {
    Error::service_unavailable("cannot verify the token: its signing keys are unavailable")
        .with_code("jwks_unavailable")
}
