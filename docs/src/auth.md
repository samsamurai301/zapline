# Authentication & authorization

Authenticate a group of routes with JWTs, then declare who may call what, next to the route:

```rust
use zapline::auth::{Authenticate, Claims, CurrentUser, JwtConfig};
use zapline::prelude::*;

async fn me(CurrentUser(claims): CurrentUser<Claims>) -> String {
    format!("hello {}", claims.sub)
}

async fn purge_cache() -> &'static str {
    "purged"
}

fn app() -> App {
    let api = Routes::new()
        .get("/me", me)
        .delete("/cache", purge_cache)
        .require_roles(["admin"])         // authorization: declarative, next to the route
        .wrap(Authenticate::jwt());       // authentication: who is calling?

    App::new()
        .jwt(JwtConfig::hs256(std::env::var("JWT_SECRET").expect("JWT_SECRET")))
        .nest("/api", api)
}
```

Authentication is split in two so each part can be swapped:

1. **Authentication** (*who is it?*) is done by an `Authenticator`, run by the `Authenticate` route middleware. It stores the user for the `CurrentUser<U>` extractor.
2. **Authorization** (*may they?*) is declared as route **policies** (`.require_auth()`, `.require_roles(..)`, `.require_scopes(..)`) and enforced by `Authenticate` (with `.authorize()`) or by `Guard`.

