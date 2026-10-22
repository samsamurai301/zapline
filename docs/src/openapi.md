# OpenAPI & interactive docs

Add one line, and your API documents itself: an OpenAPI 3.1 spec at `/openapi.json` plus Swagger UI at `/docs`.

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zapline::openapi::OpenApi;
use zapline::prelude::*;

#[derive(Serialize, Deserialize, JsonSchema)]
struct Book {
    id: u64,
    title: String,
}

/// Get a book.
///
/// Returns 404 when the book does not exist.
#[get("/books/{id}", tag = "books")]
async fn get_book(Path(id): Path<u64>) -> Option<Json<Book>> {
    (id == 1).then(|| Json(Book { id, title: "Dune".into() }))
}

fn main() -> std::io::Result<()> {
    App::new()
        .mount(get_book)
        .openapi(OpenApi::new("Bookshelf", "1.0.0"))
        .run("0.0.0.0:8080")
}
```

The route macros read the doc comment and the handler signature (`Path<T>`, `Query<T>`, `Json<T>`, `Option<T>`, `Result<T>`, ...) and the plugin turns the route table into a spec. The spec is generated **once, at startup**, and served as static bytes, so documentation costs nothing per request.

Requirements:

- Cargo features `openapi` (on by default, implies `json`) and `macros` (on by default) for the automatic parts.
- Types appear in the spec when they implement `JsonSchema`. `#[model]` derives it for you (along with serde and `Validate`) with no extra dependency. The plain derive expands to paths in the `schemars` crate, so add it to your `Cargo.toml`:

  ```toml
  schemars = "1"
  ```

  If you would rather not depend on `schemars` directly, point the derive at the re-export with `#[schemars(crate = "zapline::openapi::schemars")]` (the trait is `zapline::openapi::JsonSchema`).

Types without `JsonSchema` still work as handler arguments and return values. They are simply not described.

## What is described automatically

For routes registered with a macro (`#[get]`, `#[post]`, `#[route]`, ...), the arguments and the return type are inspected:

| Handler type | Documented as |
|---|---|
| `Path<T>` | Path parameters (a struct by field name, a tuple by position, or a scalar). Parameters that appear only in a `nest` prefix are included. |
| `Query<T>` | One query parameter per field of `T`. `Option<_>` fields are optional. |
| `Json<T>` | JSON request body, plus a `422` response |
| `Form<T>` | `application/x-www-form-urlencoded` body, plus a `422` response |
| `Bytes`, `String` | `application/octet-stream` / `text/plain` body |
| `Valid<E>` | Whatever `E` documents, plus a `422 Validation Error` response (see [Validation](validation.md)) |
| `Option<E>` (argument) | Makes `E`'s parameters and body optional |
| `Json<T>` (return) | `application/json` response of `T` |
| `String`, `&'static str` | `text/plain` response |
| `Bytes`, `Vec<u8>` | `application/octet-stream` response |
| `Html<T>` | `text/html` response |
| `()`, `StatusCode` | Response without a body |
| `Redirect` | `303` with a `Location` header |
| `Option<T>` (return) | `T`'s response plus a `404` |
| `Result<T, E>` (return) | `T`'s response plus a `default` error response using the `Error` schema |
| `(StatusCode, T)`, `([headers; N], T)` | `T`'s response |

Operation IDs default to the handler's function name (builder routes get `<method>_<path>`, such as `get_users_id`). Duplicates get a suffix, so mounting the same handler twice is fine.

## Macro arguments

Everything after the path in `#[get("/path", ...)]` documents the route. `#[route]` takes the same arguments plus `method = "..."`.

