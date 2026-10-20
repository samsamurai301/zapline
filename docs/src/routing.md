# Routing

## Registering routes

There are three equivalent styles. Use whichever reads best.

```rust
use zapline::prelude::*;

// 1. Attribute macros (also generate OpenAPI docs from the signature)
#[get("/users/{id}")]
async fn get_user(Path(id): Path<u64>) -> String { format!("user {id}") }

// 2. Builder methods with named functions
async fn list_users() -> &'static str { "[]" }

fn app() -> App {
    App::new()
        .mount(get_user)
        .get("/users", list_users)
        // 3. Closures
        .get("/ping", || async { "pong" })
}
```

`#[route("/items", method = "GET", method = "POST")]` registers one handler for several methods. Mount several macro routes at once with a tuple: `.mount((get_user, create_user, delete_user))`.

## Path patterns

| Pattern | Matches | Extract with |
|---|---|---|
| `/users` | exactly `/users` | — |
| `/users/{id}` | `/users/42` | `Path<u64>` |
| `/orgs/{org}/repos/{repo}` | `/orgs/acme/repos/web` | `Path<(String, String)>` or a struct |
| `/files/{*path}` | `/files/a/b/c.txt` | `Path<String>` → `a/b/c.txt` |

Static segments take priority over parameters, so `/users/me` and `/users/{id}` can coexist. Parameters are percent-decoded. Trailing slashes are significant. To make `/users/` match `/users`, use `NormalizePath`.

## Groups and nesting

`Routes` is a group of routes. It can carry its own middleware and metadata, and you can nest it under a prefix:

```rust
let admin = Routes::new()
    .get("/stats", stats)
    .delete("/users/{id}", ban_user)
    .wrap(require_admin);          // only applies to this group

let app = App::new()
    .nest("/admin", admin)         // -> /admin/stats, /admin/users/{id}
    .nest("/api/v1", api_v1());    // versioning is just nesting
```

Groups can be built in other modules or crates and merged with `.merge(routes)` or `.mount(routes)`.

## Methods, 404 and 405

- A path registered for some methods returns **405 Method Not Allowed** with an `Allow` header for any other method.
- `HEAD` is served automatically by the `GET` handler (with no body).
- Unknown paths return a JSON **404**. Use `.fallback(handler)` to customize it, for example to serve a SPA.
- `.any(path, handler)` matches every method that isn't otherwise registered on that path.

## Route metadata

Every route carries a typed metadata map. Plugins use it: OpenAPI stores summaries and tags there, auth stores required roles, and rate limiting stores per-route policies. You can attach your own:

```rust
#[derive(Clone)]
struct Audit(&'static str);

let app = App::new()
    .post("/payments", pay).meta(Audit("payments"))
    .wrap_route(|req: Request, next: Next| async move {
        if let Some(Audit(kind)) = req.route().and_then(|r| r.meta::<Audit>()) {
            println!("audit: {kind}");
        }
        next.run(req).await
    });
```

`Routes::meta_all(value)` applies metadata to every route in a group.

## Performance notes

Routing is a radix-tree lookup (`matchit`) followed by a fixed-slot method match. Route parameters are stored as byte ranges into the request path, inline for up to 4 parameters, so routing never allocates.
