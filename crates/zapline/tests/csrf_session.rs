//! CSRF tokens bound to the session: a token minted for one session fails
//! for another, and tokens rotate when the session id changes. Exercised
//! through the browser-like test cookie jar, in both middleware orders.
#![cfg(feature = "cookies")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;
use zapline::auth::{Csrf, CsrfToken};
use zapline::cookies::{Key, SameSite};
use zapline::prelude::*;
use zapline::session::{MemoryStore, Session, SessionLayer, SessionRecord, SessionStore};
use zapline::test::{TestClient, TestResponse};

const SITE: &str = "https://app.example.com";
const CSRF: &str = "__Host-zapline.csrf";
const SID: &str = "zapline.sid";

fn key() -> Key {
    Key::from(&[7u8; 64])
}

#[derive(Clone, Copy, Debug)]
enum Order {
    /// `.wrap(SessionLayer).wrap(Csrf)`: the session layer runs first.
    SessionFirst,
    /// `.wrap(Csrf).wrap(SessionLayer)`: `Csrf` runs first.
    CsrfFirst,
}

const ORDERS: [Order; 2] = [Order::SessionFirst, Order::CsrfFirst];

/// A `MemoryStore` that counts loads.
#[derive(Clone, Default)]
struct CountingStore {
    inner: MemoryStore,
    loads: Arc<AtomicUsize>,
}

impl SessionStore for CountingStore {
    async fn load(&self, id: &str) -> Result<Option<SessionRecord>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load(id).await
    }
    async fn save(&self, id: &str, record: &SessionRecord) -> Result<()> {
        self.inner.save(id, record).await
    }
    async fn delete(&self, id: &str) -> Result<()> {
        self.inner.delete(id).await
    }
}

fn layered(app: App, order: Order, csrf: Csrf, sessions: SessionLayer) -> App {
    match order {
        Order::SessionFirst => app.wrap(sessions).wrap(csrf),
        Order::CsrfFirst => app.wrap(csrf).wrap(sessions),
    }
}

fn routes(app: App) -> App {
    app.get("/form", |t: CsrfToken| async move { t.as_str().to_owned() })
        .post("/login", |s: Session, h: HeaderMap| async move {
            let user = h
                .get("x-user")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("ada")
                .to_owned();
            s.regenerate();
            s.insert("user", user)?;
            Ok::<_, Error>("in")
        })
        .post("/logout", |s: Session| async move {
            s.destroy();
            "out"
        })
        .post("/submit", |s: Session| async move {
            s.get::<String>("user")
                .unwrap_or_else(|| "anonymous".into())
        })
        .post("/touch", |s: Session| async move {
            // Writes without changing the id.
            s.insert("n", 1)?;
            Ok::<_, Error>("ok")
        })
}

fn app(order: Order) -> App {
    let sessions = SessionLayer::new(MemoryStore::new());
    routes(layered(
        App::new().cookie_key(key()),
        order,
        Csrf::new(),
        sessions,
    ))
}

fn browser(app: App) -> TestClient {
    app.test().with_cookies().base_url(SITE)
}

/// The CSRF token as page JavaScript reads it from the cookie.
fn token(b: &TestClient) -> Option<String> {
    let cookies = b.document_cookie(&format!("{SITE}/"));
    cookies
        .split("; ")
        .find_map(|pair| pair.strip_prefix(&format!("{CSRF}=")))
        .map(str::to_owned)
}

async fn post(b: &TestClient, path: &str, token: &str) -> TestResponse {
    b.post(path).header("x-csrf-token", token).send().await
}

