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

## Snapshot tests from real traffic

Click through your app once and keep what happened as a regression test. In a debug build the [dev console](dev-console.md) can record every request and the response it got; saved, they replay through `app.test()` on every `cargo test`.

1. **Record.** Start the app with `zapline dev --record` (it sets `ZAPLINE_RECORD=1`), or press **Start recording** at `/_dev`. Then use the app: a browser, curl, your frontend.
2. **Save.** Run `zapline snap signup`, or name the snapshot in the console and press **Save as snapshot test**. You get `tests/snapshots/signup.json` and, the first time, `tests/snapshots.rs`:

   ```rust
   #[tokio::test]
   async fn recorded_traffic() {
       my_app::app()
           .test()
           .replay_snapshots("tests/snapshots")
           .await;
   }
   ```

3. **Run.** `cargo test` replays every file in `tests/snapshots/` in name order, each request in recorded order. A response must have the recorded status and body, or the test fails with a diff:

   ```text
   1 of 5 recorded response(s) changed:

   tests/snapshots/signup.json #4 POST /users
     body (- recorded, + actual):
         {
           "code": "email_taken",
       -   "error": "email already registered"
       +   "error": "that email is taken"
         }

   If the new responses are right, accept them with ZAPLINE_UPDATE_SNAPSHOTS=1 cargo test
   ```

4. **Accept changes.** When the new responses are right, `ZAPLINE_UPDATE_SNAPSHOTS=1 cargo test --test snapshots` rewrites the snapshots that changed (and only those). Review the change in your diff like any other code.

### What may differ between runs

Values that legitimately change are compared by kind, not by value, in JSON and in HTML or text bodies alike:

| Value | Example | Compared as |
|---|---|---|
| UUIDs | `67e55044-10b1-426f-9247-bb680e5fe0c8` | `[uuid]` |
| Timestamps | `2024-05-01T12:30:00Z`, HTTP dates; any value under `created_at`, `updatedAt`, `timestamp`, `exp`... | `[timestamp]` |
| Tokens | session ids, API keys, JWTs, hex digests: long strings that mix letters and digits | `[token]` |
| Asset hashes | `/static/app.73cd192d.css` | `app.[hash].css` |
| Ids | any value under `id`, `user_id`, `userId`, `uuid` | `[id]` |

Ids also carry forward. When the recording's `POST /users` answered `{"id": 7}` and the replay's answers `{"id": 1}`, the requests after it are sent with `1` where the recording had `7`: in path segments (`/users/7` becomes `/users/1`), in id fields of the query, form or JSON body (`user_id=7`), and in the expected responses (an HTML page's `hx-delete="/users/7"`). Ids of three digits or more, UUIDs and tokens are replaced wherever they appear as a word; a bare `7` elsewhere is left alone, since it is more likely a count or a page number. So a session recorded against a database full of data replays against an empty test database.

### Credentials and cookies

`Authorization`, `Cookie`, `Proxy-Authorization`, `X-Api-Key`, `X-Auth-Token` and CSRF token headers are never recorded: the snapshot says `"[redacted]"` and the replay does not send them. Each snapshot file replays with a fresh cookie jar, like a new browser, so a recorded login sets the session cookie that the following requests use. For bearer tokens and API keys, give the replay a test credential:

```rust
app().test()
    .replay_snapshots("tests/snapshots")
    .header("authorization", "Bearer test-token")
    .await;
```

Bodies are recorded as sent, passwords included: record with throwaway dev accounts.

### What is recorded

The method, path, query, the headers that affect a response (`accept`, `accept-language`, `content-type`, `host`, `origin`, `referer`, `sec-fetch-site`, `if-none-match`, `x-requested-with` and htmx's `hx-*`), the body, the status, the content type and the response body. JSON bodies are stored as JSON, text as text and anything else as base64, so the files read well in a review.

Not recorded: the console's own requests, static files (CSS, scripts, images, fonts), WebSocket upgrades, and requests whose body is streamed or over 64 KiB. A response that is streamed (SSE), compressed or over 64 KiB is recorded without its body (`"body_not_recorded"`), and only its status is compared; an update run fills the body in.

### Replay API

`TestClient::replay_snapshots(path)` takes a directory or a single file and returns a `zapline::test::Replay`:

| Call | Effect |
|---|---|
| `.await` | Replay, and panic with every diff if anything changed |
| `.header(name, value)` | Send this header with every request, replacing the recorded one |
| `.update(true)` | Rewrite changed snapshots instead of failing (the default is `ZAPLINE_UPDATE_SNAPSHOTS=1`) |
| `.report().await` | Return a `ReplayReport` (`files`, `exchanges`, `updated`, `failures`, `passed()`) instead of panicking |

Snapshots share the app instance (and its state) in a run. For a fresh app per snapshot, replay each file with its own client: `app().test().replay_snapshots("tests/snapshots/signup.json").await`.
