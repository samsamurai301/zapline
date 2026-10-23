//! Sessions: lifecycle over Set-Cookie/Cookie round trips, lazy loading,
//! regeneration, destruction, expiry, tampering and stores.
#![cfg(feature = "cookies")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use zapline::auth::{Authenticate, CurrentUser};
use zapline::cookies::{CookieJar, Key, SameSite};
use zapline::prelude::*;
use zapline::session::{MemoryStore, Session, SessionLayer, SessionRecord, SessionStore};
use zapline::test::{TestClient, TestResponse};

const COOKIE: &str = "zapline.sid";

fn key() -> Key {
    Key::from(&[42u8; 64])
}

/// A `MemoryStore` that counts calls and can be told to fail.
#[derive(Clone, Default)]
struct CountingStore {
    inner: MemoryStore,
    loads: Arc<AtomicUsize>,
    saves: Arc<AtomicUsize>,
    deletes: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
}

impl SessionStore for CountingStore {
    async fn load(&self, id: &str) -> Result<Option<SessionRecord>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load(id).await
    }
    async fn save(&self, id: &str, record: &SessionRecord) -> Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::service_unavailable("store down"));
        }
        self.saves.fetch_add(1, Ordering::SeqCst);
        self.inner.save(id, record).await
    }
    async fn delete(&self, id: &str) -> Result<()> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete(id).await
    }
}

impl CountingStore {
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.loads.load(Ordering::SeqCst),
            self.saves.load(Ordering::SeqCst),
            self.deletes.load(Ordering::SeqCst),
        )
    }
}

/// A `MemoryStore` whose next `fail_loads` loads fail (a transient outage).
#[derive(Clone, Default)]
struct FlakyStore {
    inner: MemoryStore,
    fail_loads: Arc<AtomicUsize>,
    loads: Arc<AtomicUsize>,
}

impl SessionStore for FlakyStore {
    async fn load(&self, id: &str) -> Result<Option<SessionRecord>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        // Decrement while non-zero (a CAS loop: `fetch_update` is deprecated on
        // current stable and its replacement `try_update` is newer than the MSRV).
        let mut left = self.fail_loads.load(Ordering::SeqCst);
        let fail = loop {
            if left == 0 {
                break false;
            }
            match self.fail_loads.compare_exchange(
                left,
                left - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break true,
                Err(current) => left = current,
            }
        };
        if fail {
            return Err(Error::service_unavailable("store timeout"));
        }
        self.inner.load(id).await
    }
    async fn save(&self, id: &str, record: &SessionRecord) -> Result<()> {
        self.inner.save(id, record).await
    }
    async fn delete(&self, id: &str) -> Result<()> {
        self.inner.delete(id).await
    }
}

/// A minimal cookie-keeping client.
#[derive(Clone)]
struct Browser {
    client: TestClient,
    cookies: Arc<Mutex<BTreeMap<String, String>>>,
}

impl Browser {
    fn new(client: TestClient) -> Self {
        Browser {
            client,
            cookies: Arc::default(),
        }
    }

    fn cookie_header(&self) -> String {
        let jar = self.cookies.lock().unwrap();
        jar.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn sid(&self) -> Option<String> {
        self.cookies.lock().unwrap().get(COOKIE).cloned()
    }

    /// The session id inside the signed cookie (after the 44-char MAC).
    fn session_id(&self) -> String {
        let sid = self.sid().unwrap();
        let decoded = percent_encoding::percent_decode_str(&sid)
            .decode_utf8()
            .unwrap()
            .into_owned();
        decoded[44..].to_owned()
    }

    fn set_sid(&self, value: &str) {
        self.cookies
            .lock()
            .unwrap()
            .insert(COOKIE.into(), value.into());
    }

    async fn send(&self, method: Method, path: &str) -> TestResponse {
        let mut req = self.client.request(method, path);
        let cookies = self.cookie_header();
        if !cookies.is_empty() {
            req = req.header("cookie", cookies);
        }
        let res = req.send().await;
        let mut jar = self.cookies.lock().unwrap();
        for v in res.headers().get_all("set-cookie") {
            let v = v.to_str().unwrap();
            let (name, value) = v.split(';').next().unwrap().split_once('=').unwrap();
            if v.contains("Max-Age=0") {
                jar.remove(name);
            } else {
                jar.insert(name.into(), value.into());
            }
        }
        res
    }

    async fn get(&self, path: &str) -> TestResponse {
        self.send(Method::GET, path).await
    }

    async fn post(&self, path: &str) -> TestResponse {
        self.send(Method::POST, path).await
    }
}

fn set_cookies(r: &TestResponse) -> Vec<String> {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

async fn login(session: Session) -> Result<&'static str> {
    session.regenerate();
    session.insert("user", "ada")?;
    session.insert("visits", 0u32)?;
    Ok("logged in")
}

async fn me(session: Session) -> Result<String> {
    session
        .get::<String>("user")
        .ok_or_else(|| Error::unauthorized().with_code("no_session"))
}

async fn visit(session: Session) -> Result<String> {
    let n = session.get::<u32>("visits").unwrap_or(0) + 1;
    session.insert("visits", n)?;
    Ok(n.to_string())
}

async fn logout(session: Session) -> &'static str {
    session.destroy();
    "bye"
}

