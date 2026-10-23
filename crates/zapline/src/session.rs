//! Server-side sessions (feature `cookies`): a signed session-id cookie plus
//! a pluggable [`SessionStore`].
//!
//! The session is loaded from the store only when a handler extracts
//! [`Session`], and saved only when it changed (or its expiry needs
//! extending), so routes that never touch it cost one cookie-header clone.
//!
//! ```no_run
//! use zapline::cookies::Key;
//! use zapline::prelude::*;
//! use zapline::session::{MemoryStore, Session, SessionLayer};
//!
//! async fn login(session: Session) -> Result<&'static str> {
//!     session.regenerate(); // new id on privilege change (prevents fixation)
//!     session.insert("user_id", 42)?;
//!     Ok("logged in")
//! }
//!
//! async fn me(session: Session) -> Result<String> {
//!     let id: u64 = session.get("user_id").ok_or_else(Error::unauthorized)?;
//!     Ok(format!("user {id}"))
//! }
//!
//! async fn logout(session: Session) -> &'static str {
//!     session.destroy();
//!     "bye"
//! }
//!
//! App::new()
//!     .cookie_key(Key::generate())
//!     .wrap(SessionLayer::new(MemoryStore::new()))
//!     .post("/login", login)
//!     .get("/me", me)
//!     .post("/logout", logout)
//!     .run("0.0.0.0:8080")
//!     .unwrap();
//! ```
//!
//! [`Flash`] keeps one-time messages ("Post saved") in the session for the
//! next page; with the `views` feature every template sees them as `flash`.

use std::collections::{BTreeSet, HashMap};
use std::fmt::{self, Write as _};
use std::future::{ready, Future};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use http::header::{COOKIE, SET_COOKIE};
use http::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::auth::CsrfBinding;
use crate::cookies::{random_token, Cookie, Key, SameSite};
use crate::extract::FromRequestParts;
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::{Error, IntoResponse, Request, Response, Result};

/// What a [`SessionStore`] keeps for one session.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRecord {
    /// The session's key/value data.
    pub data: Map<String, Value>,
    /// When the session expires; stores may drop it afterwards.
    pub expires_at: SystemTime,
}

/// Persistence for sessions. Implement it for Redis, a database, etc.
/// Ids are 256-bit random, URL-safe strings.
pub trait SessionStore: Send + Sync + 'static {
    /// The record for `id`, or `None` if unknown or expired.
    fn load(&self, id: &str) -> impl Future<Output = Result<Option<SessionRecord>>> + Send;

    /// Create or replace the record for `id`, kept until `record.expires_at`.
    fn save(&self, id: &str, record: &SessionRecord) -> impl Future<Output = Result<()>> + Send;

    /// Forget `id` (no error if unknown).
    fn delete(&self, id: &str) -> impl Future<Output = Result<()>> + Send;
}

type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Object-safe twin of [`SessionStore`].
trait DynStore: Send + Sync {
    fn load<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<SessionRecord>>;
    fn save<'a>(&'a self, id: &'a str, record: &'a SessionRecord) -> StoreFuture<'a, ()>;
    fn delete<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()>;
}

impl<S: SessionStore> DynStore for S {
    fn load<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<SessionRecord>> {
        Box::pin(SessionStore::load(self, id))
    }
    fn save<'a>(&'a self, id: &'a str, record: &'a SessionRecord) -> StoreFuture<'a, ()> {
        Box::pin(SessionStore::save(self, id, record))
    }
    fn delete<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(SessionStore::delete(self, id))
    }
}

// ---------------------------------------------------------------------------
// MemoryStore

/// An in-process [`SessionStore`] for development, tests and single-node
/// apps. Expired sessions are purged as new ones are saved; when full, the
/// sessions closest to expiry are evicted first.
#[derive(Clone)]
pub struct MemoryStore {
    inner: Arc<Mutex<MemoryInner>>,
    capacity: usize,
}

#[derive(Default)]
struct MemoryInner {
    records: HashMap<String, SessionRecord>,
    by_expiry: BTreeSet<(SystemTime, String)>,
}

impl MemoryInner {
    fn remove(&mut self, id: &str) {
        if let Some(old) = self.records.remove(id) {
            self.by_expiry.remove(&(old.expires_at, id.to_owned()));
        }
    }