| Argument | Example | Effect |
|---|---|---|
| `tag` | `tag = "users"` | Adds a tag. Docs UIs group operations by tag. |
| `tags` | `tags("users", "admin")` or `tags = ["users", "admin"]` | Adds several tags |
| `summary` | `summary = "Create a user"` | One-line summary. Overrides the doc comment. |
| `description` | `description = "Longer text (CommonMark)"` | Overrides the rest of the doc comment |
| `operation_id` | `operation_id = "createUser"` | Explicit `operationId`. Must not be empty. |
| `status` | `status = 201` | Documents the success status. **It does not change the real status.** Return `(StatusCode::CREATED, ...)` yourself. |
| `deprecated` | `deprecated` | Marks the operation deprecated |
| `hidden` | `hidden` | Leaves the route out of the spec |
| `security` | `security = "bearer"` or `security("bearer", "api_key")` | Requires a scheme declared on `OpenApi`. Several schemes are alternatives. |

Unknown arguments, duplicates (`summary` twice) and bad values are compile errors that list what is allowed.

```rust
use zapline::prelude::*;

/// Create a user.
///
/// The new user receives a welcome email. Emails must be **unique**.
#[post("/users", tags("users", "admin"), status = 201, security = "bearer")]
async fn create_user(Json(name): Json<String>) -> (StatusCode, Json<String>) {
    (StatusCode::CREATED, Json(name))
}

#[get("/v1/users", deprecated, operation_id = "listUsersV1")]
async fn old_list() -> Json<Vec<String>> {
    Json(vec![])
}

#[get("/internal/debug", hidden)]
async fn debug() -> &'static str {
    "ok"
}
```

### Doc comments

The doc comment of a macro route becomes the operation text: the first paragraph is the **summary** (its lines are joined into one) and the rest is the **description**, rendered as CommonMark. `summary = ".."` and `description = ".."` take precedence. Doc comments on `JsonSchema` struct fields become property descriptions, too.

## Routes without macros

Routes registered with `.get(..)`, `.post(..)` and friends have no signature to probe, so describe them with builder methods. They apply to the most recently registered route, on both `App` and `Routes`, and they panic if no route has been registered yet.

| Method | Effect |
|---|---|
| `.summary(text)` / `.description(text)` | Operation text |
| `.tag(name)` | Adds a tag |
| `.operation_id(id)` | Explicit `operationId` |
| `.deprecated()` / `.hidden()` | Mark deprecated / leave out of the spec |
| `.security(scheme)` | Requires a declared security scheme |
| `.response::<T>(status, description)` | Documents a JSON response of type `T` |
| `.request_body::<T>()` | Documents a JSON request body of type `T` |
| `.api_doc(\|doc\| { .. })` | Full access to the route's `OperationDoc` |

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zapline::prelude::*;

#[derive(Deserialize, JsonSchema)]
struct ReindexOptions {
    full: bool,
}

#[derive(Serialize, JsonSchema)]
struct Report {
    documents: u64,
}

async fn reindex(Json(opts): Json<ReindexOptions>) -> Json<Report> {
    Json(Report { documents: if opts.full { 1000 } else { 10 } })
}

fn app() -> App {
    App::new()
        .get("/health", || async { "ok" })
        .summary("Liveness probe")
        .tag("ops")
        .post("/reindex", reindex)
        .summary("Rebuild the search index")
        .request_body::<ReindexOptions>()
        .response::<Report>(200, "The rebuild report")
        .api_doc(|doc| {
            doc.response_empty(202, "Rebuild queued")
                .error_response(409, "A rebuild is already running");
        })
}
```

`OperationDoc` has more methods for unusual routes: `request_body_as::<T>(content_type)`, `request_body_raw(content_type)`, `success::<T>(content_type)`, `success_raw(content_type)`, `success_empty()`, `default_error()`, `redirect()`, `success_status(code)`, `path_params::<T>()`, `query_params::<T>()` and `param::<T>(ParamIn::Header, "x-tenant", required)`.

## Groups

`Routes::tag_all` and `Routes::security_all` set defaults for every route in a group, including routes added later. Route-level tags are added after the group's. A route's own `security` replaces the group's. When groups are nested, the innermost group that sets a default wins.

```rust
use zapline::prelude::*;