fn app(layer: SessionLayer) -> App {
    App::new()
        .cookie_key(key())
        .wrap(layer)
        .post("/login", login)
        .get("/me", me)
        .post("/visit", visit)
        .post("/logout", logout)
        .get("/untouched", || async { "no session here" })
}

#[tokio::test]
async fn full_lifecycle() {
    let store = CountingStore::default();
    let b = Browser::new(app(SessionLayer::new(store.clone())).test());

    // No cookie: nothing is loaded or saved.
    let r = b.get("/me").await;
    assert_eq!(r.status(), 401);
    assert!(set_cookies(&r).is_empty());
    assert_eq!(store.counts(), (0, 0, 0));

    // Login creates the session and sets the cookie.
    let r = b.post("/login").await;
    assert_eq!(r.text(), "logged in");
    let set = set_cookies(&r);
    assert_eq!(set.len(), 1);
    for attr in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=86400",
    ] {
        assert!(set[0].contains(attr), "{attr} missing in {}", set[0]);
    }
    assert!(set[0].starts_with("zapline.sid="));
    assert_eq!(store.counts(), (0, 1, 0));
    assert_eq!(store.inner.len(), 1);

    // The cookie identifies the session; read-only access saves nothing.
    let r = b.get("/me").await;
    assert_eq!(r.text(), "ada");
    assert!(set_cookies(&r).is_empty());
    assert_eq!(store.counts(), (1, 1, 0));

    // Modifications are persisted (same id, cookie refreshed).
    let sid = b.sid().unwrap();
    assert_eq!(b.post("/visit").await.text(), "1");
    assert_eq!(b.post("/visit").await.text(), "2");
    assert_eq!(b.sid().unwrap(), sid);
    assert_eq!(store.counts(), (3, 3, 0));

    // Routes that never extract the session never touch the store.
    assert_eq!(b.get("/untouched").await.text(), "no session here");
    assert_eq!(store.counts(), (3, 3, 0));

    // Logout deletes the session and the cookie.
    let r = b.post("/logout").await;
    assert_eq!(r.text(), "bye");
    let set = set_cookies(&r);
    assert!(
        set[0].starts_with("zapline.sid=;") && set[0].contains("Max-Age=0"),
        "{set:?}"
    );
    assert!(b.sid().is_none());
    assert!(store.inner.is_empty());
    assert_eq!(b.get("/me").await.status(), 401);

    // Replaying the old cookie does not resurrect the session.
    b.set_sid(&sid);
    assert_eq!(b.get("/me").await.status(), 401);
}

#[tokio::test]
async fn regenerate_issues_a_new_id_and_drops_the_old() {
    let store = MemoryStore::new();
    let b = Browser::new(app(SessionLayer::new(store.clone())).test());
    b.post("/login").await;
    let first = b.sid().unwrap();
    b.post("/visit").await;

    // Logging in again (privilege change) rotates the id but keeps data.
    b.post("/login").await;
    let second = b.sid().unwrap();
    assert_ne!(first, second);
    assert_eq!(store.len(), 1);
    assert_eq!(b.get("/me").await.text(), "ada");

    // The old id is gone.
    let old = Browser::new(app(SessionLayer::new(store.clone())).test());
    old.set_sid(&first);
    assert_eq!(old.get("/me").await.status(), 401);
}