    fn purge_expired(&mut self, now: SystemTime) {
        while let Some((at, _)) = self.by_expiry.first() {
            if *at > now {
                break;
            }
            if let Some((_, id)) = self.by_expiry.pop_first() {
                self.records.remove(&id);
            }
        }
    }

    fn evict_soonest(&mut self) {
        if let Some((_, id)) = self.by_expiry.pop_first() {
            self.records.remove(&id);
        }
    }
}

impl MemoryStore {
    /// A store holding at most 100 000 sessions.
    pub fn new() -> Self {
        Self::with_capacity(100_000)
    }

    /// A store holding at most `capacity` sessions (at least 1).
    pub fn with_capacity(capacity: usize) -> Self {
        MemoryStore {
            inner: Arc::default(),
            capacity: capacity.max(1),
        }
    }

    /// Number of stored (possibly expired) sessions.
    pub fn len(&self) -> usize {
        self.lock().records.len()
    }

    /// Whether the store holds no sessions.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, MemoryInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryStore")
            .field("len", &self.len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl SessionStore for MemoryStore {
    fn load(&self, id: &str) -> impl Future<Output = Result<Option<SessionRecord>>> + Send {
        let record = self
            .lock()
            .records
            .get(id)
            .filter(|r| r.expires_at > SystemTime::now())
            .cloned();
        ready(Ok(record))
    }

    fn save(&self, id: &str, record: &SessionRecord) -> impl Future<Output = Result<()>> + Send {
        let mut inner = self.lock();
        inner.remove(id);
        inner.purge_expired(SystemTime::now());
        while inner.records.len() >= self.capacity {
            inner.evict_soonest();
        }
        inner.by_expiry.insert((record.expires_at, id.to_owned()));
        inner.records.insert(id.to_owned(), record.clone());
        ready(Ok(()))
    }

    fn delete(&self, id: &str) -> impl Future<Output = Result<()>> + Send {
        self.lock().remove(id);
        ready(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// SessionLayer

#[derive(Clone)]
struct Config {
    cookie_name: String,
    ttl: Duration,
    secure: bool,
    http_only: bool,
    same_site: SameSite,
    path: String,
    domain: Option<String>,
    key: Option<Key>,
}

/// Middleware that provides [`Session`] to handlers. Install it globally
/// (`App::wrap`) or on a route group (`Routes::wrap`).
///
/// Defaults: cookie `zapline.sid`, 24 h sliding expiry, `Secure`, `HttpOnly`,
/// `SameSite=Lax`, `Path=/`. The cookie holds only the session id, signed
/// with the app's [`Key`] (see [`App::cookie_key`](crate::App::cookie_key))
/// or the one given to [`key`](Self::key).
#[derive(Clone)]
pub struct SessionLayer {
    store: Arc<dyn DynStore>,
    config: Arc<Config>,
}

impl SessionLayer {
    /// Sessions kept in `store`, with the defaults above.
    pub fn new(store: impl SessionStore) -> Self {
        SessionLayer {
            store: Arc::new(store),
            config: Arc::new(Config {
                cookie_name: "zapline.sid".into(),
                ttl: Duration::from_secs(24 * 3600),
                secure: true,
                http_only: true,
                same_site: SameSite::Lax,
                path: "/".into(),
                domain: None,
                key: None,
            }),
        }
    }

    fn config(&mut self) -> &mut Config {
        Arc::make_mut(&mut self.config)
    }

    /// Session cookie name (default `zapline.sid`).
    ///
    /// # Panics
    /// If `name` is empty or not a valid cookie name.
    pub fn cookie_name(mut self, name: &str) -> Self {
        assert!(
            !name.is_empty() && name.bytes().all(is_token_char),
            "invalid session cookie name: {name:?}"
        );
        self.config().cookie_name = name.to_owned();
        self
    }

    /// Idle timeout: sessions expire this long after their last save, and are
    /// re-saved once less than half of it remains (default 24 h).
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.config().ttl = ttl.max(Duration::from_secs(1));
        self
    }

    /// The cookie's `Secure` attribute (default `true`).
    pub fn secure(mut self, secure: bool) -> Self {
        self.config().secure = secure;
        self
    }

    /// The cookie's `HttpOnly` attribute (default `true`).
    pub fn http_only(mut self, http_only: bool) -> Self {
        self.config().http_only = http_only;
        self
    }

    /// The cookie's `SameSite` attribute (default `Lax`).
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        self.config().same_site = same_site;
        self
    }

    /// The cookie's `Path` (default `/`).
    pub fn path(mut self, path: &str) -> Self {
        self.config().path = path.to_owned();
        self
    }

    /// The cookie's `Domain` (default: none, host-only).
    pub fn domain(mut self, domain: &str) -> Self {
        self.config().domain = Some(domain.to_owned());
        self
    }

    /// Sign the session cookie with this key instead of the app's.
    pub fn key(mut self, key: Key) -> Self {
        self.config().key = Some(key);
        self
    }
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

impl Middleware for SessionLayer {
    fn handle(&self, mut req: Request, next: Next) -> BoxFuture<Response> {
        let key = match self
            .config
            .key
            .clone()
            .or_else(|| req.state::<Key>().cloned())
        {
            Some(k) => k,
            None => {
                let err = Error::internal(
                    "SessionLayer needs a cookie key: register one with `App::cookie_key(..)`",
                )
                .with_code("cookie_key_missing");
                return Box::pin(ready(err.into_response()));
            }
        };
        let cookies: Vec<HeaderValue> = req.headers().get_all(COOKIE).iter().cloned().collect();
        let session = Session {
            inner: Arc::new(Shared {
                store: self.store.clone(),
                config: self.config.clone(),
                key,
                state: Mutex::new(State {
                    cookies,
                    ..State::default()
                }),
            }),
        };
        req.extensions_mut().insert(SessionHandle(session.clone()));
        // `Csrf` ran first (it is installed outside this layer): bind its
        // token to this session now.
        let csrf = req.extensions().get::<CsrfBinding>().cloned();
        if let Some(csrf) = csrf {
            if let Err(e) = csrf.bind(&mut req, &session) {
                return Box::pin(ready(e.into_response()));
            }
        }
        Box::pin(async move {
            let mut res = next.run(req).await;
            if let Err(e) = session.commit(&mut res).await {
                return e.into_response();
            }
            res
        })
    }
}

/// Private wrapper so only loaded sessions reach user code.
#[derive(Clone)]
struct SessionHandle(Session);

// ---------------------------------------------------------------------------
// Session

struct Shared {
    store: Arc<dyn DynStore>,
    config: Arc<Config>,
    key: Key,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Raw `Cookie` headers; parsed into `cookie_id` on first load.
    cookies: Vec<HeaderValue>,
    parsed: bool,
    /// The verified session id the client sent. Only a successful store
    /// load decides what it means (until then the request is not
    /// cookie-less).
    cookie_id: Option<String>,
    loaded: bool,
    /// Why loading failed, if it did. Every later access in the request
    /// fails the same way, and the client's cookie is left alone.
    load_error: Option<String>,
    /// The client sent a valid session cookie.
    had_cookie: bool,
    id: Option<String>,
    data: Map<String, Value>,
    expires_at: Option<SystemTime>,
    modified: bool,
    regenerate: bool,
    destroyed: bool,
    /// The expiry is due for extension.
    refresh: bool,
    /// Ids to delete from the store at the end of the request.
    stale: Vec<String>,
    /// The request's CSRF token, bound to this session: rotated on commit
    /// when the session id changes.
    csrf: Option<CsrfBinding>,
    /// The client sent the flash marker cookie: a flash message is waiting.
    flash_marker: bool,
    /// A flash message was queued during this request.
    flash_added: bool,
    /// The request came from htmx (recorded by the `Flash` extractor).
    htmx: bool,
    /// Flash messages consumed in this request (by a template or
    /// `Flash::take`), so every read in the request sees the same list.
    flash_taken: Option<Vec<FlashMessage>>,
}

/// The current session, from [`SessionLayer`]. Cheap to clone; all clones
/// share the same data. Changes are persisted after the handler returns.
///
/// Values are stored as JSON. Without the layer, extraction fails with
/// `500`. When the store fails to load the session, extraction (and
/// [`Session::current`]) fails with `503` (code
/// `session_store_unavailable`) for the rest of the request, and the
/// client's session cookie is left untouched: a store outage never starts a
/// new session over the user's.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Shared>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = self.state();
        f.debug_struct("Session")
            .field("keys", &st.data.keys().collect::<Vec<_>>())
            .field("modified", &st.modified)
            .finish_non_exhaustive()
    }
}

/// What to do after the handler ran.
#[derive(Default)]
struct Plan {
    save: Option<(String, SessionRecord)>,
    delete: Vec<String>,
    remove_cookie: bool,
    /// The session id the client presents after this response.
    next_id: Option<String>,
    /// Set (`true`) or clear (`false`) the flash marker cookie.
    flash_marker: Option<bool>,
}

impl Session {
    /// The loaded session of `req` (for middleware and authenticators, which
    /// cannot use extractors). The returned future does not borrow `req`.
    pub fn current(req: &Request) -> impl Future<Output = Result<Session>> + Send + 'static {
        let handle = req.extensions().get::<SessionHandle>().cloned();
        async move {
            let session = handle.ok_or_else(missing_layer)?.0;
            session.load().await?;
            Ok(session)
        }
    }

    /// The session of `req` without loading it, if a layer is installed.
    pub(crate) fn unloaded(req: &Request) -> Option<Session> {
        req.extensions().get::<SessionHandle>().map(|h| h.0.clone())
    }

    /// The key that signs the session cookie.
    pub(crate) fn cookie_key(&self) -> &Key {
        &self.inner.key
    }

    /// The verified session id from the request's cookie, without touching
    /// the store (the id may be unknown or expired there).
    pub(crate) fn presented_id(&self) -> Option<String> {
        let mut st = self.state();
        self.parse_cookie(&mut st);
        st.cookie_id.clone()
    }

    /// Rotate `csrf` with the session id when the session commits.
    pub(crate) fn attach_csrf(&self, csrf: CsrfBinding) {
        self.state().csrf = Some(csrf);
    }

    fn parse_cookie(&self, st: &mut State) {
        if !st.parsed {
            let cookies = std::mem::take(&mut st.cookies);
            st.cookie_id = self.read_cookie(&cookies);
            st.flash_marker = has_cookie(&cookies, &self.flash_cookie_name());
            st.parsed = true;
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Load from the store (once). A store error fails this and every later
    /// load in the request with `503`: the session is never mistaken for a
    /// missing one, which would start a new session over the client's.
    pub(crate) async fn load(&self) -> Result<()> {
        let id = {
            let mut st = self.state();
            if st.loaded {
                return Ok(());
            }
            if let Some(cause) = &st.load_error {
                return Err(store_unavailable().with_source(cause.clone()));
            }
            self.parse_cookie(&mut st);
            st.cookie_id.clone()
        };
        let record = match &id {
            Some(id) => self.inner.store.load(id).await,
            None => Ok(None),
        };
        let now = SystemTime::now();
        let mut st = self.state();
        if st.loaded {
            return Ok(());
        }
        if let Some(cause) = &st.load_error {
            return Err(store_unavailable().with_source(cause.clone()));
        }
        let record = match record {
            Ok(record) => record,
            Err(e) => {
                st.load_error = Some(e.to_string());
                return Err(store_unavailable().with_source(e));
            }
        };
        st.loaded = true;
        st.had_cookie = id.is_some();
        match (id, record) {
            (Some(id), Some(rec)) if rec.expires_at > now => {
                let ttl = self.inner.config.ttl;
                let remaining = rec.expires_at.duration_since(now).unwrap_or_default();
                st.refresh = remaining < ttl / 2;
                st.id = Some(id);
                st.data = rec.data;
                st.expires_at = Some(rec.expires_at);
            }
            // Unknown or expired: never adopt the client's id (fixation).
            (Some(id), Some(_)) => st.stale.push(id),
            _ => {}
        }
        Ok(())
    }

    /// The verified session id from the request cookies.
    fn read_cookie(&self, headers: &[HeaderValue]) -> Option<String> {
        let mut map = HeaderMap::new();
        for v in headers {
            map.append(COOKIE, v.clone());
        }
        let jar = crate::cookies::SignedCookieJar::from_headers(&map, self.inner.key.clone());
        let id = jar.get(&self.inner.config.cookie_name)?.value().to_owned();
        is_session_id(&id).then_some(id)
    }

    /// A value, deserialized; `None` if missing or of another shape.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let st = self.state();
        st.data.get(key).and_then(|v| T::deserialize(v).ok())
    }

    /// The raw JSON value.
    pub fn get_value(&self, key: &str) -> Option<Value> {
        self.state().data.get(key).cloned()
    }

    /// Store a value (replacing any previous one).
    pub fn insert<T: Serialize>(&self, key: impl Into<String>, value: T) -> Result<()> {
        let value = serde_json::to_value(value)?;
        let mut st = self.state();
        st.data.insert(key.into(), value);
        st.modified = true;
        Ok(())
    }

    /// Remove a value, returning it.
    pub fn remove(&self, key: &str) -> Option<Value> {
        let mut st = self.state();
        let old = st.data.remove(key);
        st.modified |= old.is_some();
        old
    }

    /// Whether `key` is set.
    pub fn contains_key(&self, key: &str) -> bool {
        self.state().data.contains_key(key)
    }

    /// All keys.
    pub fn keys(&self) -> Vec<String> {
        self.state().data.keys().cloned().collect()
    }

    /// Whether the session holds no values.
    pub fn is_empty(&self) -> bool {
        self.state().data.is_empty()
    }

    /// Remove all values, keeping the session.
    pub fn clear(&self) {
        let mut st = self.state();
        st.modified |= !st.data.is_empty();
        st.data.clear();
    }

    /// Issue a new session id, keeping the data. Call it on login and other
    /// privilege changes to prevent session fixation.
    pub fn regenerate(&self) {
        let mut st = self.state();
        st.regenerate = true;
        st.modified = true;
    }

    /// End the session: its data is deleted from the store and the client is
    /// told to drop the cookie. Values inserted afterwards start a new
    /// session.
    pub fn destroy(&self) {
        let mut st = self.state();
        if let Some(id) = st.id.take() {
            st.stale.push(id);
        }
        st.data.clear();
        st.modified = false;
        st.regenerate = false;
        st.refresh = false;
        st.destroyed = true;
    }

    /// The session id, once the session exists in the store.
    pub fn id(&self) -> Option<String> {
        self.state().id.clone()
    }

    /// When the stored session expires (`None` for a new session).
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.state().expires_at
    }

    /// Decide what to persist; runs under the lock.
    fn plan(&self) -> Result<Plan> {
        let mut st = self.state();
        let mut plan = Plan {
            delete: std::mem::take(&mut st.stale),
            next_id: st.cookie_id.clone(),
            ..Plan::default()
        };
        // Not loaded (or the store failed): keep the client's cookie as is.
        if !st.loaded || st.load_error.is_some() {
            return Ok(plan);
        }
        let flash_pending = st.data.contains_key(FLASH_KEY);
        if flash_pending != st.flash_marker {
            plan.flash_marker = Some(flash_pending);
        }
        let persist = (st.modified || st.refresh) && !(st.id.is_none() && st.data.is_empty());
        if persist {
            if st.regenerate || st.id.is_none() {
                if let Some(old) = st.id.take() {
                    plan.delete.push(old);
                }
                st.id = Some(random_token()?);
            }
            let expires_at = SystemTime::now() + self.inner.config.ttl;
            st.expires_at = Some(expires_at);
            st.modified = false;
            st.regenerate = false;
            st.refresh = false;
            let id = st.id.clone().unwrap_or_default();
            let record = SessionRecord {
                data: st.data.clone(),
                expires_at,
            };
            plan.next_id = Some(id.clone());
            plan.save = Some((id, record));
        } else if st.destroyed && st.had_cookie && st.id.is_none() {
            plan.remove_cookie = true;
            plan.next_id = None;
        }
        Ok(plan)
    }

    /// Persist changes and set or clear the cookie.
    async fn commit(&self, res: &mut Response) -> Result<()> {
        self.flash_to_htmx(res);
        let plan = self.plan()?;
        let store = &self.inner.store;
        if let Some((id, record)) = &plan.save {
            store.save(id, record).await?;
        }
        for id in &plan.delete {
            store.delete(id).await?;
        }
        let cookie = match (&plan.save, plan.remove_cookie) {
            (Some((id, _)), _) => Some(self.cookie(id)),
            (None, true) => Some(self.removal_cookie()),
            (None, false) => None,
        };
        if let Some(value) = cookie.and_then(|c| HeaderValue::try_from(c).ok()) {
            res.headers_mut().append(SET_COOKIE, value);
        }
        if let Some(pending) = plan.flash_marker {
            let mut c = self.base_cookie(self.flash_cookie_name(), "1".into());
            if pending {
                let secs = i64::try_from(self.inner.config.ttl.as_secs()).unwrap_or(i64::MAX);
                c.set_max_age(cookie::time::Duration::seconds(secs));
            } else {
                c.make_removal();
            }
            if let Ok(value) = HeaderValue::try_from(c.encoded().to_string()) {
                res.headers_mut().append(SET_COOKIE, value);
            }
        }
        let csrf = self.state().csrf.take();
        if let Some(csrf) = csrf {
            csrf.commit(plan.next_id.as_deref(), res)?;
        }
        Ok(())
    }

    /// Name of the unsigned marker cookie that says a flash message is
    /// waiting. It lets templates skip loading the session otherwise; a
    /// forged one only costs a session load.
    fn flash_cookie_name(&self) -> String {
        format!("{}.flash", self.inner.config.cookie_name)
    }

    /// For an htmx request that queued flash messages and is answered in
    /// place (not redirected), hand them to the page now as an
    /// `HX-Trigger: {"flash": {"messages": [...]}}` event instead of keeping
    /// them for a next page that never comes.
    fn flash_to_htmx(&self, res: &mut Response) {
        let navigates = res.status().is_redirection()
            || ["hx-redirect", "hx-location", "hx-refresh"]
                .iter()
                .any(|h| res.headers().contains_key(*h));
        let messages = {
            let mut st = self.state();
            if !(st.htmx && st.flash_added && st.loaded) || navigates {
                return;
            }
            match st.data.remove(FLASH_KEY) {
                Some(messages) => {
                    st.modified = true;
                    messages
                }
                None => return,
            }
        };
        let mut events = Map::new();
        if let Some(existing) = res.headers().get(HX_TRIGGER).and_then(|v| v.to_str().ok()) {
            match serde_json::from_str::<Value>(existing) {
                Ok(Value::Object(map)) => events = map,
                _ => {
                    for name in existing.split(',').map(str::trim) {
                        if !name.is_empty() {
                            events.insert(name.to_owned(), Value::Null);
                        }
                    }
                }
            }
        }
        let mut detail = Map::new();
        detail.insert("messages".into(), messages);
        events.insert("flash".into(), Value::Object(detail));
        let json = ascii_json(&Value::Object(events).to_string());
        if let Ok(value) = HeaderValue::try_from(json) {
            res.headers_mut().insert(HX_TRIGGER, value);
        }
    }

    fn base_cookie(&self, name: String, value: String) -> Cookie<'static> {
        let cfg = &self.inner.config;
        let mut c = Cookie::new(name, value);
        c.set_path(cfg.path.clone());
        if let Some(d) = &cfg.domain {
            c.set_domain(d.clone());
        }
        c.set_secure(cfg.secure);
        c.set_http_only(cfg.http_only);
        c.set_same_site(cfg.same_site);
        c
    }

