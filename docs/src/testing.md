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

## Fuzz every route

Your OpenAPI document already says what every route accepts and returns, so zapline can write the tedious tests for you. `client.fuzz()` walks the document and sends each operation a few hundred requests generated from its own JSON Schemas:

```rust
#[tokio::test]
async fn every_route_survives_fuzzing() {
    app().test().fuzz().await.assert_clean();
}
```

For every operation it sends, in this order:

- **a typical valid request**, using schema examples when there are some;
- **boundary values that are still valid**: empty and maximum-length strings, unicode (combining marks, emoji, right-to-left text, NUL), `0`, `-1` and the limits of each integer type (`u8::MAX`, `i32::MIN`...), tiny and huge floats, empty and long arrays, objects with only their required fields or with all of them;
- **invalid input**: each required parameter and field missing, wrong types, numbers out of range, strings shorter than `minLength` or longer than `maxLength`, unknown enum variants and properties, malformed and deeply nested JSON, the wrong content type, a body one byte over the size limit;
- **random requests** (a third of their values at the boundaries) until the budget of 100 requests per operation is spent.

A response is a **failure** when:

| Failure | Meaning |
|---|---|
| `panicked` | The handler (or a middleware) panicked. Panics caught by `CatchPanic` count too. |
| `server error` | Any 5xx. |
| `timed out` | No response within the timeout (5 s). |
| `undeclared status` | A status the operation does not declare: its code, its range (`4XX`) or, for errors, `default`. A `201` from a route documented as `200` is caught here; declare it with `#[post("/items", status = 201)]`. Invalid requests may also get the framework's rejections (`400`, `404`, `405`, `411`, `413`, `414`, `415`, `422`, `431`). |
| `response does not match its schema` | A 2xx JSON body that breaks the declared response schema, with every mismatch listed by JSON pointer. |

Each failure is reported once per operation and kind, after **shrinking**: optional parameters and fields are dropped and strings and arrays shortened for as long as the same failure reproduces. The report gives the minimal request and a line of test code that replays it:

```text
zapline fuzz: 6 operations, 642 requests, seed 24301: 2 failures

1) GET /ratio/{n} (ratio): panicked: attempt to divide by zero at src/routes/math.rs:12:5
   case: boundary integer for path parameter `n` (hit by 8 requests)
   request: GET /ratio/0
   response: none
   reproduce: client.request(Method::GET, "/ratio/0").send().await

2) GET /stats (stats): response does not match its schema (1 error)
   - /count: expected integer, got string "many"
   case: valid request (hit by 100 requests)
   request: GET /stats
   response: 200 OK {"count":"many"}
   reproduce: client.request(Method::GET, "/stats").send().await

rerun the same requests with ZAPLINE_FUZZ_SEED=24301
```

Paste the `reproduce:` line into a regular test to keep the bug fixed. The fuzzer checks the app against the document its `OpenApi` plugin serves (including `.modify(..)` hooks), or one generated with the default settings when there is no plugin, so a failure is either a bug in the handler or a gap in the docs: both are worth fixing.

### Configuration

```rust
use std::time::Duration;
use zapline::test::FuzzConfig;

let report = app()
    .test()
    .fuzz()
    .config(
        FuzzConfig::new()
            .seed(7)                        // same seed, same requests
            .cases(500)                     // requests per operation
            .bearer("test-token")           // sent with every request
            .header("x-tenant", "acme")
            .timeout(Duration::from_secs(1))
            .allow_status(409)              // accepted from every operation
            .only("/admin")                 // operation id, `METHOD /path` or path prefix
            .skip("POST /webhooks"),
    )
    .await;
for failure in report.failures() {
    println!("{} -> {}", failure.request(), failure.kind());
}
report.assert_clean();
```

Runs are deterministic: the requests depend only on the seed and the document, and each operation has its own stream of cases, so adding a route does not change what the others get. When the test does not set them, the `ZAPLINE_FUZZ_SEED` and `ZAPLINE_FUZZ_CASES` environment variables choose the seed and the budget, which is how `zapline fuzz --seed 7 --cases 1000` explores further without editing the test.

The app keeps its state across cases, as a real server would: requests that create things create them, so later `GET`s see real data. Each request comes from its own peer address, so per-IP rate limits don't turn the run into a wall of `429`s. Operations documented as WebSocket upgrades or server-sent event streams are skipped and listed in the report; an undocumented event stream is recognized by its `text/event-stream` content type and not read past its headers. After its first timeout an operation gets no further cases. Request schemas use the JSON Schema subset schemars emits; `pattern` and string `format`s are not checked by the validator (values for common formats such as `email`, `uuid` and `date-time` are still generated correctly).

Projects created with `zapline new` include this test as `tests/fuzz.rs`; `zapline fuzz` runs it and prints the report.

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