#[tokio::test]
async fn client_chosen_ids_are_never_adopted() {
    // A correctly signed cookie for an id the server never issued
    // (session fixation attempt).
    let planted = "A".repeat(43);
    let jar = CookieJar::new()
        .into_signed(key())
        .add((COOKIE, planted.clone()));
    let signed = jar.delta().next().unwrap().value().to_owned();

    let store = MemoryStore::new();
    let b = Browser::new(app(SessionLayer::new(store.clone())).test());
    b.set_sid(&signed);
    assert_eq!(b.get("/me").await.status(), 401);
    assert_eq!(b.post("/visit").await.text(), "1");
    let issued = b.sid().unwrap();
    assert_ne!(issued, signed);
    assert!(!issued.ends_with(&planted));
    assert_eq!(store.len(), 1);
}

#[tokio::test]
async fn tampered_or_unsigned_cookies_are_ignored() {
    let store = CountingStore::default();
    let b = Browser::new(app(SessionLayer::new(store.clone())).test());
    b.post("/login").await;
    let good = b.sid().unwrap();

    for bad in [
        format!("{}x", &good[..good.len() - 1]),
        b.session_id(), // the bare id without its signature
        "garbage".to_owned(),
        String::new(),
    ] {
        let b2 = Browser::new(b.client.clone());
        b2.set_sid(&bad);
        assert_eq!(b2.get("/me").await.status(), 401, "{bad}");
    }
    // Forged cookies never reach the store.
    assert_eq!(store.loads.load(Ordering::SeqCst), 0);

    // A cookie signed with another key is rejected too.
    let other = Browser::new(
        App::new()
            .cookie_key(Key::generate())
            .wrap(SessionLayer::new(store.inner.clone()))
            .get("/me", me)
            .test(),
    );
    other.set_sid(&good);
    assert_eq!(other.get("/me").await.status(), 401);
}

#[tokio::test]
async fn expired_sessions_are_dropped() {
    let store = MemoryStore::new();
    let b = Browser::new(app(SessionLayer::new(store.clone())).test());
    b.post("/login").await;
    assert_eq!(b.get("/me").await.text(), "ada");

    // Expire it in the store.
    let id = b.session_id();
    let mut record = store.load(&id).await.unwrap().unwrap();
    record.expires_at = SystemTime::now() - Duration::from_secs(1);
    store.save(&id, &record).await.unwrap();
    assert!(store.load(&id).await.unwrap().is_none());

    assert_eq!(b.get("/me").await.status(), 401);
    // A new session starts with a new id.
    assert_eq!(b.post("/visit").await.text(), "1");
    assert_ne!(b.session_id(), id);
}

#[tokio::test]
async fn ttl_is_extended_when_half_used() {
    let store = CountingStore::default();
    let layer = SessionLayer::new(store.clone()).ttl(Duration::from_secs(3600));
    let b = Browser::new(app(layer).test());
    let r = b.post("/login").await;
    assert!(set_cookies(&r)[0].contains("Max-Age=3600"));
    let id = b.session_id();
    assert_eq!(id.len(), 43);

    // Fresh session: reading does not re-save.
    b.get("/me").await;
    assert_eq!(store.counts().1, 1);

    // Less than half the TTL left: the next access extends it.
    let mut record = store.inner.load(&id).await.unwrap().unwrap();
    record.expires_at = SystemTime::now() + Duration::from_secs(600);
    store.inner.save(&id, &record).await.unwrap();
    let r = b.get("/me").await;
    assert_eq!(r.text(), "ada");
    assert_eq!(set_cookies(&r).len(), 1);
    assert_eq!(store.counts().1, 2);
    let renewed = store.inner.load(&id).await.unwrap().unwrap().expires_at;
    let left = renewed.duration_since(SystemTime::now()).unwrap();
    assert!(left > Duration::from_secs(3500), "{left:?}");
}