    /// `Set-Cookie` value carrying the signed id.
    fn cookie(&self, id: &str) -> String {
        let mut jar = cookie::CookieJar::new();
        let mut c = self.base_cookie(self.inner.config.cookie_name.clone(), id.to_owned());
        let secs = i64::try_from(self.inner.config.ttl.as_secs()).unwrap_or(i64::MAX);
        c.set_max_age(cookie::time::Duration::seconds(secs));
        jar.signed_mut(&self.inner.key).add(c);
        jar.delta()
            .next()
            .map(|c| c.encoded().to_string())
            .unwrap_or_default()
    }

    fn removal_cookie(&self) -> String {
        let mut c = self.base_cookie(self.inner.config.cookie_name.clone(), String::new());
        c.make_removal();
        c.encoded().to_string()
    }
}

/// Ids are base64url of 32 random bytes.
fn is_session_id(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn store_unavailable() -> Error {
    Error::service_unavailable("session store unavailable").with_code("session_store_unavailable")
}

fn missing_layer() -> Error {
    Error::internal("no session: install `SessionLayer` to use `Session`")
        .with_code("session_layer_missing")
}

impl FromRequestParts for Session {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        Session::current(req).await
    }
}

// ---------------------------------------------------------------------------
// Flash messages

/// Session key holding the queued flash messages.
const FLASH_KEY: &str = "_flash";
const HX_TRIGGER: &str = "hx-trigger";

