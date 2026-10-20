# Cookies, sessions & CSRF

Cookies, server-side sessions and CSRF protection for browser-facing apps. All of it needs the Cargo feature `cookies` (CSRF included; it adds the `cookie` and `getrandom` crates). The common case is a session behind a signed cookie:

```rust
use zapline::cookies::Key;
use zapline::prelude::*;
use zapline::session::{MemoryStore, Session, SessionLayer};

async fn login(session: Session) -> Result<&'static str> {
    session.regenerate();                 // new session id on login: prevents fixation
    session.insert("user_id", 42)?;
    Ok("logged in")
}

async fn me(session: Session) -> Result<String> {
    let id: u64 = session.get("user_id").ok_or_else(Error::unauthorized)?;
    Ok(format!("user {id}"))
}

async fn logout(session: Session) -> &'static str {
    session.destroy();
    "bye"
}

fn app() -> App {
    App::new()
        .cookie_key(Key::generate())      // use a stable key in production
        .wrap(SessionLayer::new(MemoryStore::new()))
        .post("/login", login)
        .get("/me", me)
        .post("/logout", logout)
}
```

## Cookies

`CookieJar` is an extractor holding the request's cookies. Changes made with `add` and `remove` are sent back as `Set-Cookie` headers when you **return the jar** from the handler: `jar`, `(jar, body)` or `(StatusCode, jar, body)`. Only changes are sent. Forgetting to return the jar loses the change (the methods are `#[must_use]`).

```rust
use zapline::cookies::{secure_cookie, CookieJar};
use zapline::prelude::*;

async fn visit(jar: CookieJar) -> (CookieJar, String) {
    let n: u32 = jar.get("visits").and_then(|c| c.value().parse().ok()).unwrap_or(0) + 1;
    (jar.add(secure_cookie("visits", n.to_string())), format!("visit #{n}"))
}

async fn forget(jar: CookieJar) -> (CookieJar, StatusCode) {
    (jar.remove("visits"), StatusCode::NO_CONTENT)
}
```

`secure_cookie(name, value)` returns a `CookieBuilder` with safe defaults: `HttpOnly`, `Secure`, `SameSite=Lax` and `Path=/`. Adjust it with the builder methods, for example `secure_cookie("theme", "dark").max_age(zapline::cookies::time::Duration::days(30))`. The `cookie` crate's types are re-exported from `zapline::cookies`: `Cookie`, `CookieBuilder`, `SameSite`, `Key`, `Expiration` and `time`.

Cookie values are percent-encoded. At most 256 cookies are read per request, and if a name appears twice the first occurrence wins. `remove(..)` defaults to `Path=/`; pass a full cookie if you set another path or domain.

### Signed and private jars

| Jar | Protection | The client can... |
|---|---|---|
| `CookieJar` | None | read and change it |
| `SignedCookieJar` | HMAC-SHA256 signature | read it, but not change it |
| `PrivateCookieJar` | AES-256-GCM encryption | neither read nor change it |

Both protected jars need a `Key` registered with `App::cookie_key`; without one they fail with `500 cookie_key_missing`. `get` returns `None` for cookies that are missing or have been tampered with. They use the same API as `CookieJar`:

```rust
use zapline::cookies::{Key, PrivateCookieJar};
use zapline::prelude::*;

async fn remember(jar: PrivateCookieJar) -> (PrivateCookieJar, &'static str) {
    (jar.add(("prefs", "dark-mode")), "saved")
}

async fn prefs(jar: PrivateCookieJar) -> String {
    jar.get("prefs").map(|c| c.value().to_owned()).unwrap_or_else(|| "none".into())
}

fn app() -> App {
    // 64 random bytes from your secret store, identical on every instance.
    let secret = std::env::var("COOKIE_KEY").expect("COOKIE_KEY");
    let key = Key::try_from(secret.as_bytes()).expect("COOKIE_KEY must be at least 64 bytes");

    App::new().cookie_key(key).post("/prefs", remember).get("/prefs", prefs)
}
```

`Key::generate()` creates a random key. That's fine for development, but every restart invalidates all signed and private cookies and all sessions, and each instance would use a different key. In production, load a stable key of at least 64 bytes. The same key signs sessions and protects the jars.

## Sessions

A session is a map of JSON values stored on the server, found through a signed cookie that contains only a random 256-bit session id. Install `SessionLayer` (globally with `App::wrap`, or on a group with `Routes::wrap`), then extract `Session` in handlers:

| `Session` method | Effect |
|---|---|
| `get::<T>(key)` | The value deserialized, or `None` if missing or of another shape |
| `get_value(key)` | The raw `serde_json::Value` |
| `insert(key, value)` | Stores any `Serialize` value. Returns an error if serialization fails. |
| `remove(key)` | Removes and returns a value |
| `contains_key(key)`, `keys()`, `is_empty()` | Inspection |
| `clear()` | Removes all values, keeping the session |
| `regenerate()` | New session id, same data. **Call it on login** and on other privilege changes. |
| `destroy()` | Deletes the data from the store and tells the client to drop the cookie. Values inserted afterwards start a new session. |
| `id()`, `expires_at()` | The id and expiry of the stored session |
| `Session::current(&req)` | The session from inside middleware or an authenticator, where extractors can't be used |

`Session` is cheap to clone and all clones share the same data. Changes are saved after the handler returns.

How it behaves:

- **Lazy.** The store is only touched when a handler extracts `Session`. Routes that never use it cost one clone of the `Cookie` header.
- **Saved only when needed.** Unchanged sessions aren't written, except to extend the expiry.
- **Sliding expiry.** A session expires after the idle timeout (`ttl`, default 24 hours). It is saved again, with a fresh cookie, when less than half of that remains.
- **Fixation-proof.** An unknown or expired id sent by the client is never adopted: you get a new session, and a new id is issued when data is stored. An empty new session is never stored.
- Without the layer, extracting `Session` fails with `500 session_layer_missing`. Without a cookie key, the layer fails with `500 cookie_key_missing`.

### `SessionLayer` options

| Method | Default | Effect |
|---|---|---|
| `SessionLayer::new(store)` | | Sessions kept in `store` |
| `.cookie_name(name)` | `zapline.sid` | Cookie name (panics on an invalid token) |
| `.ttl(duration)` | 24 hours | Idle timeout |
| `.secure(bool)` | `true` | `Secure` attribute. Browsers drop `Secure` cookies on plain `http://` (most make an exception for `localhost`). Disable it only for local development. |
| `.http_only(bool)` | `true` | `HttpOnly` attribute |
| `.same_site(SameSite)` | `Lax` | `SameSite` attribute |
| `.path(path)` / `.domain(domain)` | `/` / host-only | Cookie scope |
| `.key(key)` | the app's key | Sign with this key instead of `App::cookie_key` |

### Stores

`MemoryStore` keeps sessions in the process. It is meant for development, tests and single-instance apps: sessions vanish on restart and aren't shared between replicas. It holds at most 100,000 sessions by default (`MemoryStore::with_capacity(n)` changes that), purging expired ones as new ones arrive and evicting the sessions closest to expiry when full.

For several instances, implement `SessionStore` over Redis, a database or whatever you run. It has three async methods. Ids are 256-bit random URL-safe strings, so they are safe to use as keys directly:

```rust
use std::collections::HashMap;
use std::sync::Mutex;
use zapline::prelude::*;
use zapline::session::{SessionLayer, SessionRecord, SessionStore};

#[derive(Default)]
struct MapStore {
    rows: Mutex<HashMap<String, SessionRecord>>,
}

impl SessionStore for MapStore {
    async fn load(&self, id: &str) -> Result<Option<SessionRecord>> {
        // Return None for unknown ids. Expired rows may be returned too:
        // the layer checks `expires_at`.
        Ok(self.rows.lock().unwrap().get(id).cloned())
    }

    async fn save(&self, id: &str, record: &SessionRecord) -> Result<()> {
        // Keep the row until `record.expires_at` (for example, `SET .. EXAT` in Redis).
        self.rows.lock().unwrap().insert(id.to_owned(), record.clone());
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.rows.lock().unwrap().remove(id);
        Ok(())
    }
}

fn layer() -> SessionLayer {
    SessionLayer::new(MapStore::default())
}
```

A `SessionRecord` has two fields: `data` (a JSON map) and `expires_at` (a `SystemTime`). Errors from your store propagate to the client as the `Error` you return (usually map them to a `503` or `500`).

### Sessions as authentication

`Authenticate` (see [Authentication & authorization](auth.md)) can turn the session into a `CurrentUser`. The session layer must run first, so install it globally and the authenticator on the group:

```rust
use zapline::auth::{Authenticate, Authenticator, CurrentUser};
use zapline::cookies::Key;
use zapline::prelude::*;
use zapline::session::{MemoryStore, Session, SessionLayer};

struct SessionAuth;

impl Authenticator for SessionAuth {
    type User = String;

    async fn authenticate(&self, req: &Request) -> Result<Option<String>> {
        let session = Session::current(req).await?;
        Ok(session.get::<String>("user"))
    }
}

async fn dashboard(CurrentUser(user): CurrentUser<String>) -> String {
    format!("welcome back, {user}")
}

fn app() -> App {
    let private = Routes::new()
        .get("/dashboard", dashboard)
        .wrap(Authenticate::new(SessionAuth).required());

    App::new()
        .cookie_key(Key::generate())
        .wrap(SessionLayer::new(MemoryStore::new()))
        .nest("/app", private)
}
```

### Testing sessions

The test client does not keep cookies. Pass the `Set-Cookie` value back yourself:

```rust
use zapline::cookies::Key;
use zapline::prelude::*;
use zapline::session::{MemoryStore, Session, SessionLayer};

fn app() -> App {
    App::new()
        .cookie_key(Key::generate())
        .wrap(SessionLayer::new(MemoryStore::new()))
        .post("/login", |session: Session| async move {
            session.insert("user_id", 42)?;
            Ok::<_, Error>("ok")
        })
        .get("/me", |session: Session| async move {
            session.get::<u64>("user_id").map(|id| format!("user {id}")).unwrap_or_default()
        })
}

#[tokio::test]
async fn session_survives_a_round_trip() {
    let client = app().test();

    let res = client.post("/login").send().await;
    let cookie = res.header("set-cookie").unwrap().split(';').next().unwrap().to_owned();

    let res = client.get("/me").header("cookie", cookie).send().await;
    assert_eq!(res.text(), "user 42");
}
```

## CSRF

`Csrf` is double-submit-cookie protection for browser apps that authenticate with cookies. On the first response to a client it sets a random token cookie (`zapline.csrf`, deliberately readable by JavaScript). Requests with an unsafe method (anything but `GET`, `HEAD`, `OPTIONS` and `TRACE`) must send the same token back in the `x-csrf-token` header or, for `application/x-www-form-urlencoded` bodies, in the `csrf_token` form field. Otherwise they get `403` with the code `csrf_failed`. The comparison is constant-time.

Install it as global middleware and render the token with the `CsrfToken` extractor:

```rust
use zapline::auth::{Csrf, CsrfToken};
use zapline::prelude::*;

async fn form(token: CsrfToken) -> Html<String> {
    Html(format!(
        r#"<form method="post" action="/transfer">{}<button>Send</button></form>"#,
        token.hidden_input()
    ))
}

async fn transfer(Form(fields): Form<std::collections::HashMap<String, String>>) -> String {
    format!("{} fields received", fields.len())
}

fn app() -> App {
    App::new()
        .wrap(Csrf::new().exempt("/webhooks/*"))
        .get("/form", form)
        .post("/transfer", transfer)
}
```

JavaScript clients read the cookie (or get the token from the page) and send it as a header:

```js
fetch("/transfer", {
  method: "POST",
  headers: { "x-csrf-token": token, "content-type": "application/json" },
  body: JSON.stringify({ amount: 5 }),
});
```

| `Csrf` method | Default | Effect |
|---|---|---|
| `.cookie_name(name)` | `zapline.csrf` | Token cookie name. In production consider `__Host-csrf`: the cookie already has `Path=/`, `Secure` and no `Domain`. |
| `.header_name(name)` | `x-csrf-token` | Request header carrying the token |
| `.form_field(name)` | `csrf_token` | URL-encoded form field carrying the token |
| `.exempt(path)` | none | Skips the check for an exact path, or for a prefix when it ends in `*` (`"/webhooks/*"`) |
| `.secure(bool)` | `true` | `Secure` attribute of the token cookie |
| `.same_site(SameSite)` | `Lax` | `SameSite` attribute of the token cookie |

`CsrfToken` offers `as_str()`, `field_name()`, `hidden_input()` (an HTML-escaped `<input type="hidden">`) and `Display`. Without the middleware, extracting it fails with `500 csrf_layer_missing`.

Things to know:

- **Forms must be URL-encoded to use the field.** `multipart/form-data` bodies are never inspected; send the token in the header instead (for example, in a `fetch` call).
- **APIs that authenticate with an `Authorization` header don't need CSRF.** Browsers don't attach that header on their own. Apply `Csrf` only to the cookie-authenticated part of the app, for instance with `.exempt("/api/*")`, or use `Routes::wrap` on the group.
- Exempt paths still receive a token cookie, and they are matched against the request path (before routing).
- Defense in depth: keep the session cookie `SameSite=Lax` (the default) or `Strict`, in addition to the token.