#[tokio::test]
async fn session_api() {
    async fn ops(session: Session) -> Result<Json<Value>> {
        session.insert("a", 1)?;
        session.insert("b", json!({"x": [1, 2]}))?;
        session.insert("c", "three")?;
        let removed = session.remove("c");
        let mut keys = session.keys();
        keys.sort();
        let snapshot = json!({
            "a": session.get::<u8>("a"),
            "a_as_string": session.get::<String>("a"),
            "b": session.get_value("b"),
            "removed": removed,
            "keys": keys,
            "has_c": session.contains_key("c"),
            "id_before_save": session.id(),
        });
        session.clear();
        assert!(session.is_empty());
        session.insert("after_clear", true)?;
        Ok(Json(snapshot))
    }
    let c = App::new()
        .cookie_key(key())
        .wrap(SessionLayer::new(MemoryStore::new()))
        .post("/ops", ops)
        .test();
    let r = c.post("/ops").send().await;
    assert_eq!(
        r.json::<Value>(),
        json!({
            "a": 1, "a_as_string": null, "b": {"x": [1, 2]}, "removed": "three",
            "keys": ["a", "b"], "has_c": false, "id_before_save": null
        })
    );
    assert_eq!(set_cookies(&r).len(), 1);
}

#[tokio::test]
async fn destroy_then_insert_starts_a_new_session() {
    async fn switch(session: Session) -> Result<&'static str> {
        session.destroy();
        session.insert("user", "bob")?;
        Ok("switched")
    }
    let store = MemoryStore::new();
    let b = Browser::new(
        app(SessionLayer::new(store.clone()))
            .post("/switch", switch)
            .test(),
    );
    b.post("/login").await;
    let first = b.sid().unwrap();
    b.post("/switch").await;
    assert_ne!(b.sid().unwrap(), first);
    assert_eq!(b.get("/me").await.text(), "bob");
    assert_eq!(store.len(), 1);
}

#[tokio::test]
async fn empty_new_sessions_are_not_stored() {
    async fn noop(session: Session) -> &'static str {
        session.regenerate();
        session.clear();
        "ok"
    }
    let store = MemoryStore::new();
    let c = App::new()
        .cookie_key(key())
        .wrap(SessionLayer::new(store.clone()))
        .post("/", noop)
        .test();
    let r = c.post("/").send().await;
    assert!(set_cookies(&r).is_empty());
    assert!(store.is_empty());
}

#[tokio::test]
async fn cookie_options() {
    let layer = SessionLayer::new(MemoryStore::new())
        .cookie_name("app_session")
        .secure(false)
        .http_only(false)
        .same_site(SameSite::Strict)
        .path("/app")
        .domain("example.com")
        .ttl(Duration::from_secs(60));
    let c = App::new()
        .cookie_key(key())
        .wrap(layer)
        .post("/login", login)
        .test();
    let set = set_cookies(&c.post("/login").send().await);
    let s = &set[0];
    assert!(s.starts_with("app_session="), "{s}");
    for attr in [
        "SameSite=Strict",
        "Path=/app",
        "Domain=example.com",
        "Max-Age=60",
    ] {
        assert!(s.contains(attr), "{attr} missing in {s}");
    }
    assert!(!s.contains("Secure") && !s.contains("HttpOnly"), "{s}");
}

#[tokio::test]
async fn keys_and_missing_layer() {
    // No key anywhere: configuration error.
    let c = App::new()
        .wrap(SessionLayer::new(MemoryStore::new()))
        .get("/", me)
        .test();
    let r = c.get("/").send().await;
    assert_eq!(
        (r.status().as_u16(), r.json::<Value>()["code"].clone()),
        (500, json!("cookie_key_missing"))
    );

    // A layer-specific key works without an app key.
    let b = Browser::new(
        App::new()
            .wrap(SessionLayer::new(MemoryStore::new()).key(Key::generate()))
            .post("/login", login)
            .get("/me", me)
            .test(),
    );
    b.post("/login").await;
    assert_eq!(b.get("/me").await.text(), "ada");

    // Extracting a session without the layer.
    let c = App::new().get("/", me).test();
    let r = c.get("/").send().await;
    assert_eq!(r.status(), 500);
    assert_eq!(r.json::<Value>()["code"], "session_layer_missing");
}

#[tokio::test]
async fn store_failures_surface_as_errors() {
    let store = CountingStore::default();
    let c = app(SessionLayer::new(store.clone())).test();
    store.fail.store(true, Ordering::SeqCst);
    let r = c.post("/login").send().await;
    assert_eq!(r.status(), 503);
    assert!(set_cookies(&r).is_empty());
}