fn admin() -> Routes {
    Routes::new()
        .get("/stats", || async { "stats" })
        .summary("Admin statistics")
        .delete("/cache", || async { StatusCode::NO_CONTENT })
        .tag_all("admin")
        .security_all("bearer")
}

fn app() -> App {
    App::new().nest("/admin", admin())
}
```

## The `OpenApi` plugin

`OpenApi::new(title, version)` is a plugin, installed with `App::openapi(..)`. All other methods are optional:

| Method | Effect |
|---|---|
| `.description(text)` | API description (CommonMark) |
| `.server(url)` / `.server_with(url, description)` | Server URLs |
| `.contact(name, email)` / `.license(name)` | `info` metadata |
| `.tag(name, description)` | Describes a tag. Tags are listed in declaration order. |
| `.bearer_auth(name)` / `.basic_auth(name)` / `.api_key_auth(name, header)` | Declares a security scheme |
| `.security_scheme(name, json)` | Declares any scheme object (OAuth 2, OpenID Connect, ...) |
| `.security(name)` | Requires a scheme for every operation that doesn't set its own |
| `.json_path(path)` | Where the spec is served (default `/openapi.json`) |
| `.spec_url(url)` | The URL docs pages load the spec from. Set it behind a path-rewriting proxy. |
| `.docs(DocsUi, path)` | Serves a docs UI. The first call replaces the default (Swagger at `/docs`), further calls add more. |
| `.no_docs()` | Serves only the JSON spec |
| `.cdn(base_url)` | Loads UI assets from a mirror instead of jsDelivr |
| `.modify(\|spec\| { .. })` | Post-processes the generated `serde_json::Value` |

```rust
use zapline::openapi::{DocsUi, OpenApi};
use zapline::prelude::*;

fn app() -> App {
    App::new().get("/", || async { "hi" }).openapi(
        OpenApi::new("My API", "1.2.0")
            .description("Everything about my service.")
            .server("https://api.example.com")
            .server_with("https://staging.example.com", "Staging")
            .tag("users", "User management")
            .bearer_auth("bearer")
            .docs(DocsUi::Swagger, "/docs")
            .docs(DocsUi::Scalar, "/reference")
            .docs(DocsUi::Redoc, "/redoc"),
    )
}
```

The documentation routes themselves never appear in the spec.

### Docs UIs

| `DocsUi` | Looks like |
|---|---|
| `Swagger` | [Swagger UI](https://swagger.io/tools/swagger-ui/). Lets you try requests in the browser. |
| `Scalar` | [Scalar](https://scalar.com). A modern reference with a built-in client. |
| `Redoc` | [Redoc](https://redocly.com/redoc). A clean three-panel reference. |

The pages are plain HTML rendered once at startup. Their assets load from jsDelivr with **exact pinned versions and Subresource Integrity hashes**, so a compromised CDN cannot inject code. In an air-gapped network, mirror the npm packages (the files must be byte-identical) and call `.cdn("https://npm.internal.example")`.

## Security schemes

Declare the schemes on the plugin, then opt routes in with `security = "name"` (macro), `.security("name")` (builder) or `Routes::security_all("name")` (group).

```rust
use zapline::openapi::OpenApi;
use zapline::prelude::*;

#[get("/me", security = "bearer")]
async fn me() -> &'static str {
    "ada"
}