/// How a [`FlashMessage`] should look: templates usually turn it into a
/// CSS class (`flash-{{ m.level }}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlashLevel {
    /// Something worked: "Post saved".
    Success,
    /// Something failed: "Could not send the email".
    Error,
    /// Neutral news: "You have 3 new messages".
    Info,
    /// Needs attention: "Your trial ends tomorrow".
    Warning,
}

impl FlashLevel {
    /// `"success"`, `"error"`, `"info"` or `"warning"`.
    ///
    /// ```
    /// # use zapline::session::FlashLevel;
    /// assert_eq!(FlashLevel::Success.as_str(), "success");
    /// ```
    pub fn as_str(self) -> &'static str {
        match self {
            FlashLevel::Success => "success",
            FlashLevel::Error => "error",
            FlashLevel::Info => "info",
            FlashLevel::Warning => "warning",
        }
    }
}

/// One message queued with [`Flash`]. Serializes as
/// `{"level": "success", "message": "Post saved"}`.
///
/// ```
/// # use zapline::session::{FlashLevel, FlashMessage};
/// let m = FlashMessage { level: FlashLevel::Info, message: "Welcome back".into() };
/// assert_eq!(m.level.as_str(), "info");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlashMessage {
    /// Its kind.
    pub level: FlashLevel,
    /// The text, shown as is (HTML-escaped by templates).
    pub message: String,
}