#[tokio::test]
async fn failed_loads_never_replace_the_cookie() {
    let store = FlakyStore::default();
    // Public pages that greet signed-in users: the authenticator's errors
    // are swallowed there, and the handler uses the session afterwards.
    let auth = Authenticate::new(|req: &Request| {
        let session = Session::current(req);
        async move { Ok(session.await?.get::<String>("user")) }
    });
    let pages = Routes::new()
        .post("/home", visit)
        .allow_anonymous()
        .get("/about", || async { "about" })
        .allow_anonymous()
        .wrap(auth);
    let b = Browser::new(
        app(SessionLayer::new(store.clone()))
            .nest("/pages", pages)
            .test(),
    );
    b.post("/login").await;
    let sid = b.sid().unwrap();
    let loads = || store.loads.load(Ordering::SeqCst);

    // The store's first load fails (in the authenticator). The handler's
    // access fails too, instead of starting a new, empty session that would
    // overwrite the user's cookie.
    store.fail_loads.store(1, Ordering::SeqCst);
    let before = loads();
    let r = b.post("/pages/home").await;
    assert_eq!(r.status(), 503);
    assert_eq!(r.json::<Value>()["code"], "session_store_unavailable");
    assert!(set_cookies(&r).is_empty(), "{:?}", set_cookies(&r));
    assert_eq!(b.sid().unwrap(), sid);
    // Later accesses in the same request don't hit the store again.
    assert_eq!(loads(), before + 1);

    // A failure the handler never sees still leaves the cookie alone.
    store.fail_loads.store(1, Ordering::SeqCst);
    let r = b.get("/pages/about").await;
    assert_eq!(r.text(), "about");
    assert!(set_cookies(&r).is_empty());

    // Direct accesses: 503, no Set-Cookie, nothing deleted or rewritten.
    for path in ["/visit", "/logout", "/login"] {
        store.fail_loads.store(1, Ordering::SeqCst);
        let r = b.post(path).await;
        assert_eq!(r.status(), 503, "{path}");
        assert_eq!(r.json::<Value>()["code"], "session_store_unavailable");
        assert!(set_cookies(&r).is_empty(), "{path}: {:?}", set_cookies(&r));
    }
    store.fail_loads.store(1, Ordering::SeqCst);
    assert_eq!(b.get("/me").await.status(), 503);

    // Once the store is back, the user is still signed in, with their data.
    assert_eq!(b.sid().unwrap(), sid);
    assert_eq!(store.inner.len(), 1);
    assert_eq!(b.get("/me").await.text(), "ada");
    assert_eq!(b.post("/pages/home").await.text(), "1");
    assert_eq!(b.sid().unwrap(), sid);
}

#[tokio::test]
async fn sessions_power_authenticators() {
    let auth = Authenticate::new(|req: &Request| {
        let session = Session::current(req);
        async move { Ok(session.await?.get::<String>("user")) }
    })
    .required();
    let api = Routes::new()
        .get(
            "/me",
            |CurrentUser(u): CurrentUser<String>| async move { u },
        )
        .wrap(auth);
    let b = Browser::new(
        App::new()
            .cookie_key(key())
            .wrap(SessionLayer::new(MemoryStore::new()))
            .post("/login", login)
            .nest("/api", api)
            .test(),
    );
    assert_eq!(b.get("/api/me").await.status(), 401);
    b.post("/login").await;
    assert_eq!(b.get("/api/me").await.text(), "ada");
}

#[tokio::test]
async fn memory_store_is_bounded_and_expiry_aware() {
    let store = MemoryStore::with_capacity(2);
    let now = SystemTime::now();
    let rec = |secs: u64| SessionRecord {
        data: Default::default(),
        expires_at: now + Duration::from_secs(secs),
    };
    store.save("a", &rec(300)).await.unwrap();
    store.save("b", &rec(100)).await.unwrap();
    store.save("c", &rec(200)).await.unwrap();
    // Full: the session closest to expiry ("b") was evicted.
    assert_eq!(store.len(), 2);
    assert!(store.load("b").await.unwrap().is_none());
    assert!(store.load("a").await.unwrap().is_some());

    // Re-saving an id replaces it without growing.
    store.save("a", &rec(400)).await.unwrap();
    assert_eq!(store.len(), 2);

    // Expired records are invisible and purged on the next save.
    let expired = SessionRecord {
        data: Default::default(),
        expires_at: now - Duration::from_secs(1),
    };
    store.save("c", &expired).await.unwrap();
    assert!(store.load("c").await.unwrap().is_none());
    store.save("d", &rec(50)).await.unwrap();
    assert_eq!(store.len(), 2);
    store.delete("a").await.unwrap();
    store.delete("missing").await.unwrap();
    assert_eq!(store.len(), 1);
    assert!(format!("{store:?}").contains("len: 1"));
}