Policies **fail closed**: a route with a policy can never become public because a middleware was forgotten or installed in the wrong place. See [Policies fail closed](#policies-fail-closed).

Cargo features: everything here is always available except JWT, which needs `jwt` (it pulls in `jsonwebtoken`), and password hashing, which needs `password`. CSRF is in [Cookies, sessions & CSRF](sessions.md).

## Accounts in one command: `zapline g auth`

For apps with users who sign up with an email and password, generate the whole thing instead of assembling it. In a project with a database (the `fullstack` template, or any project with `migrations/`):

```sh
zapline g auth
```

| File | What it holds |
|---|---|
| `migrations/<ts>_create_users.sql` | `users` table: unique (lowercased) email, Argon2id password hash |
| `src/routes/auth.rs` | `POST /api/auth/signup`, `/login`, `/logout`, `GET /api/auth/me`; the `CurrentUser` extractor; sessions stored in the database |
| `templates/auth/signup.html`, `login.html` | htmx pages at `/signup` and `/login` (when `templates/base.html` exists) |
| `tests/auth.rs` | Sign-up, log-in, log-out, wrong passwords, duplicates, weak input, the HTML forms |

It also enables zapline's `cookies` and `password` features and compiles Argon2 optimized in debug builds, so tests stay fast.

Protect a handler by taking the generated extractor; it answers `401 login_required` when nobody is logged in:

```rust
use crate::routes::auth::CurrentUser;

#[get("/api/notes")]
async fn my_notes(db: Db, CurrentUser(user): CurrentUser) -> Result<Json<Vec<Note>>> {
    let notes = sqlx::query_as::<_, Note>("SELECT * FROM notes WHERE user_id = $1")
        .bind(user.id)
        .fetch_all(&*db)
        .await?;
    Ok(Json(notes))
}
```

Take `Option<CurrentUser>` to serve visitors and users differently. The generated code is yours to change: it is about 200 lines with no framework magic.

What it does for you: passwords hashed with Argon2id on the blocking thread pool; a fresh session id at login (no session fixation); the same error and the same timing for an unknown email and a wrong password (`password::verify_missing`); sessions in the database, so logins survive restarts and deploys; `HttpOnly`, `SameSite=Lax` cookies, `Secure` in release builds. In production set `SESSION_SECRET` to 32 or more random bytes (`openssl rand -base64 48`): release builds refuse to start without it.

## Passwords

With the `password` feature, `zapline::auth::password` hashes and checks passwords with Argon2id (OWASP parameters, PHC strings that carry their salt and parameters):

```rust
use zapline::auth::password;

let hash = password::hash("correct horse battery staple").await?;
assert!(password::verify("correct horse battery staple", &hash).await?);
```

Both run on the blocking thread pool, since a hash takes tens of milliseconds by design. `password::verify_missing(pw)` spends the same time and returns `false`: call it when the account doesn't exist. `hash_blocking` and `verify_blocking` are the synchronous versions.

## Credential extractors

Three extractors read credentials without any setup. Missing or malformed credentials are rejected with `401`, a `WWW-Authenticate` challenge and a stable error code: `missing_credentials` or `invalid_credentials`.

| Extractor | Header | Challenge | Check with |
|---|---|---|---|
| `BearerToken` | `Authorization: Bearer <token>` | `Bearer` | `token.token()` and `constant_time_eq` |
| `BasicAuth` | `Authorization: Basic <base64>` | `Basic realm="Restricted", charset="UTF-8"` | `auth.verify(user, pass)` |
| `ApiKey` | `x-api-key: <key>` | `ApiKey header="x-api-key"` | `key.verify(expected)`, `key.verify_any(keys)` |

Use `Option<BearerToken>` (or any of the others) for optional credentials. The extractors never compare secrets for you. Do it in constant time:

```rust
use zapline::auth::{constant_time_eq, ApiKey, BasicAuth, BearerToken};
use zapline::prelude::*;

async fn admin(token: BearerToken) -> Result<&'static str> {
    if !constant_time_eq(token.token(), "let-me-in") {
        return Err(Error::forbidden());
    }
    Ok("welcome")
}

async fn metrics(auth: BasicAuth) -> Result<&'static str> {
    if !auth.verify("ops", "correct horse battery staple") {
        return Err(auth.reject());          // 401 invalid_credentials + Basic challenge
    }
    Ok("# metrics")
}

async fn report(key: ApiKey) -> Result<&'static str> {
    if !key.verify_any(["k-123", "k-456"]) {
        return Err(key.reject());
    }
    Ok("report")
}
```

`constant_time_eq(a, b)` compares byte strings without leaking their contents through timing (only a length difference is visible). `BasicAuth::verify` and `ApiKey::verify_any` always run every comparison. All three types redact their secrets in `Debug` output.

Customize where the credentials come from by registering a config as state:

```rust
use zapline::auth::{ApiKeyConfig, BasicAuthConfig};
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .state(BasicAuthConfig::new("admin area"))                           // realm in the challenge
        .state(ApiKeyConfig::new().header("x-token").query_param("api_key")) // keys in URLs end up in logs: prefer headers
}
```

## JWT

Needs the `jwt` feature. Register a `JwtConfig` with `App::jwt`, then either extract claims with `Jwt<C>` or authenticate a group with `Authenticate::jwt()`.

```rust
use std::time::Duration;
use zapline::auth::{Claims, Jwt, JwtConfig};
use zapline::prelude::*;

// Issue a token (for example in a login handler).
async fn login(State(jwt): State<JwtConfig>) -> Result<String> {
    let claims = Claims::new("user-1")
        .roles(["admin"])
        .scopes(["reports:read"])
        .claim("tenant", "acme");
    jwt.encode(&claims)
}

// Verify one: the `Jwt` extractor validates the bearer token and yields its claims.
async fn whoami(Jwt(claims): Jwt) -> String {
    claims.sub
}

fn app() -> App {
    let jwt = JwtConfig::hs256("an-example-secret-of-at-least-32-bytes")
        .issuer("https://auth.example.com")
        .audience("api")
        .ttl(Duration::from_secs(15 * 60));

    App::new().jwt(jwt).post("/login", login).get("/whoami", whoami)
}
```

`Jwt<C>` defaults to `Claims`. Use `Jwt<MyClaims>` for your own claims type (`C: Deserialize`). A missing token is `401 missing_credentials`, an expired one `401 token_expired` and anything else wrong `401 invalid_token`, all with a `Bearer` challenge. Without `App::jwt(..)` the extractor fails with `500 auth_misconfigured`.

### `JwtConfig`

| Constructor | Algorithm |
|---|---|
| `JwtConfig::hs256(secret)`, `hs384`, `hs512` | HMAC with a shared secret. **Panics if the secret is shorter than 32 bytes.** |
| `JwtConfig::rs256_pem(public_pem, Some(private_pem))` | RSA. Pass `None` to verify tokens without being able to issue them. |
| `JwtConfig::es256_pem(public_pem, Some(private_pem))` | ECDSA P-256 (PKCS#8 private key) |
| `JwtConfig::try_rs256_pem` / `try_es256_pem` | Fallible versions that return an error instead of panicking on a bad key |
| `JwtConfig::new(algorithm, decoding_key, encoding_key)` | Full control over algorithm and keys |

| Builder method | Effect |
|---|---|
| `.issuer(iss)` | Tokens must carry this `iss`. Issued tokens get it. |
| `.audience(aud)` | An accepted audience (call again to accept several). Tokens must carry a matching `aud`. Issued tokens get the first one. |
| `.ttl(duration)` | Lifetime of issued tokens when the claims have no `exp` (default 1 hour) |
| `.leeway(duration)` | Clock-skew tolerance for `exp` and `nbf` (default 60 s) |
| `.required_claims(names)` | Claims that must be present, from `exp`, `nbf`, `iss`, `aud`, `sub` (default: `exp`) |
| `.key_id(kid)` | `kid` header for issued tokens (key rotation) |
| `.validation(\|v\| ..)` | Adjust the underlying `jsonwebtoken::Validation` |

The configured algorithm is pinned, so a token cannot choose its own (no `alg: none`, no HS/RS confusion). `exp` is required and validated, `nbf` is validated when present, and tokens longer than 16 KiB are rejected before parsing. `encode` fills in a missing `iat`, `exp` (now + ttl), `iss` and `aud`.

### `Claims`

| Field | Notes |
|---|---|
| `sub`, `exp`, `iat`, `nbf`, `iss`, `aud` | Registered claims. `aud` accepts a string or an array. |
| `roles` | A string or an array. Checked by `.require_roles(..)`. |
| `scopes` | An array, or the OAuth `scope` / `scp` claim as a space-separated string. Checked by `.require_scopes(..)`. |
| `extra` | Every other claim. Read it with `claims.get::<T>("tenant")`. |

Build claims with `Claims::new(sub)`, then `.roles(..)`, `.scopes(..)`, `.claim(key, value)`, `.expires_in(duration)`, `.expires_at(unix_secs)`, `.issuer(..)` and `.audience(..)`. `claims.has_role("admin")` and `claims.has_scope("x")` are available too.

For your own claims type, implement `HasRoles` and use `JwtAuthenticator`:

```rust
use serde::{Deserialize, Serialize};
use zapline::auth::{Authenticate, CurrentUser, HasRoles, JwtAuthenticator};
use zapline::prelude::*;

#[derive(Clone, Serialize, Deserialize)]
struct AppClaims {
    sub: String,
    exp: u64,
    #[serde(default)]
    roles: Vec<String>,
    tenant: String,
}

impl HasRoles for AppClaims {
    fn roles(&self) -> &[String] {
        &self.roles
    }
}

async fn tenant(CurrentUser(c): CurrentUser<AppClaims>) -> String {
    c.tenant
}

fn api() -> Routes {
    Routes::new()
        .get("/tenant", tenant)
        .wrap(Authenticate::new(JwtAuthenticator::<AppClaims>::new()).required().authorize())
}
```

## Custom authenticators

Anything that can turn a request into a user (sessions, API keys looked up in a database, mTLS, an external identity provider) is an `Authenticator`. Return:

- `Ok(None)` when the request carries no credentials for this scheme,
- `Ok(Some(user))` when they are valid,
- `Err(..)`, typically a `401`, when they are present but invalid.

```rust
use zapline::auth::{constant_time_eq, Authenticate, Authenticator, CurrentUser, HasRoles};
use zapline::prelude::*;

#[derive(Clone)]
struct User {
    name: String,
    roles: Vec<String>,
}

impl HasRoles for User {
    fn roles(&self) -> &[String] {
        &self.roles
    }
}

struct StaticKeys;

impl Authenticator for StaticKeys {
    type User = User;

    async fn authenticate(&self, req: &Request) -> Result<Option<User>> {
        let Some(key) = req.headers().get("x-api-key") else {
            return Ok(None);
        };
        if constant_time_eq(key.as_bytes(), "k-admin") {
            return Ok(Some(User { name: "ops".into(), roles: vec!["admin".into()] }));
        }
        Err(Error::unauthorized().with_code("invalid_credentials"))
    }
}

async fn who(CurrentUser(user): CurrentUser<User>) -> String {
    user.name
}

fn admin() -> Routes {
    Routes::new()
        .get("/stats", || async { "stats" })
        .require_roles(["admin"])
        .get("/who", who)
        .wrap(Authenticate::new(StaticKeys).authorize())
}
```

A closure `Fn(&Request) -> impl Future<Output = Result<Option<User>>>` works as an authenticator too:

```rust
use zapline::auth::{Authenticate, CurrentUser};
use zapline::prelude::*;

fn api() -> Routes {
    let auth = Authenticate::new(|req: &Request| {
        let user = req.headers().get("x-user").and_then(|v| v.to_str().ok()).map(str::to_owned);
        async move { Ok(user) }
    })
    .required();

    Routes::new()
        .get("/me", |CurrentUser(name): CurrentUser<String>| async move { name })
        .wrap(auth)
}
```

### `Authenticate`

`Authenticate` is route middleware (install it with `Routes::wrap` or `App::wrap_route`). On each request:

- **No credentials:** the request passes through anonymously, unless `.required()` is set or the route has a policy. Then it is rejected with `401 missing_credentials` and the authenticator's challenge.
- **Invalid credentials:** rejected with the authenticator's error, even when authentication is optional. The exception is a route marked `.allow_anonymous()`, where the error is ignored.
- **Valid credentials:** the user is stored and `CurrentUser<U>` works in the handler. With `.authorize()`, the route's role and scope policies are checked too (`403 forbidden`).

| Method | Effect |
|---|---|
| `Authenticate::new(authenticator)` | Anonymous requests are allowed on routes without a policy |
| `.required()` / `.optional()` | Reject unauthenticated requests on every route this wraps (except `.allow_anonymous()`), or allow them again |
| `.authorize()` | Also enforce `.require_roles(..)` and `.require_scopes(..)`. The user type must implement `HasRoles`. |
| `.challenge(value)` | The `WWW-Authenticate` value for `401`s (the authenticator can provide a default through `Authenticator::challenge`) |
| `Authenticate::jwt()` / `jwt_with(config)` | `JwtAuthenticator<Claims>`, `.required()` and `.authorize()` in one call |

`CurrentUser<U>` derefs to `U`. It rejects with `401 missing_credentials` when there is no user. Use `Option<CurrentUser<U>>` where anonymous access is fine. `U` is cloned on every extraction, so wrap big users in an `Arc` (`Arc<T>` implements `HasRoles` when `T` does).

## Policies

Declare requirements on the route they protect. Builder methods apply to the most recently registered route, on `Routes` and on `App`:

| Method | Requirement |
|---|---|
| `.require_auth()` | Any authenticated user |
| `.require_roles(["admin", "ops"])` | The user needs **at least one** of these roles |
| `.require_scopes(["users:read", "users:write"])` | The user needs **all** of these scopes |
| `.allow_anonymous()` | The route is public. Group requirements and every role/scope policy are skipped. Valid credentials still populate `CurrentUser`. |
| `.require_auth_all()`, `.require_roles_all([..])`, `.require_scopes_all([..])` | The same, for **every route in a group** (on `Routes`) |

```rust
use zapline::auth::{Authenticate, Claims, CurrentUser};
use zapline::prelude::*;

async fn me(CurrentUser(c): CurrentUser<Claims>) -> String {
    c.sub
}

fn api() -> Routes {
    Routes::new()
        .get("/me", me)
        .get("/reports", || async { "reports" })
        .require_scopes(["reports:read"])
        .delete("/users/{id}", || async { StatusCode::NO_CONTENT })
        .require_roles(["admin"])
        .get("/status", || async { "up" })
        .allow_anonymous()
        .wrap(Authenticate::jwt())
}

fn admin_area() -> Routes {
    Routes::new()
        .get("/stats", || async { "stats" })
        .get("/audit", || async { "audit" })
        .require_roles(["auditor"])        // replaces the group's roles for this route
        .require_roles_all(["admin"])
        .wrap(Authenticate::jwt())
}
```

Rules:

- For each kind of requirement, the most specific declaration wins. A route's own `.require_roles(..)` **replaces** its group's `.require_roles_all(..)`, while a group's role requirement and a route's scope requirement **both apply**.
- Calling `.require_roles(..)` or `.require_scopes(..)` repeatedly on one route adds to the list.
- Failing a check gives `403` with the code `forbidden`. A missing user gives `401 missing_credentials`.
- Policies are plain route metadata, so tooling can read them: `RoutePolicy::of(route)` has `.requires_auth()`, `.roles()`, `.scopes()`, `.is_anonymous()` and `.allows(&user)`.
- Policies don't change the OpenAPI document. Add `security = "bearer"` or `Routes::security_all("bearer")` separately; see [OpenAPI & interactive docs](openapi.md).

## Policies fail closed

A declarative policy is only as good as the middleware that enforces it. A common mistake in other frameworks is to declare `@RolesAllowed("admin")` and forget to install the filter, which silently makes the endpoint public. zapline rules that out: **every `.require_*()` call also attaches an innermost enforcement layer to the route**. It runs right before the handler, after all group and route middleware, and it checks that something has actually verified the policy:

| Situation | Response |
|---|---|
| A policy, but no authentication middleware ran | `401 missing_credentials` |
| A user was authenticated, the policy has only `.require_auth()` | Allowed |
| A user was authenticated, the policy has roles or scopes, but **no middleware checked them** | `500 auth_misconfigured` |
| The user lacks the role or scope (checked by `Authenticate::authorize()` or `Guard`) | `403 forbidden` |
| The route is `.allow_anonymous()` | Policy skipped |

The `500` is a loud signal, not a security hole. The request never reaches your handler. The response body names the route and the missing piece (`call Authenticate::authorize()` ...), so it shows up in the first test you write.

### Why a global `App::wrap(Authenticate..)` cannot check roles

Global middleware (`App::wrap`) runs **before routing**, so while it runs there is no matched route yet and therefore no policy to check against. A global `Authenticate` can still identify the user (`CurrentUser` works, and `.require_auth()` is satisfied), but a route with roles or scopes behind it answers `500 auth_misconfigured`.

Check roles in middleware that runs **after** routing. All of these work:

```rust
use zapline::auth::{Authenticate, Claims, Guard, JwtConfig};
use zapline::prelude::*;

fn secret() -> JwtConfig {
    JwtConfig::hs256("an-example-secret-of-at-least-32-bytes")
}

// 1. A group: authentication and authorization for exactly these routes.
fn grouped() -> App {
    let admin = Routes::new()
        .get("/stats", || async { "stats" })
        .require_roles(["admin"])
        .wrap(Authenticate::jwt());
    App::new().jwt(secret()).nest("/admin", admin)
}

// 2. Route middleware for the whole app (runs after routing for every matched route).
fn route_wide() -> App {
    App::new()
        .jwt(secret())
        .wrap_route(Authenticate::jwt().optional())
        .get("/stats", || async { "stats" })
        .require_roles(["admin"])
}

// 3. Identify globally, enforce after routing.
fn split() -> App {
    App::new()
        .jwt(secret())
        .wrap(Authenticate::jwt().optional())
        .wrap_route(Guard::<Claims>::policies())
        .get("/stats", || async { "stats" })
        .require_roles(["admin"])
}
```

Prefer a group when only part of the app is protected. `App::wrap_route` also wraps the routes that plugins add (docs, `/health`, `/metrics`). That is why the examples above use `.optional()` there: a `.required()` authenticator would lock your health checks. Anything that must stay public can be marked `.allow_anonymous()` on your own routes.

## `Guard`

`Guard<U>` is the other half of authorization: route middleware that checks the `CurrentUser<U>` set by an earlier `Authenticate`. It must run after routing and after authentication (`.wrap(auth).wrap(guard)`, or `App::wrap_route` together with a global `Authenticate`). Used before routing, it answers `500 auth_misconfigured`.

| Constructor | Rule |
|---|---|
| `Guard::policies()` | Enforces each route's own policies, for an `Authenticate` that doesn't call `.authorize()`. Routes without a policy pass. `U: HasRoles`. |
| `Guard::roles([..])` | At least one of these roles on every route it wraps. `U: HasRoles`. |
| `Guard::scopes([..])` | All of these scopes on every route it wraps. `U: HasRoles`. |
| `Guard::new(\|user, route\| bool)` | Your own rule. It receives the user and the matched `RouteInfo`, including its metadata. |

Routes marked `.allow_anonymous()` are skipped. A request without a user gets `401`, a failed rule `403`.

```rust
use zapline::auth::{Authenticate, Guard, HasRoles};
use zapline::prelude::*;
use zapline::RouteInfo;

#[derive(Clone)]
struct User {
    id: u64,
    roles: Vec<String>,
}

impl HasRoles for User {
    fn roles(&self) -> &[String] {
        &self.roles
    }
}

fn api() -> Routes {
    let auth = Authenticate::new(|_: &Request| async { Ok(Some(User { id: 1, roles: vec![] })) });
    // Custom rule: only user 1 may call DELETE routes.
    let guard = Guard::new(|user: &User, route: &RouteInfo| {
        route.method() != Some(&Method::DELETE) || user.id == 1
    });
    Routes::new()
        .delete("/things/{id}", || async { StatusCode::NO_CONTENT })
        .wrap(auth)
        .wrap(guard)
}
```

## `HasRoles`

Roles and scopes are plain strings. Any user type becomes usable with policies by implementing `roles()` and, optionally, `scopes()`:

```rust
use zapline::auth::HasRoles;

#[derive(Clone)]
struct User {
    roles: Vec<String>,
    scopes: Vec<String>,
}

impl HasRoles for User {
    fn roles(&self) -> &[String] {
        &self.roles
    }
    fn scopes(&self) -> &[String] {
        &self.scopes
    }
}
```

`has_role(name)` and `has_scope(name)` come for free.

## Testing authorization

Send tokens with the test client's `.bearer(..)` and assert on the status. Test the unhappy paths as well as the happy ones:

```rust
use zapline::auth::{Authenticate, Claims, JwtConfig};
use zapline::prelude::*;

fn app() -> App {
    let admin = Routes::new()
        .get("/stats", || async { "stats" })
        .require_roles(["admin"])
        .wrap(Authenticate::jwt());
    App::new()
        .jwt(JwtConfig::hs256("an-example-secret-of-at-least-32-bytes"))
        .nest("/admin", admin)
}

#[tokio::test]
async fn only_admins_see_stats() {
    let client = app().test();
    let jwt = JwtConfig::hs256("an-example-secret-of-at-least-32-bytes");
    let admin = jwt.encode(&Claims::new("ada").roles(["admin"])).unwrap();
    let user = jwt.encode(&Claims::new("bob").roles(["user"])).unwrap();

    assert_eq!(client.get("/admin/stats").send().await.status(), 401);
    assert_eq!(client.get("/admin/stats").bearer(&user).send().await.status(), 403);
    assert_eq!(client.get("/admin/stats").bearer(&admin).send().await.status(), 200);
}
```