fn app() -> App {
    App::new().mount(me).openapi(
        OpenApi::new("Secure API", "1.0.0")
            .bearer_auth("bearer")                      // Authorization: Bearer <jwt>
            .api_key_auth("api_key", "x-api-key")       // a header
            .basic_auth("basic"),
    )
}
```

`OpenApi::security(name)` makes a scheme the default for **all** operations instead.

Documenting a scheme does not enforce anything, and enforcing something does not document it. Authentication policies such as `.require_roles(..)` (see [Authentication & authorization](auth.md)) are separate from the `security` metadata: use both.

## Schemas

Request bodies are described the way they *deserialize* and responses the way they *serialize*, because the two can differ (`Option<T>` fields are optional in a request but always present in a response, serde defaults, and so on). When the schemas of one type differ, the request side is published as `<Name>Input`, otherwise one component is shared. All named types go under `components/schemas`, and the framework's error body is published as `Error`.

`#[derive(JsonSchema)]` follows serde attributes (`rename`, `default`, `skip`, `flatten`, ...). It also recognizes `validator` rules, so `#[validate(length(min = 1, max = 64))]` becomes `minLength` / `maxLength`, `range` becomes `minimum` / `maximum` and `email` becomes `format: email`. See [Validation](validation.md).

## Custom extractors and responses

A macro route documents an argument or return type when it implements `ApiInput` / `ApiOutput`. Implement them for your own types:

```rust
use zapline::extract::FromRequestParts;
use zapline::openapi::{ApiInput, OperationDoc, ParamIn};
use zapline::prelude::*;

struct TenantId(String);

impl FromRequestParts for TenantId {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        let value = req.headers().get("x-tenant").and_then(|v| v.to_str().ok());
        value
            .map(|v| TenantId(v.to_owned()))
            .ok_or_else(|| Error::bad_request("missing x-tenant header"))
    }
}

impl ApiInput for TenantId {
    fn describe_input(op: &mut OperationDoc) {
        op.param::<String>(ParamIn::Header, "x-tenant", true);
    }
}

#[get("/reports")]
async fn reports(TenantId(tenant): TenantId) -> String {
    tenant
}
```

The framework's own security extractors (`BearerToken`, `Jwt`, `CurrentUser`, ...) do not document themselves. Add `security = "..."` to the route.

## Exporting the spec

Set `ZAPLINE_EXPORT_OPENAPI` to a file path and run your app. The plugin writes the spec as pretty-printed JSON and exits (status 0) from its build hook, before any port is bound and before any startup hook runs:

```sh
ZAPLINE_EXPORT_OPENAPI=openapi.json cargo run --bin my-api
```

The `zapline openapi` command does exactly this for you (see [Project layout & the CLI](cli.md)). It is what you want in CI, to publish the spec or to generate client SDKs. Do not set the variable in a test environment: `App::test()` builds the app too, and the process would exit.

In tests, generate the spec without starting anything and assert on it:

```rust
use zapline::openapi::{spec_for, OpenApi};
use zapline::prelude::*;

#[test]
fn health_is_documented() {
    let app = App::new().get("/health", || async { "ok" }).summary("Liveness probe");
    let spec = spec_for(app, OpenApi::new("Test", "1.0.0"));
    assert_eq!(spec["openapi"], "3.1.0");
    assert_eq!(spec["paths"]["/health"]["get"]["summary"], "Liveness probe");
}
```

`zapline::openapi::export_spec(&spec, path)` is the function behind the environment variable.

## Limitations

- Only routes registered with a macro are documented from their signature. Builder routes need explicit `.response::<T>()` / `.request_body::<T>()` calls.
- A handler returning `impl IntoResponse` gets a generic success response, because its type cannot be named. Document it with `.response::<T>(..)`. Wrappers such as `Result<impl IntoResponse>` still get the error response.
- Generic handler functions cannot be inspected. Their macro arguments still apply.
- `.any(..)` routes and methods OpenAPI cannot express (`CONNECT`, custom methods) are left out.
- Auth policies, `Multipart`, `Dep<T>`, `CurrentUser<U>` and other extractors do not appear in the spec unless they implement `ApiInput`. For uploads, use `.api_doc(|d| { d.request_body_raw("multipart/form-data"); })`.
- An operation cannot opt out of a global `OpenApi::security(..)` default. Use `.modify(..)` to set `"security": []` on a path yourself, or declare security per route or group instead.
- Response headers are documented only for redirects.
- The docs pages need the browser to reach the CDN (or your mirror). The JSON spec does not.