/// One-time messages for the next page ("Post saved", "Wrong password"),
/// kept in the [`Session`]: queue one before redirecting, and the page the
/// browser lands on shows it once.
///
/// With the `views` feature every template sees the waiting messages as
/// `flash` (reading them there uses them up), with no setup:
///
/// ```jinja
/// {% for m in flash %}<p class="flash flash-{{ m.level }}">{{ m.message }}</p>{% endfor %}
/// ```
///
/// Messages stay queued until something reads them: a template that uses
/// `flash`, or [`take`](Self::take). For an htmx request answered in place
/// (no redirect), messages queued during it are sent right away instead,
/// as an `HX-Trigger: {"flash": {"messages": [...]}}` event the page can
/// show as toasts. Needs [`SessionLayer`]; extraction fails like
/// [`Session`]'s otherwise.
///
/// ```
/// use zapline::prelude::*;
/// use zapline::session::Flash;
///
/// async fn publish(flash: Flash, Path(id): Path<i64>) -> Response {
///     // ... publish post `id` ...
///     flash.success("Post published");
///     // `303 See Other`, or `HX-Redirect` for htmx.
///     flash.redirect(&format!("/posts/{id}"))
/// }
/// # let _ = App::new().post("/posts/{id}/publish", publish);
/// ```
#[derive(Clone, Debug)]
pub struct Flash {
    session: Session,
    htmx: bool,
}

