# Errors

zapline has one error type, `zapline::Error`, and one result alias, `zapline::Result<T>`.

```rust
#[get("/users/{id}")]
async fn get_user(State(db): State<Db>, Path(id): Path<u64>) -> Result<Json<User>> {
    let user = db.find(id).await?                // any std::error::Error -> 500
        .ok_or_else(Error::not_found)?;          // explicit client error -> 404
    Ok(Json(user))
}
```

## Rules

- **Any `std::error::Error` converts into a 500 with `?`.** The underlying error is logged (through `tracing` when enabled) and **never** sent to the client. The client sees `{"error":"internal server error"}`.
- **Client errors are explicit.** Use the constructors: `bad_request(msg)`, `unauthorized()`, `forbidden()`, `not_found()`, `conflict(msg)`, `unprocessable(msg)`, `too_many_requests()`, `service_unavailable(msg)`, `payload_too_large()`, `unsupported_media_type(msg)`, or `Error::new(status, msg)`.
- **Errors are structured.** Each one renders as JSON:

  ```json
  { "error": "validation failed", "code": "validation_error", "details": [ ... ] }
  ```

  `code` and `details` are omitted when unset.

```rust
Err(Error::conflict("email already registered")
    .with_code("email_taken")                       // stable, machine-readable
    .with_details(json!({ "field": "email" }))      // structured context
    .with_header(header::RETRY_AFTER, HeaderValue::from_static("30")))
```

## Customizing the error format

Every error response includes an `ErrorReport` (status, message, code, details) in its extensions. That covers extractor rejections, handler errors, 404s and 405s. `App::error_handler` re-renders all of them in one place. For example, to emit RFC 7807 `application/problem+json`:

```rust
App::new().error_handler(|e: &ErrorReport| {
    let body = serde_json::json!({
        "type": "about:blank",
        "title": e.status.canonical_reason(),
        "status": e.status.as_u16(),
        "detail": e.message,
        "code": e.code,
    });
    (e.status, [(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"))], body.to_string())
        .into_response()
})
```

## Domain errors

Map your own error enums into `zapline::Error` once, and `?` will work everywhere:

```rust
enum AppError { NotFound, Db(sqlx::Error) }

impl From<AppError> for zapline::Error {
    fn from(e: AppError) -> Self {
        match e {
            AppError::NotFound => Error::not_found().with_code("user_not_found"),
            AppError::Db(e) => Error::internal("database error").with_source(e),
        }
    }
}
```

(Implement `From` for your error type instead of `std::error::Error`. Otherwise, the blanket conversion turns it into a plain 500.)

## Diagnostic codes

The mistakes you make while building an app (a dependency nobody provides, a duplicate route, a config key nobody set, a template that does not parse, a request body that does not match its type) have stable codes, `ZL0001` to `ZL0091`. Each has a title, the cause, and the fix written as the code to add. The code appears wherever the mistake surfaces:

```text
$ cargo run
startup failed: 500 Internal Server Error: resource `my_app::Settings` is not registered [ZL0004]
  fix: Register it first: .state(value) // value: my_app::Settings; or res.insert(value) in an earlier on_startup hook
  see: zapline explain ZL0004
```

