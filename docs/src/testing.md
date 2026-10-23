# Testing

`App::test()` turns an app into an in-process client. It runs the full pipeline (global middleware, routing, route middleware, extractors, handlers, error rendering) without opening a socket, so tests are fast and deterministic.

```rust
use zapline::prelude::*;

fn app() -> App {
    App::new().mount((create_user, get_user))
}

#[tokio::test]
async fn creates_and_fetches_a_user() {
    let client = app().test();

    let res = client
        .post("/users")
        .json(&serde_json::json!({ "name": "ada", "email": "ada@example.com" }))
        .send()
        .await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let created: User = res.json();

    let res = client.get(&format!("/users/{}", created.id)).send().await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.json::<User>().name, "ada");
}
```

## Request builder

| Method | Effect |
|---|---|
| `.header(name, value)` | Add a header |
| `.bearer(token)` | `Authorization: Bearer <token>` |
| `.query(&value)` | Append a URL-encoded query string (after any query already in the URI) |
| `.json(&value)` / `.form(&value)` / `.body(bytes)` | Request body, with the matching content type |
| `.multipart(form)` | A `multipart/form-data` body built with `zapline::test::MultipartForm` |
| `.remote_addr(addr)` | Pretend the request came from this peer. The default is `127.0.0.1`. |

```rust
use zapline::test::MultipartForm;

let form = MultipartForm::new()
    .text("title", "Holiday")
    .file("photo", "beach.png", "image/png", png_bytes);
let res = client.post("/upload").multipart(form).send().await;
```

## Response helpers

`status()`, `headers()`, `header(name) -> Option<&str>`, `header_all(name) -> Vec<&str>`, `text()`, `bytes()`, `json::<T>()` (panics with the body text if parsing fails) and `cookie(name)`, which returns the response's `Set-Cookie` for that cookie with its attributes (`value()`, `path()`, `max_age()`, `http_only()`, `is_removal()`...).

`assert_status(..)` and `assert_header(name, value)` return the response, so they chain, and their panic messages include the response body:

```rust
client.post("/users").json(&new_user).send().await
    .assert_status(StatusCode::CREATED)
    .assert_header("content-type", "application/json");
// panics with: expected status 201, got 422 Unprocessable Entity
//              response body: {"error":"invalid request body: missing field `email`",...}
```

## Cookies and sessions

`app.test()` is stateless: each request carries only the headers you set. `.with_cookies()` gives the client a cookie store, shared by its clones, that works like a browser's: it keeps every `Set-Cookie` it is allowed to and sends the matching cookies back in one `Cookie` header. That makes login flows one line per step:

```rust
let browser = app().test().with_cookies();
browser.post("/login").json(&credentials).send().await.assert_status(200);
browser.get("/me").send().await.assert_status(200);
browser.post("/logout").send().await;
assert_eq!(browser.cookie("zapline.sid"), None);
```

Calling `.with_cookies()` again starts a separate store, e.g. for a second user. The store applies the rules browsers do (RFC 6265 and 6265bis):

| Attribute | In the test store |
|---|---|
| `Domain` | Without it, a cookie is host-only. With it, the cookie also goes to subdomains. A `Domain` the host doesn't belong to, or a public suffix (`com`, `co.uk`), is rejected. |
| `Path` | Prefix match, defaulting to the request's directory. Longer paths are sent first. |
| `Secure` | Stored and sent only on secure requests: `https`, or a loopback host such as the default `http://localhost`. A plain-`http` response can't overwrite a `Secure` cookie. |
| `SameSite` | Applied to requests another page starts (below). `Strict` stays home on cross-site requests; `Lax` (also the default) goes only with safe top-level navigations; `None` always, and needs `Secure`. |
| `Max-Age`, `Expires` | `Max-Age` wins. Expired cookies are deleted and never sent. |
| `HttpOnly` | Kept, and hidden from `document_cookie`. |
| `__Secure-`, `__Host-` | `__Secure-` needs `Secure`; `__Host-` also needs `Path=/` and no `Domain`. |

Cookies you set with `.header("cookie", ..)` are sent first and win over stored ones with the same name.

### Origins and cross-site requests

Relative URIs go to `http://localhost` unless you pick the site with `.base_url("https://app.example.com")`: requests then get a `Host` header, and `https` makes them secure (`ConnInfo::secure`). Absolute URIs (`client.get("https://api.example.com/x")`) use their own origin.

`.from_origin(url)` sends a request as if a page on `url` started it with a link or a form, and `.fetch_from(url)` as a `fetch` with credentials. They add the `Origin` and `Sec-Fetch-Site`/`Sec-Fetch-Mode` headers a browser would, and the store applies `SameSite`. Sites are compared like browsers do, by scheme and registrable domain, using a short built-in list of multi-label public suffixes (`co.uk`, `github.io`, ...) instead of the full Public Suffix List. So you can test CSRF defenses against what a browser would really send:

```rust
let browser = app().test().with_cookies().base_url("https://app.example.com");
browser.post("/login").json(&credentials).send().await.assert_status(200);

// A form on another site: the Lax session cookie stays home, and
// `SameOrigin` refuses the request.
browser.post("/transfer").from_origin("https://evil.example").send().await.assert_status(403);
// A sibling subdomain is the same site, and the cookie goes along.
browser.post("/transfer").from_origin("https://blog.example.com").send().await;
```

To look inside the store:

| Method | Returns |
|---|---|
| `cookie(name)` | The value of a stored, unexpired cookie |
| `cookies()` | Every stored cookie with its attributes (`domain`, `host_only`, `path`, `secure`, `http_only`, `same_site`, `expires`) |
| `document_cookie(url)` | What `document.cookie` would return on that page: matching cookies that aren't `HttpOnly` |
| `set_cookie(url, set_cookie)` | Stores a `Set-Cookie` as if that page sent it, with every rule applied, and returns whether it was accepted. Use it to simulate another site planting a cookie. |
| `clear_cookies()` | Empties the store |

```rust
// JavaScript can read the CSRF token but never the session id.
let js = browser.document_cookie("https://app.example.com/");
assert!(js.contains("__Host-zapline.csrf=") && !js.contains("zapline.sid"));

// evil.example.com can plant a cookie for all of example.com, but not a __Host- one.
assert!(browser.set_cookie("https://evil.example.com/", "csrf=x; Domain=example.com"));
assert!(!browser.set_cookie("https://evil.example.com/", "__Host-csrf=x; Domain=example.com; Path=/; Secure"));
```

Not modelled: redirects are not followed, there is no per-domain cookie limit or eviction, and the Chrome "Lax+POST" two-minute exception for cookies without a `SameSite` attribute is not applied.

## Lifecycle in tests

Startup hooks run on the first request. Use `client.start().await` to run them up front, `client.resources().await` to inspect state, and `client.shutdown().await` to run shutdown hooks. To swap real dependencies for fakes, register replacements before calling `.test()`. Registering the same type again replaces the earlier value:

```rust
let client = app().provide_dyn::<dyn UserRepo>(Arc::new(FakeRepo::default())).test();
```

## Real sockets

When a feature needs a real connection (WebSockets, TLS, HTTP/2), serve on an ephemeral port:

```rust
let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
let addr = listener.local_addr()?;
tokio::spawn(app().serve(listener));
// connect to `addr` with any client
```