impl Flash {
    /// Queue a success message.
    ///
    /// ```
    /// # async fn h(flash: zapline::session::Flash) {
    /// flash.success("Saved").info("Changes go live in a minute");
    /// # }
    /// ```
    pub fn success(&self, message: impl Into<String>) -> &Self {
        self.push(FlashLevel::Success, message)
    }

    /// Queue an error message.
    ///
    /// ```
    /// # async fn h(flash: zapline::session::Flash) {
    /// flash.error("Could not send the invitation");
    /// # }
    /// ```
    pub fn error(&self, message: impl Into<String>) -> &Self {
        self.push(FlashLevel::Error, message)
    }

    /// Queue an informational message.
    ///
    /// ```
    /// # async fn h(flash: zapline::session::Flash) {
    /// flash.info("You have 3 new messages");
    /// # }
    /// ```
    pub fn info(&self, message: impl Into<String>) -> &Self {
        self.push(FlashLevel::Info, message)
    }

    /// Queue a warning.
    ///
    /// ```
    /// # async fn h(flash: zapline::session::Flash) {
    /// flash.warning("Your trial ends tomorrow");
    /// # }
    /// ```
    pub fn warning(&self, message: impl Into<String>) -> &Self {
        self.push(FlashLevel::Warning, message)
    }

