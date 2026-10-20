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
| `.json(&value)` / `.form(&value)` / `.body(bytes)` | Request body, with the matching content type |
| `.remote_addr(addr)` | Pretend the request came from this peer. The default is `127.0.0.1`. |

## Response helpers

`status()`, `headers()`, `header(name) -> Option<&str>`, `text()`, `bytes()`, and `json::<T>()`, which panics with the body text if parsing fails.

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
