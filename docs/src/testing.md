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

`app.test()` is stateless: each request carries only the headers you set. `.with_cookies()` gives the client a cookie store, shared by its clones, that works like a browser's: it keeps every `Set-Cookie` (honoring `Path`, `Max-Age` and `Expires`, and deleting cookies set to expire) and sends matching cookies back in one `Cookie` header. That makes login flows one line per step:

```rust
let browser = app().test().with_cookies();
browser.post("/login").json(&credentials).send().await.assert_status(200);
browser.get("/me").send().await.assert_status(200);
browser.post("/logout").send().await;
assert_eq!(browser.cookie("zapline.sid"), None);
```

Every request counts as same-site and secure, so `Secure` session cookies are sent too. Calling `.with_cookies()` again starts a separate store, e.g. for a second user.

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