    /// Queue a message of any level.
    ///
    /// ```
    /// # use zapline::session::FlashLevel;
    /// # async fn h(flash: zapline::session::Flash) {
    /// flash.push(FlashLevel::Info, "Signed in");
    /// # }
    /// ```
    pub fn push(&self, level: FlashLevel, message: impl Into<String>) -> &Self {
        self.session.push_flash(FlashMessage {
            level,
            message: message.into(),
        });
        self
    }

    /// The waiting messages, used up: later requests won't see them. Every
    /// call in the same request returns the same list. Templates do this
    /// for you through `flash`.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # use zapline::session::{Flash, FlashMessage};
    /// async fn notices(flash: Flash) -> Json<Vec<FlashMessage>> {
    ///     Json(flash.take())
    /// }
    /// # let _ = App::new().get("/notices", notices);
    /// ```
    pub fn take(&self) -> Vec<FlashMessage> {
        self.session.take_flash()
    }

    /// Go to `to` after a form post: `303 See Other` for a plain browser
    /// request; for htmx, `HX-Redirect`, so htmx loads the whole page
    /// (where the flash shows) instead of swapping it into the form's
    /// target.
    ///
    /// ```
    /// # async fn h(flash: zapline::session::Flash) -> zapline::Response {
    /// flash.success("Welcome!").redirect("/")
    /// # }
    /// ```
    pub fn redirect(&self, to: &str) -> Response {
        if !self.htmx {
            return crate::Redirect::to(to).into_response();
        }
        match HeaderValue::try_from(to) {
            Ok(location) => {
                let mut res = ().into_response();
                res.headers_mut().insert("hx-redirect", location);
                res
            }
            Err(_) => Error::internal("invalid redirect location")
                .with_code("invalid_redirect")
                .into_response(),
        }
    }
}

