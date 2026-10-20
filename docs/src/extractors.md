# Extractors

Handler arguments are *extractors*: types that know how to build themselves from the request. A handler can take up to 12 arguments. Every argument except the last must be a **parts** extractor, meaning it doesn't read the body. The last argument may consume the body.

```rust
#[post("/orgs/{org}/members")]
async fn add_member(
    State(db): State<Db>,                 // app state
    Path(org): Path<String>,              // route parameter
    Query(opts): Query<AddOptions>,       // query string
    user: CurrentUser<Claims>,            // auth (see the Security chapter)
    Json(body): Json<NewMember>,          // body: must be last
) -> Result<StatusCode> { /* ... */ }
```

If an extractor fails, its error response is returned and the handler isn't called. For example, a bad path parameter gives a 400 and a wrong content type gives a 415. Rejections carry a stable `code`, and `details` that locate the problem:

| Rejection | Status | `code` | `details` |
|---|---|---|---|
| Bad route parameter | 400 | `invalid_path` | `{"param": "id"}` |
| Bad query string | 400 | `invalid_query` | |
| Bad form body | 422 | `invalid_form` | |
| Malformed JSON | 422 | `invalid_json` | `{"line": 1, "column": 9}` |
| JSON of the wrong shape (missing field, wrong type...) | 422 | `invalid_body` | `{"line": 1, "column": 22, "path": "items[0].qty"}` |
| Wrong `Content-Type` for `Json` / `Form` | 415 | `unsupported_media_type` | |

```text
POST /orders  {"items": [{"qty": "x"}]}

HTTP/1.1 422 Unprocessable Entity
{"error":"invalid request body: invalid type: string \"x\", expected u32",
 "code":"invalid_body","details":{"column":22,"line":1,"path":"items[0].qty"}}
```

## Built-in extractors

| Extractor | Source | Failure |
|---|---|---|
| `Path<T>` | Route parameters: a scalar, a tuple, or a struct with field names matching the parameters | 400 |
| `Query<T>` | Query string (`serde_urlencoded`) | 400 |
| `Json<T>` | JSON body. Requires `Content-Type: application/json` and enforces the body limit. | 415 / 413 / 422 |
| `Form<T>` | `application/x-www-form-urlencoded` body | 415 / 413 / 422 |
| `Bytes`, `String` | Raw body | 413 / 400 |
| `State<T>` | App state registered with `.state(v)` or a startup hook | 500 if not registered |
| `Extension<T>` | A value a middleware inserted with `req.extensions_mut().insert(v)` | 500 |
| `HeaderMap`, `Method`, `Uri` | Request metadata | — |
| `ConnInfo` | Peer address, local address, TLS flag | — |
| `MatchedPath` | Route pattern, e.g. `/users/{id}` | 500 outside a route |
| `Request` | The whole request (last argument) | — |
| `Option<T>` | Any parts extractor, made optional | never fails |
| `Result<T, Error>` | Any parts extractor, with its error handled by you | never fails |

Feature modules add more: `Valid<T>`, `Dep<T>`, `BackgroundTasks`, `BearerToken`, `Jwt<C>`, `CurrentUser<U>`, `CookieJar`, `Session`, `Multipart`, `BodyStream`, `WebSocketUpgrade`, `RequestId`. Each is covered in its own chapter.

## Path parameters in detail

```rust
#[get("/users/{id}")]                      async fn a(Path(id): Path<u64>) {}
#[get("/repos/{owner}/{name}")]            async fn b(Path((owner, name)): Path<(String, String)>) {}

#[derive(serde::Deserialize)]
struct RepoPath { owner: String, name: String }
#[get("/repos/{owner}/{name}/issues")]     async fn c(Path(p): Path<RepoPath>) {}
```

Numbers, booleans, chars, enums (unit variants) and `Option<T>` are parsed from the textual value. A value that doesn't parse returns `400` with a message that names the parameter.

## Body limits

Every buffering extractor enforces the app's body limit (default 2 MiB, configurable with `App::body_limit(bytes)`). Oversized bodies are rejected with `413` before they're read in full when `Content-Length` is known, and as soon as the limit is crossed otherwise.

## Writing your own extractor

Implement `FromRequestParts` (or `FromRequest` to consume the body). Both use `async fn`:

```rust
use zapline::extract::FromRequestParts;
use zapline::prelude::*;

pub struct Tenant(pub String);

impl FromRequestParts for Tenant {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.headers()
            .get("x-tenant")
            .and_then(|v| v.to_str().ok())
            .map(|t| Tenant(t.to_owned()))
            .ok_or_else(|| Error::bad_request("missing x-tenant header").with_code("missing_tenant"))
    }
}

async fn handler(Tenant(t): Tenant) -> String { t }
```

An extractor can do async work too, such as looking up a session or calling a service. To compute a value only once per request when several extractors need it, wrap it in `Cached<T>` (see [Dependency injection](di.md)).