fn code(r: &TestResponse) -> String {
    r.json::<Value>()["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn tokens_rotate_with_the_session_id() {
    for order in ORDERS {
        let b = browser(app(order));
        // An anonymous visitor's token.
        let r = b.get("/form").send().await;
        let anon = r.text();
        assert_eq!(token(&b).as_deref(), Some(anon.as_str()), "{order:?}");
        assert_eq!(anon.len(), 107);

        // Logging in (new session id) rotates it.
        let r = post(&b, "/login", &anon).await;
        assert_eq!(r.status(), 200, "{order:?}: {}", r.text());
        assert!(r.cookie(SID).is_some());
        let rotated = r.cookie(CSRF).expect("rotated token cookie");
        assert_ne!(rotated.value(), anon, "{order:?}");
        let user = token(&b).unwrap();
        assert_eq!(user, rotated.value());
        // The session id is never readable by scripts.
        assert!(!b.document_cookie(&format!("{SITE}/")).contains(SID));

        assert_eq!(post(&b, "/submit", &user).await.text(), "ada", "{order:?}");
        // Writes that keep the id keep the token too.
        let r = post(&b, "/touch", &user).await;
        assert_eq!(r.status(), 200);
        assert!(r.cookie(CSRF).is_none(), "{order:?}");
        // Pages render the bound token.
        assert_eq!(b.get("/form").send().await.text(), user, "{order:?}");

        // The pre-login token no longer works, even sent as its own cookie.
        let r = b
            .post("/submit")
            .header("cookie", format!("{CSRF}={anon}"))
            .header("x-csrf-token", anon.clone())
            .send()
            .await;
        assert_eq!(r.status(), 403, "{order:?}");
        assert_eq!(code(&r), "csrf_failed");

        // A second login (regenerate) rotates again.
        let r = post(&b, "/login", &user).await;
        assert_eq!(r.status(), 200);
        let second = token(&b).unwrap();
        assert_ne!(second, user, "{order:?}");

        // Logout rotates back to an anonymous token, which then works.
        let r = post(&b, "/logout", &second).await;
        assert_eq!(r.status(), 200);
        assert!(r.cookie(SID).unwrap().is_removal());
        let after = token(&b).unwrap();
        assert_ne!(after, second, "{order:?}");
        assert_eq!(post(&b, "/submit", &after).await.text(), "anonymous");
        assert_eq!(post(&b, "/submit", &second).await.status(), 403);
    }
}

#[tokio::test]
async fn a_token_from_another_session_fails() {
    for order in ORDERS {
        let client = app(order).test();
        let alice = client.clone().with_cookies().base_url(SITE);
        let mallory = client.clone().with_cookies().base_url(SITE);
        for (b, name) in [(&alice, "alice"), (&mallory, "mallory")] {
            b.get("/form").send().await;
            let t = token(b).unwrap();
            let r = b
                .post("/login")
                .header("x-user", name)
                .header("x-csrf-token", t)
                .send()
                .await;
            assert_eq!(r.status(), 200);
        }
        let alice_token = token(&alice).unwrap();
        let mallory_token = token(&mallory).unwrap();

        // Mallory's server-minted token, planted in Alice's browser in place
        // of hers and submitted with Alice's session: refused.
        let r = alice
            .post("/submit")
            .header("cookie", format!("{CSRF}={mallory_token}"))
            .header("x-csrf-token", mallory_token.clone())
            .send()
            .await;
        assert_eq!(r.status(), 403, "{order:?}");
        assert_eq!(code(&r), "csrf_failed");
        // ...also through the form field.
        let r = alice
            .post("/submit")
            .header("cookie", format!("{CSRF}={mallory_token}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("csrf_token={mallory_token}"))
            .send()
            .await;
        assert_eq!(r.status(), 403, "{order:?}");

        // A token signed with the same key but bound to no session (from an
        // app without `SessionLayer`) is refused too.
        let unbound = {
            let c = App::new()
                .cookie_key(key())
                .wrap(Csrf::new())
                .get("/form", |t: CsrfToken| async move { t.as_str().to_owned() })
                .test();
            c.get("/form").send().await.text()
        };
        let r = alice
            .post("/submit")
            .header("cookie", format!("{CSRF}={unbound}"))
            .header("x-csrf-token", unbound.clone())
            .send()
            .await;
        assert_eq!(r.status(), 403, "{order:?}");

        // Each browser's own token works.
        assert_eq!(post(&alice, "/submit", &alice_token).await.text(), "alice");
        assert_eq!(
            post(&mallory, "/submit", &mallory_token).await.text(),
            "mallory"
        );

        // A safe request with a foreign token gets a fresh, bound one.
        let r = alice
            .get("/form")
            .header("cookie", format!("{CSRF}={mallory_token}"))
            .send()
            .await;
        let fresh = r.text();
        assert_ne!(fresh, mallory_token);
        assert_eq!(r.cookie(CSRF).unwrap().value(), fresh);
    }
}

/// A sibling subdomain plants its own valid token for the parent domain in
/// a browser whose token cookie is gone (evicted), then posts a form
/// cross-site. Binding is what stops it: the session cookie is
/// `SameSite=None` here and the token cookie has no `__Host-` prefix.
#[tokio::test]
async fn binding_stops_a_planted_token_from_a_sibling_subdomain() {
    let csrf = || Csrf::new().cookie_name("csrf");
    let sessions = SessionLayer::new(MemoryStore::new()).same_site(SameSite::None);
    let victim = browser(routes(layered(
        App::new().cookie_key(key()),
        Order::SessionFirst,
        csrf(),
        sessions,
    )));
    victim.get("/form").send().await;
    let t = victim.cookie("csrf").unwrap();
    post(&victim, "/login", &t).await.assert_status(200);

    // The attacker's token: minted by the same app for the attacker.
    let attacker = victim.clone().with_cookies().base_url(SITE);
    let attacker_token = attacker.get("/form").send().await.text();

    // The victim's token cookie is evicted, and evil.example.com plants
    // the attacker's for all of example.com.
    assert!(victim.set_cookie(&format!("{SITE}/"), "csrf=; Max-Age=0; Path=/; Secure"));
    assert!(victim.set_cookie(
        "https://evil.example.com/",
        &format!("csrf={attacker_token}; Domain=example.com; Path=/; Secure; SameSite=None")
    ));
    let r = victim
        .post("/submit")
        .from_origin("https://evil.example.com")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("csrf_token={attacker_token}"))
        .send()
        .await;
    assert_eq!(r.status(), 403);
    assert_eq!(code(&r), "csrf_failed");

    // The same attack on an app without sessions (the token alone guards
    // cookie-authenticated state) gets through: the token isn't bound.
    let unbound = App::new()
        .cookie_key(key())
        .wrap(csrf())
        .get("/form", |t: CsrfToken| async move { t.as_str().to_owned() })
        .post("/submit", || async { "accepted" });
    let b = browser(unbound);
    let attacker_token = b.get("/form").send().await.text();
    b.clear_cookies();
    assert!(b.set_cookie(
        "https://evil.example.com/",
        &format!("csrf={attacker_token}; Domain=example.com; Path=/; Secure; SameSite=None")
    ));
    let r = b
        .post("/submit")
        .from_origin("https://evil.example.com")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("csrf_token={attacker_token}"))
        .send()
        .await;
    assert_eq!(r.text(), "accepted");
}

#[tokio::test]
async fn binding_does_not_load_the_session() {
    for order in ORDERS {
        let store = CountingStore::default();
        let loads = store.loads.clone();
        let b = browser(routes(layered(
            App::new().cookie_key(key()),
            order,
            Csrf::new(),
            SessionLayer::new(store),
        )));
        b.get("/form").send().await;
        let t = token(&b).unwrap();
        post(&b, "/login", &t).await.assert_status(200);
        let before = loads.load(Ordering::SeqCst);
        // Routes that never extract `Session` don't touch the store, though
        // their token is checked against the session cookie.
        b.get("/form").send().await.assert_status(200);
        assert_eq!(loads.load(Ordering::SeqCst), before, "{order:?}");
    }
}

#[tokio::test]
async fn the_session_key_signs_tokens_without_an_app_key() {
    for order in ORDERS {
        let sessions = SessionLayer::new(MemoryStore::new()).key(key());
        let b = browser(routes(layered(App::new(), order, Csrf::new(), sessions)));
        let anon = b.get("/form").send().await.text();
        // Signed (and bound), not a plain random token.
        assert_eq!(anon.len(), 107, "{order:?}");
        post(&b, "/login", &anon).await.assert_status(200);
        let user = token(&b).unwrap();
        assert_ne!(user, anon);
        assert_eq!(post(&b, "/submit", &user).await.text(), "ada");
        // A well-formed but unsigned token is refused.
        let forged = "A".repeat(43);
        let r = b
            .post("/submit")
            .header("cookie", format!("{CSRF}={forged}"))
            .header("x-csrf-token", forged)
            .send()
            .await;
        assert_eq!(r.status(), 403, "{order:?}");
    }
}

/// Sessions only on a group, `Csrf` global: tokens outside the group are
/// unbound; inside it they are rebound before any form is rendered.
#[tokio::test]
async fn sessions_on_a_group() {
    let account = Routes::new()
        .get("/form", |t: CsrfToken| async move { t.as_str().to_owned() })
        .post("/login", |s: Session| async move {
            s.regenerate();
            s.insert("user", "ada")?;
            Ok::<_, Error>("in")
        })
        .post("/submit", |s: Session| async move {
            s.get::<String>("user").unwrap_or_default()
        })
        .wrap(SessionLayer::new(MemoryStore::new()));
    let app = App::new()
        .cookie_key(key())
        .wrap(Csrf::new())
        .get(
            "/public",
            |t: CsrfToken| async move { t.as_str().to_owned() },
        )
        .post("/public", || async { "public" })
        .nest("/account", account);
    let b = browser(app);

    let public = b.get("/public").send().await.text();
    assert_eq!(post(&b, "/public", &public).await.text(), "public");
    // The unbound token doesn't pass in the session group...
    assert_eq!(post(&b, "/account/submit", &public).await.status(), 403);
    // ...but the group's pages hand out a bound one.
    let r = b.get("/account/form").send().await;
    let bound = r.text();
    assert_ne!(bound, public);
    assert_eq!(token(&b).unwrap(), bound);
    post(&b, "/account/login", &bound).await.assert_status(200);
    let user = token(&b).unwrap();
    assert_eq!(post(&b, "/account/submit", &user).await.text(), "ada");
    // Bound tokens are still signed by this server, so they pass outside.
    assert_eq!(post(&b, "/public", &user).await.text(), "public");
}

#[tokio::test]
async fn exempt_paths_and_safe_methods_still_rotate() {
    let app = App::new()
        .cookie_key(key())
        .wrap(SessionLayer::new(MemoryStore::new()))
        .wrap(Csrf::new().exempt("/hooks/*"))
        .post("/hooks/login", |s: Session| async move {
            s.insert("user", "bot")?;
            Ok::<_, Error>("ok")
        })
        .get("/form", |t: CsrfToken| async move { t.as_str().to_owned() });
    let b = browser(app);
    let anon = b.get("/form").send().await.text();
    // An exempt route that starts a session still rotates the token.
    let r = b.post("/hooks/login").send().await;
    assert_eq!(r.status(), 200);
    let rotated = r.cookie(CSRF).expect("rotated");
    assert_ne!(rotated.value(), anon);
    assert_eq!(b.get("/form").send().await.text(), rotated.value());
}