| Where | What you get |
|---|---|
| Startup errors and panics on invalid setup (duplicate routes, conflicting paths, missing Cargo features) | `[ZL0010]` after the message, then the fix and `zapline explain ZL0010` |
| `Error::diagnostic_code()` | `Some("ZL0002")` for an error the framework recognized |
| The [dev error page](#the-dev-error-page) | The code, the cause, the fix and a prompt for your AI assistant |
| The [dev console](dev-console.md) and `GET /_dev/api` | Recent errors with their codes and fixes |
| `zapline explain ZL0002` | The full entry; `zapline explain` lists them all |
| [`zapline mcp`](cli.md#ai-assistants-zapline-mcp) | `explain_error`, and every tool answer that mentions a code ends with its fix |

Codes are never renumbered or reused, so they are safe to search for, to match on in tests and to put in a bug report. They are diagnostics for developers: the JSON your clients see keeps its own `code` (`missing_dependency`, `invalid_json`), and the `ZL` code is never added to it.

```rust
let err = Resources::default().dep::<Repo>().unwrap_err();
assert_eq!(err.code(), Some("missing_dependency")); // sent to clients
assert_eq!(err.diagnostic_code(), Some("ZL0002")); // for you
```

The registry lives in one file, [`crates/zapline/src/codes.in`](https://github.com/samsamurai301/zapline/blob/main/crates/zapline/src/codes.in), also available as `zapline::codes::ALL` and `zapline::codes::lookup("ZL0002")`. The index below is generated from it.

## The dev error page

In a debug build, `App::dev_errors()` turns every 500 and every panic into a page that says what broke, for requests from your own machine:

```rust
App::new()
    .dev_errors()        // also on with .dev_console(), and whenever ZAPLINE_DEV=1 (zapline dev sets it)
    .mount(routes)
    .run("127.0.0.1:8080")
```

- **Browsers** (`Accept: text/html`) get an HTML page with the real error and its causes, the code and its fix, the file and line of a panic, the request with its headers (credentials hidden), and a **Copy fix prompt** block: one click copies a ready-made prompt for your AI assistant with the request, the error, the cause and the suggested fix.
- **JSON clients** get [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457) `application/problem+json`:

  ```json
  {"type":"https://samsamurai301.github.io/zapline/errors.html#zl0002","title":"No provider for a dependency",
   "status":500,"detail":"no provider registered for `my_app::Repo`","error":"no provider registered for `my_app::Repo`",
   "code":"ZL0002","error_code":"missing_dependency","fix":".provide(value) // value: my_app::Repo; ...",
   "request":"GET /orders"}
  ```

  (`error` repeats `detail` so clients that read the usual `{"error": ..}` keep working; `error_code` is the client-facing code.)
- **Rejected requests** (a 4xx the framework recognized, such as `invalid_json`) keep their normal body and gain an `x-zapline-diagnostic: ZL0062` header.

It is safe to leave in. A release build compiles it out (`.dev_errors()` does nothing), requests from other machines or through a proxy get the usual generic error, and when it is off nothing is added to the request path: the middleware is installed when the app is built, only if dev errors are on.

## Code index

<!--codes:start (generated from crates/zapline/src/codes.in; do not edit by hand)-->

| Code | Title |
|---|---|
| [ZL0001](#zl0001) | State not registered |
| [ZL0002](#zl0002) | No provider for a dependency |
| [ZL0003](#zl0003) | A dependency failed to initialize |
| [ZL0004](#zl0004) | Resource not registered |
| [ZL0005](#zl0005) | Request extension missing |
| [ZL0010](#zl0010) | Duplicate route |
| [ZL0011](#zl0011) | Invalid or conflicting route path |
| [ZL0012](#zl0012) | Route metadata without a route |
| [ZL0013](#zl0013) | Route information used before routing |
| [ZL0020](#zl0020) | Cargo feature not enabled |
| [ZL0021](#zl0021) | Database URL does not match the backend |
| [ZL0030](#zl0030) | Config file unreadable |
| [ZL0031](#zl0031) | Config file malformed |
| [ZL0032](#zl0032) | Config key missing |
| [ZL0033](#zl0033) | Config value has the wrong type |
| [ZL0034](#zl0034) | Invalid config profile name |
| [ZL0040](#zl0040) | Database URL not set |
| [ZL0041](#zl0041) | Cannot connect to the database |
| [ZL0042](#zl0042) | Migrations unreadable |
| [ZL0043](#zl0043) | Migration failed |
| [ZL0044](#zl0044) | No database registered |
| [ZL0045](#zl0045) | Database registered too late |
| [ZL0046](#zl0046) | No seed data |
| [ZL0050](#zl0050) | No templates registered |
| [ZL0051](#zl0051) | Template error |
| [ZL0060](#zl0060) | Path parameter rejected |
| [ZL0061](#zl0061) | Query string rejected |
| [ZL0062](#zl0062) | Malformed JSON body |
| [ZL0063](#zl0063) | JSON body has the wrong shape |
| [ZL0064](#zl0064) | Wrong Content-Type |
| [ZL0065](#zl0065) | Form body rejected |
| [ZL0066](#zl0066) | Validation failed |
| [ZL0067](#zl0067) | Body too large |
| [ZL0070](#zl0070) | No mailer registered |
| [ZL0071](#zl0071) | No cache registered |
| [ZL0072](#zl0072) | No Redis registered |
| [ZL0073](#zl0073) | No cookie key |
| [ZL0074](#zl0074) | No CSRF middleware |
| [ZL0075](#zl0075) | JWT not configured |
| [ZL0076](#zl0076) | No translations registered |
| [ZL0077](#zl0077) | No file storage registered |
| [ZL0078](#zl0078) | Authorization misconfigured |
| [ZL0080](#zl0080) | Handler panicked |
| [ZL0081](#zl0081) | Unhandled error |
| [ZL0090](#zl0090) | Dev console not mounted |
| [ZL0091](#zl0091) | Dev app not running |

### ZL0001

**State not registered.** A handler takes `State<T>`, but no value of that exact type was registered. State is looked up by type, so `State<Arc<Db>>` and `State<Db>` are different keys.

```text
App::new().state(value) // value: T; or in a startup hook: res.insert(value)
```

### ZL0002

**No provider for a dependency.** A handler takes `Dep<T>` (or code calls `res.dep::<T>()`), but nothing provides `T`. Providers are registered on the app before it starts.

```text
.provide(value) // value: T; to build it at startup: .provide_with(|res: Resources| async move { Ok(value) }); for a trait object: .provide_dyn::<dyn Trait>(Arc::new(imp))
```

### ZL0003

**A dependency failed to initialize.** The factory given to `provide_with` / `provide_injected` returned an error, so the app did not start. The message ends with the factory's own error.

```text
Fix the cause named in the message; inside the factory, resolve what it needs with res.dep::<Other>()? and register Other before it
```

### ZL0004

**Resource not registered.** `Resources::require::<T>()` found no value of type `T`. Startup hooks run in the order they were registered, so a hook can only require what an earlier hook or `.state(..)` inserted.

```text
Register it first: .state(value) // value: T; or res.insert(value) in an earlier on_startup hook
```

### ZL0005

**Request extension missing.** A handler takes `Extension<T>`, but no middleware inserted a `T` into this request's extensions, or the middleware runs after the handler is reached.

```text
In a middleware that runs before the handler: req.extensions_mut().insert(value); // value: T, installed with .wrap(..) or .wrap_route(..)
```

### ZL0010

**Duplicate route.** Two handlers were registered for the same method and path. zapline refuses to pick one silently; `zapline routes` prints the whole table.

```text
Remove one of the two registrations of T, or change its method or path
```

### ZL0011

**Invalid or conflicting route path.** The router cannot add this path: two routes give the same segment different parameter names (`/users/{id}` and `/users/{name}`), a catch-all is not the last segment, or a brace is unbalanced.

```text
Use one parameter name per position (/users/{id} everywhere) and keep catch-alls last: /files/{*path}
```

### ZL0012

**Route metadata without a route.** `Routes::meta(..)` (or `App::meta`) attaches metadata to the route registered just before it, and none was registered yet.

```text
Call it right after the route: .get("/x", handler).meta(value)
```

### ZL0013

**Route information used before routing.** `MatchedPath` (or `req.route()`) was used in global middleware, which runs before the router has matched the request.

```text
Install that middleware after routing: .wrap_route(mw) instead of .wrap(mw)
```

### ZL0020

**Cargo feature not enabled.** The app asks for something that is compiled only with a zapline Cargo feature that is off.

```text
In Cargo.toml: zapline = { version = "0.1", features = ["T"] }
```

### ZL0021

**Database URL does not match the backend.** The database URL names one backend (`postgres://`, `sqlite://`) while the app's `Database` uses the other, usually because only one of the `postgres` and `sqlite` features is enabled.

```text
Use a URL for the compiled backend (T), or enable the other feature: zapline = { version = "0.1", features = ["postgres"] } and Database::postgres(url)
```

### ZL0030

**Config file unreadable.** A configuration file exists but cannot be read (permissions, a directory in its place, invalid UTF-8).

```text
Fix the file's permissions or contents: T
```

### ZL0031

**Config file malformed.** A TOML layer (`config/default.toml`, `config/<profile>.toml`) or `.env` does not parse. The message gives the file and the line.

```text
Fix the syntax at the reported line; TOML strings need quotes: name = "value"
```

### ZL0032

**Config key missing.** The settings struct has a required field that no layer sets.

```text
Set T in config/default.toml, or as an environment variable (APP__ prefix, __ between levels, e.g. APP__SERVER__PORT=8080), or make the field Option<_> / #[serde(default)]
```

### ZL0033

**Config value has the wrong type.** A config value does not deserialize into the field's type. The message says which layer set it.

```text
Give T a value of the field's type in the layer named in the message (numbers unquoted: port = 8080)
```

### ZL0034

**Invalid config profile name.** The profile (`APP_PROFILE` or `Config::builder().profile(..)`) contains characters other than ASCII letters, digits, `-` and `_`.

```text
Use a plain name: APP_PROFILE=production
```

### ZL0040

**Database URL not set.** `Database::from_env()` reads `DATABASE_URL` (or the variable you named), and it is not set.

```text
Add to .env: DATABASE_URL=sqlite://app.db?mode=rwc (or postgres://user:pass@localhost/app)
```

### ZL0041

**Cannot connect to the database.** The pool could not open a connection at startup: the server is down, the credentials are wrong, or a SQLite file cannot be created.

```text
Check DATABASE_URL; for SQLite add ?mode=rwc so the file is created; for Postgres start it, e.g. docker run -p 5432:5432 -e POSTGRES_PASSWORD=pass postgres
```

### ZL0042

**Migrations unreadable.** The migrations directory exists but its files cannot be read or are not named `<version>_<name>.sql`.

```text
Create migrations with: zapline db new <name> (names them <timestamp>_<name>.sql)
```

### ZL0043

**Migration failed.** A migration's SQL failed, or an applied migration was edited (its checksum changed). Applied migrations are immutable.

```text
Fix the SQL of the migration named in the message; to change an applied one, add a new migration: zapline db new fix_<name> (in development you can also delete the SQLite file)
```

### ZL0044

**No database registered.** A handler takes `Db`, but the app never called `.database(..)`. With both the `postgres` and `sqlite` features on, `Db` means Postgres; use `SqliteDb` for SQLite.

```text
App::new().database(Database::from_env())
```

### ZL0045

**Database registered too late.** Jobs and seed data use the pool that `.database(..)` creates at startup, so it has to be registered before them.

```text
App::new().database(Database::from_env()).jobs(Jobs::new().register::<MyJob>()).seed(|db| async move { Ok(()) })
```

### ZL0046

**No seed data.** `zapline db seed` ran, but the app has no seed function.

```text
.seed(|db| async move { Factory::new("todos").create(&db).await?; Ok(()) }) // after .database(..)
```

### ZL0050

**No templates registered.** A handler takes `Views`, but the app has no template directory.

```text
App::new().views("templates")
```

### ZL0051

**Template error.** A template does not exist, does not parse, or failed while rendering (an undefined filter, a wrong type). The message names the template and line. Templates are found relative to the working directory, i.e. the project root under `cargo run`.

```text
Fix the template at the reported line; extend layouts with {% extends "base.html" %} and pass every variable the template uses: views.render("page.html", context! { title })
```

### ZL0060

**Path parameter rejected.** A `Path<T>` parameter did not parse into its type, e.g. `/users/abc` for `Path<u64>`. The client gets 400 `invalid_path`.

```text
Send a value of the parameter's type, or take it as text: Path(id): Path<String>
```

### ZL0061

**Query string rejected.** `Query<T>` could not deserialize the query string: a missing required field or a value of the wrong type. The client gets 400 `invalid_query`.

```text
Make optional fields Option<_> or #[serde(default)] in the query struct
```

### ZL0062

**Malformed JSON body.** The body of a `Json<T>` request is not valid JSON. `details` has the line and column. The client gets 422 `invalid_json`.

```text
Send valid JSON with Content-Type: application/json, e.g. curl -H 'content-type: application/json' -d '{"name":"x"}'
```

### ZL0063

**JSON body has the wrong shape.** The body is valid JSON but does not match `T`: a missing field, a wrong type, an unknown variant. `details.path` points at the value. The client gets 422 `invalid_body`.

```text
Match the request type, or relax it: Option<_> / #[serde(default)] for fields clients may leave out
```

### ZL0064

**Wrong Content-Type.** `Json<T>` needs `application/json` and `Form<T>` needs `application/x-www-form-urlencoded`. The client gets 415 `unsupported_media_type`.

```text
Send the header: Content-Type: application/json (fetch: headers: { 'content-type': 'application/json' })
```

### ZL0065

**Form body rejected.** `Form<T>` could not deserialize the form. The client gets 422 `invalid_form`.

```text
Name the inputs like the struct's fields; checkboxes are absent when unchecked, so use #[serde(default)] bool
```

### ZL0066

**Validation failed.** `Valid<T>` ran the struct's `#[validate(..)]` rules and some failed. `details` lists them per field. The client gets 422 `validation_error`.

```text
Send values that pass the rules shown in details, or change the rules: #[validate(length(min = 1))]
```

### ZL0067

**Body too large.** The request body is larger than the app's limit (2 MiB by default). The client gets 413.

```text
Raise the limit: App::new().body_limit(16 * 1024 * 1024); or stream the body with BodyStream
```

### ZL0070

**No mailer registered.** A handler takes `Mailer`, but the app has none.

```text
App::new().mailer(Mailer::from_env()?) // SMTP_URL; without it, mail goes to the dev inbox at /_dev/mail
```

### ZL0071

**No cache registered.** A handler takes `Cache`, but the app has none.

```text
App::new().cache(Cache::memory())
```

### ZL0072

**No Redis registered.** A handler takes `Redis`, but the app has none.

```text
App::new().redis(Redis::from_env()?) // REDIS_URL=redis://localhost:6379
```

### ZL0073

**No cookie key.** Signed or private cookies (and sessions) need a key, and none was registered.

```text
App::new().cookie_key(zapline::cookies::key_from_env("SESSION_SECRET")?) // 32+ random bytes: openssl rand -base64 48
```

### ZL0074

**No CSRF middleware.** A handler takes `CsrfToken`, but the `Csrf` middleware that issues tokens is not installed.

```text
App::new().wrap(Csrf::new())
```

### ZL0075

**JWT not configured.** JWT extraction or signing needs a `JwtConfig`, and none was registered.

```text
App::new().jwt(JwtConfig::hs256(std::env::var("JWT_SECRET")?))
```

### ZL0076

**No translations registered.** A handler takes `Locale`, but the app has no translations.

```text
App::new().i18n(I18n::load("locales")?)
```

### ZL0077

**No file storage registered.** A handler takes `Storage`, but the app has none.

```text
App::new().storage(Storage::from_env()?) // a local directory, or S3 when S3_BUCKET is set
```

### ZL0078

**Authorization misconfigured.** A route requires roles or scopes, but nothing checks them after routing, or a `Guard` was installed as global middleware. Policies fail closed: the route answers 500 instead of letting the request through.

```text
Check roles after routing: .wrap_route(Authenticate::new(authenticator).authorize()) or .wrap_route(guard), never .wrap(guard)
```

### ZL0080

**Handler panicked.** A handler or middleware panicked. The panic was caught and answered with 500. Panics usually come from `unwrap()`, `expect()` or indexing.

```text
Return an error instead of panicking: replace .unwrap() with ? (or .ok_or_else(Error::not_found)?) in the function at the reported location
```

### ZL0081

**Unhandled error.** An error reached the client as a 500 through `?`: any std error converts into a logged 500 with a generic message. Client mistakes should get a 4xx instead.

```text
Map expected failures to client errors: .ok_or_else(Error::not_found)? or .map_err(|e| Error::bad_request(e.to_string()))?; keep real failures as Error::internal("what failed").with_source(e)
```

### ZL0090

**Dev console not mounted.** The live tools of `zapline mcp` read the running app's `/_dev` console, which this app does not serve. It exists only in debug builds.

```text
App::new().dev_console() // debug builds only; does nothing in release
```

### ZL0091

**Dev app not running.** Nothing answers at the app's address. The live tools need the app running, ideally under `zapline dev`, which records its address in target/zapline-dev.addr.

```text
Start it: zapline dev (or pass the address: url = "http://127.0.0.1:8080")
```

<!--codes:end-->