// ---- flash messages ---------------------------------------------------------------

fn flash_app(store: CountingStore) -> App {
    use zapline::session::Flash;
    App::new()
        .cookie_key(key())
        .wrap(SessionLayer::new(store))
        .post("/save", |flash: Flash| async move {
            flash.success("Saved").warning("Check the date");
            flash.redirect("/next")
        })
        .post("/inline", |flash: Flash| async move {
            flash.success("Gespeichert ✓");
            (
                [(
                    header::HeaderName::from_static("hx-trigger"),
                    HeaderValue::from_static("saved"),
                )],
                "<li>row</li>",
            )
        })
        .get("/next", |flash: Flash| async move { Json(flash.take()) })
        .get("/plain", || async { "no session here" })
}

#[tokio::test]
async fn a_flash_survives_exactly_one_redirect() {
    let store = CountingStore::default();
    let browser = flash_app(store.clone()).test().with_cookies();
    let res = browser.post("/save").send().await;
    assert_eq!(res.status(), 303);
    assert_eq!(res.header("location"), Some("/next"));
    // A marker cookie says a message is waiting.
    let marker = res.cookie("zapline.sid.flash").unwrap();
    assert_eq!(marker.value(), "1");
    assert!(marker.http_only());

    let res = browser.get("/next").send().await;
    assert_eq!(
        res.json::<Value>(),
        json!([
            {"level": "success", "message": "Saved"},
            {"level": "warning", "message": "Check the date"}
        ])
    );
    assert_eq!(res.cookie("zapline.sid.flash").unwrap().max_age(), Some(0));
    assert_eq!(browser.get("/next").send().await.json::<Value>(), json!([]));

    // Routes that never touch the session never load it.
    let (loads, _, _) = store.counts();
    browser.get("/plain").send().await;
    assert_eq!(store.counts().0, loads);
}

#[tokio::test]
async fn htmx_gets_the_flash_redirected_or_as_an_event() {
    let browser = flash_app(CountingStore::default()).test().with_cookies();
    // Redirected: `HX-Redirect`, and the message waits for that page.
    let res = browser
        .post("/save")
        .header("hx-request", "true")
        .send()
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.header("hx-redirect"), Some("/next"));
    assert_eq!(res.header("hx-trigger"), None);
    let shown: Value = browser.get("/next").send().await.json();
    assert_eq!(shown.as_array().unwrap().len(), 2);

    // Answered in place: sent now as an event, merged with the handler's own,
    // non-ASCII escaped; nothing is left for the next page.
    let res = browser
        .post("/inline")
        .header("hx-request", "true")
        .send()
        .await;
    assert_eq!(res.text(), "<li>row</li>");
    let trigger = res.header("hx-trigger").unwrap();
    assert!(trigger.is_ascii(), "{trigger}");
    assert_eq!(
        serde_json::from_str::<Value>(trigger).unwrap(),
        json!({
            "saved": null,
            "flash": {"messages": [{"level": "success", "message": "Gespeichert ✓"}]}
        })
    );
    assert_eq!(browser.get("/next").send().await.json::<Value>(), json!([]));

    // Without htmx the same handler keeps the message for the next page.
    browser.post("/inline").send().await;
    let shown: Value = browser.get("/next").send().await.json();
    assert_eq!(shown[0]["message"], "Gespeichert ✓");
}

#[tokio::test]
async fn sessions_work_over_a_real_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn roundtrip(addr: std::net::SocketAddr, req: String) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app(SessionLayer::new(MemoryStore::new())).serve(listener));

    let out = roundtrip(
        addr,
        "POST /login HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    )
    .await;
    let line = out
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap();
    let cookie = line["set-cookie:".len()..]
        .trim()
        .split(';')
        .next()
        .unwrap();
    let out = roundtrip(
        addr,
        format!("GET /me HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.ends_with("ada"), "{out}");
}