impl FromRequestParts for Flash {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        let htmx = req
            .headers()
            .get("hx-request")
            .is_some_and(|v| v.as_bytes() == b"true");
        let session = Session::current(req).await?;
        if htmx {
            session.state().htmx = true;
        }
        Ok(Flash { session, htmx })
    }
}

impl Session {
    /// Whether a flash message may be waiting, without touching the store:
    /// the marker cookie before loading, the data afterwards.
    #[cfg(feature = "views")]
    pub(crate) fn flash_pending(&self) -> bool {
        let st = self.state();
        if st.parsed {
            st.flash_marker || st.data.contains_key(FLASH_KEY) || st.flash_taken.is_some()
        } else {
            has_cookie(&st.cookies, &self.flash_cookie_name())
        }
    }

    fn push_flash(&self, message: FlashMessage) {
        let mut entry = Map::new();
        entry.insert("level".into(), Value::from(message.level.as_str()));
        entry.insert("message".into(), Value::from(message.message));
        let mut st = self.state();
        let queue = st
            .data
            .entry(FLASH_KEY)
            .or_insert_with(|| Value::Array(Vec::new()));
        if !queue.is_array() {
            *queue = Value::Array(Vec::new());
        }
        if let Value::Array(list) = queue {
            list.push(Value::Object(entry));
        }
        st.modified = true;
        st.flash_added = true;
    }

    /// Use up the waiting flash messages (see [`Flash::take`]).
    pub(crate) fn take_flash(&self) -> Vec<FlashMessage> {
        let mut st = self.state();
        let fresh: Vec<FlashMessage> = match st.data.remove(FLASH_KEY) {
            Some(Value::Array(list)) => {
                st.modified = true;
                list.into_iter()
                    .filter_map(|v| FlashMessage::deserialize(v).ok())
                    .collect()
            }
            Some(_) => {
                st.modified = true;
                Vec::new()
            }
            None => Vec::new(),
        };
        let taken = st.flash_taken.get_or_insert_with(Vec::new);
        taken.extend(fresh);
        taken.clone()
    }
}

/// Whether raw `Cookie` headers carry a cookie called `name`.
fn has_cookie(headers: &[HeaderValue], name: &str) -> bool {
    headers
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .any(|pair| pair.split('=').next().map(str::trim) == Some(name))
}

/// `json` with every non-ASCII character as a `\uXXXX` escape: browsers
/// read header values as Latin-1, so raw UTF-8 would turn into mojibake.
/// Non-ASCII only occurs inside JSON strings, where escapes are valid.
fn ascii_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out
}
