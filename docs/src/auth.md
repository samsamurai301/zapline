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
| `migrations/<ts>_create_users.sql` | `users` table (unique lowercased email, Argon2id password hash), `password_resets` (hashed single-use tokens) and `oauth_accounts` (linked GitHub/Google identities) |
| `src/routes/auth.rs` | `POST /api/auth/signup`, `/login`, `/logout`, `/forgot`, `/reset`, `GET /api/auth/me`; the `CurrentUser` extractor; sessions stored in the database; the [admin panel](admin.md) opened to the accounts in `ADMIN_EMAILS`, once each owner has proved the address |
| `templates/auth/*.html` | htmx pages at `/signup`, `/login`, `/forgot` and `/reset` (when `templates/base.html` exists): rejected posts come back as a 422 with a message next to each field and the email kept (passwords are never echoed); success redirects with a flash message ([Forms](views.md#forms-errors-next-to-each-field)) |
| `tests/auth.rs` | Sign-up, log-in, log-out, wrong passwords, duplicates, weak input, password reset, the HTML forms, GitHub login against a stand-in provider, who gets into `/admin` |

It also enables zapline's `cookies`, `mail`, `oauth` and `password` features, adds `SESSION_SECRET`, `SMTP_URL`, `MAIL_FROM`, `APP_URL`, `ADMIN_EMAILS` and the GitHub/Google client variables to `.env.example`, and compiles Argon2 optimized in debug builds, so tests stay fast.

**Password reset.** `POST /api/auth/forgot` (or the `/forgot` page) emails a link to `APP_URL/reset?token=...` and answers the same whether or not the account exists; the email goes out after the response, so timing doesn't tell either. The token is 256 random bits, stored only as a SHA-256 digest, valid for an hour and usable once; using it sets the new password, logs the user in and voids the account's other reset links. While developing, without `SMTP_URL`, the emails appear in the [dev inbox](mail.md#the-dev-inbox) at `/_dev/mail`, so the whole flow works on your machine.

**Log in with GitHub or Google.** Set `GITHUB_CLIENT_ID` and `GITHUB_CLIENT_SECRET` (or the `GOOGLE_` pair) and the login and sign-up pages show "Continue with GitHub"; providers without credentials are simply left out. Register the callback `APP_URL/auth/github/callback` (or `/auth/google/callback`) with the provider. The first social login links the identity to the account with the same verified email, or creates an account without a usable password ("Forgot your password?" sets one); later logins use the link. Sign-up doesn't check the address, so someone could open an account with another person's email ahead of them: when the real owner then logs in with GitHub or Google, that account's password is dropped and its sessions end.

**Admins.** The accounts in `ADMIN_EMAILS` get into `/admin` only after the address is proved: by using a reset link sent to it, or by a GitHub/Google login with it. Until then they get `403 email_unverified`, so signing up first with the boss's address gets an attacker nowhere. See [Social login](#social-login-oauth) for the pieces underneath.

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

What it does for you: passwords hashed with Argon2id on the blocking thread pool; a fresh session id at login (no session fixation); changing the password logs out every other session (a `session_version` column); sign-up, log-in and reset requests limited to 10 a minute per client IP (set `TRUSTED_PROXIES` behind a proxy); requests from other sites refused ([`SameOrigin`](sessions.md#sameorigin-no-tokens), CSRF protection with nothing to add to forms); the same error and the same timing for an unknown email and a wrong password (`password::verify_missing`); sessions in the database, so logins survive restarts and deploys; `HttpOnly`, `SameSite=Lax` cookies, `Secure` in release builds. In production set `SESSION_SECRET` to 32 or more random bytes (`openssl rand -base64 48`): release builds refuse to start without it.

## Passwords

With the `password` feature, `zapline::auth::password` hashes and checks passwords with Argon2id (OWASP parameters, PHC strings that carry their salt and parameters):

```rust
use zapline::auth::password;

let hash = password::hash("correct horse battery staple").await?;
assert!(password::verify("correct horse battery staple", &hash).await?);
```

Both run on the blocking thread pool, since a hash takes tens of milliseconds by design. `password::verify_missing(pw)` spends the same time and returns `false`: call it when the account doesn't exist. `hash_blocking` and `verify_blocking` are the synchronous versions.

## Social login (OAuth)

With the `oauth` feature, `App::oauth` adds "log in with GitHub/Google/..." using the authorization-code flow with PKCE. Register a provider, then handle its callback with the `OAuthLogin` extractor:

```rust
use zapline::auth::oauth::{OAuth, OAuthLogin};

#[get("/auth/github/callback")]
async fn github_done(login: OAuthLogin, session: Session) -> Result<Redirect> {
    // login.user: provider, id, email, email_verified, name, avatar_url, raw
    let user_id = find_or_create_user(&login.user).await?;
    session.regenerate();
    session.insert("user_id", user_id)?;
    Ok(Redirect::to(login.next.as_deref().unwrap_or("/")))
}

let app = App::new()
    .oauth(OAuth::from_env("github")?) // GITHUB_CLIENT_ID, GITHUB_CLIENT_SECRET
    .mount(github_done);
```

Send people to `/auth/github` (optionally `?next=/settings`) to start. That route redirects to the provider with a random `state` and a PKCE challenge, kept in a short-lived `HttpOnly` cookie scoped to `/auth`. On the way back, `OAuthLogin` checks the state, exchanges the code (sending the PKCE verifier) and fetches the profile. It answers `400 invalid_oauth_state` for a forged or replayed callback, `401 oauth_denied` when the user cancelled at the provider, `401 oauth_failed` when the provider rejects the code, and `404` for a provider that isn't registered. `next` is kept only when it is a local path, so it can't be used as an open redirect.

| Provider | Constructor | Notes |
|---|---|---|
| GitHub | `OAuth::github(id, secret)` | Scope `read:user user:email`; the email is the verified primary one from `/user/emails` |
| Google | `OAuth::google(id, secret)` | OpenID Connect `userinfo`; scopes `openid email profile` |
| Any OAuth 2 provider | `OAuth::custom(name, authorize_url, token_url, userinfo_url)` | Reads `sub`/`id`, `email`, `email_verified`, `name`, `picture`/`avatar_url`; the whole profile is in `user.raw` |

`.scopes([...])` replaces the scopes, `.redirect_url(url)` overrides the callback URL (by default `APP_URL` + `/auth/{name}/callback`, or the request's host when `APP_URL` is unset), and `.endpoints(..)` points a built-in provider at a test double. Only trust `user.email` for account linking when `user.email_verified` is true.

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
| `JwtConfig::from_key(jwt_key)` | Starts from a named [`JwtKey`](#key-rotation), which also covers EdDSA (Ed25519) |
| `JwtConfig::from_jwks(Jwks::new(url))` | Verify-only, with the keys an identity provider publishes ([JWKS](#tokens-from-an-identity-provider-jwks), feature `jwks`) |

| Builder method | Effect |
|---|---|
| `.issuer(iss)` | Tokens must carry this `iss`. Issued tokens get it. |
| `.audience(aud)` | An accepted audience (call again to accept several). Tokens must carry a matching `aud`. Issued tokens get the first one. |
| `.ttl(duration)` | Lifetime of issued tokens when the claims have no `exp` (default 1 hour) |
| `.leeway(duration)` | Clock-skew tolerance for `exp` and `nbf` (default 60 s) |
| `.required_claims(names)` | Claims that must be present, from `exp`, `nbf`, `iss`, `aud`, `sub` (default: `exp`) |
| `.key_id(kid)` | Names the config's key: issued tokens carry this `kid`, and tokens naming it verify with it ([key rotation](#key-rotation)) |
| `.jwks(Jwks::new(url))` | Also accept tokens signed with a remote JWK Set's keys (feature `jwks`) |
| `.validation(\|v\| ..)` | Adjust the underlying `jsonwebtoken::Validation` |

The configured algorithm is pinned, so a token cannot choose its own (no `alg: none`, no HS/RS confusion). With several keys, each key keeps its own algorithm: a token naming an RS256 key with `alg: HS256` is rejected. `exp` is required and validated, `nbf` is validated when present, and tokens longer than 16 KiB are rejected before parsing. `encode` fills in a missing `iat`, `exp` (now + ttl), `iss` and `aud`.

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

### Key rotation

A `JwtConfig` holds a key ring: the key that signs, and every key that still verifies. Tokens name their key in the `kid` header, and the ring picks the key by that name. A `JwtKey` is one entry: a `kid`, an algorithm and the key material.

| `JwtKey` constructor | Algorithm |
|---|---|
| `JwtKey::hs256(kid, secret)` | HMAC-SHA256 (errors if the secret is shorter than 32 bytes) |
| `JwtKey::rs256_pem(kid, public_pem, Some(private_pem))` | RSA |
| `JwtKey::es256_pem(kid, public_pem, Some(private_pem))` | ECDSA P-256 |
| `JwtKey::ed25519_pem(kid, public_pem, Some(private_pem))` | EdDSA (Ed25519) |
| `JwtKey::from_jwk(&jwk)` | A public JWK (RSA, EC P-256/P-384, Ed25519). Symmetric and encryption keys are refused. |
| `JwtKey::new(kid, algorithm, decoding_key, encoding_key)` | Full control |

Pass `None` for the private key to get a key that only verifies. Three methods change the ring while the app runs. Every clone of the config shares the ring, so calling them on the `State<JwtConfig>` a handler extracted takes effect for the whole app:

| Method | Effect |
|---|---|
| `jwt.rotate(key)` | Sign with `key` from now on. The previous signing key stays and keeps verifying the tokens it issued. Errors if `key` has no private part or no `kid`. |
| `jwt.add_key(key)` | Add a key that only verifies: an old key kept after a restart, or the next key published before you start signing with it |
| `jwt.retire(kid)` | Stop accepting a key's tokens. The current signing key can't be retired. |
| `jwt.key_ids()` | The `kid`s in the ring, the signing key's first |

```rust
use zapline::auth::{Claims, JwtConfig, JwtKey};
use zapline::prelude::*;

fn jwt() -> Result<JwtConfig> {
    let current = std::env::var("JWT_SECRET")?;
    let jwt = JwtConfig::hs256(current).key_id("2026-10");
    // Tokens signed with last month's key keep working until they expire.
    if let Ok(previous) = std::env::var("JWT_SECRET_PREVIOUS") {
        jwt.add_key(JwtKey::hs256("2026-09", previous)?);
    }
    Ok(jwt)
}

// Rotate without a restart, for example from an admin endpoint.
async fn rotate(State(jwt): State<JwtConfig>) -> Result<&'static str> {
    let next = std::env::var("JWT_SECRET_NEXT")?;
    jwt.rotate(JwtKey::hs256("2026-11", next)?)?;
    Ok("rotated")
}
```

How a key is picked:

- If the token's `kid` names a key in the ring, only that key is tried. The key's algorithm must match the token's `alg`.
- Keys without a `kid` (a config built without `.key_id(..)`) accept any `kid` with their algorithm, so configs that predate key ids keep working.
- A token without `kid` verifies only when the choice is unambiguous: a key without a `kid`, or a ring with a single key.
- Otherwise the token is `401 invalid_token`, or it is looked up in the remote JWK Set when one is configured.

A config with a single unnamed key (the usual `JwtConfig::hs256(secret)`) verifies without reading the token header, as before. Rotations live in memory. When several instances sign tokens, give each instance the same keys at startup with `add_key`, or let one service issue tokens and have the others follow its JWK Set.

### Publishing your keys

`App::serve_jwks(path)` serves the public half of every RSA, EC and Ed25519 key in the registered config as a JWK Set ([RFC 7517](https://www.rfc-editor.org/rfc/rfc7517)). HMAC keys are never published. The set is read on every request, so a rotation shows up at once. Responses carry `Cache-Control: public, max-age=300`, and the route is left out of the OpenAPI document.

```rust
use zapline::auth::{JwtConfig, JwtKey};
use zapline::prelude::*;

fn app() -> Result<App> {
    let public = std::fs::read("keys/2026-10.pub.pem")?;
    let private = std::fs::read("keys/2026-10.pem")?;
    let key = JwtKey::es256_pem("2026-10", &public, Some(&private))?;
    Ok(App::new()
        .jwt(JwtConfig::from_key(key).issuer("https://auth.example.com"))
        .serve_jwks("/.well-known/jwks.json"))
}
```

`jwt.public_jwks()` returns the same set (a `JwkSet`) if you serve it some other way.

To rotate without failing any request, publish the next key first with `jwt.add_key(next_public_key)`. Wait at least as long as verifiers cache the set, then switch with `jwt.rotate(next_key)`. Zapline verifiers don't need the wait: they refetch the set when a token names a key they haven't seen.

### Tokens from an identity provider (JWKS)

With the `jwks` feature, a config can verify tokens signed by keys that an identity provider publishes at its `jwks_uri`: Auth0, Okta, Keycloak, Cognito, Entra ID, Google, or another zapline app's `serve_jwks`.

```rust
use std::time::Duration;
use zapline::auth::{Authenticate, Jwks, JwtConfig};
use zapline::prelude::*;

fn app() -> App {
    let issuer = "https://example.eu.auth0.com/";
    let jwt = JwtConfig::from_jwks(Jwks::discover(issuer))
        .issuer(issuer)
        .audience("https://api.example.com");
    App::new()
        .jwt(jwt)
        .nest("/api", Routes::new().get("/me", || async { "hi" }).wrap(Authenticate::jwt()))
}
```

`Jwks::new(url)` takes the JWK Set URL directly. `Jwks::discover(issuer)` reads `jwks_uri` from `{issuer}/.well-known/openid-configuration` (OpenID Connect Discovery) and checks that the document names the same issuer. `JwtConfig::hs256(secret).jwks(..)` combines your own keys with the provider's: local `kid`s are checked first and never cause a fetch.

How the cache behaves:

- **Startup.** `App::jwt` fetches the set before the server starts accepting requests. This is best effort: if the provider is down, the failure is logged and the first request tries again.
- **TTL.** Keys are fresh for `.ttl(..)` (default 10 minutes). After that, the next request still verifies with the cached keys while a background task refreshes them. No request waits for a refresh of keys it already has.
- **Unknown `kid`.** A token naming a key the cache doesn't have triggers a refresh, so a provider's rotation is picked up at once. Refreshes happen at most once per `.min_refresh_interval(..)` (default 30 s), however many made-up `kid`s arrive. Concurrent requests share one fetch.
- **Timeouts.** A request waits at most `.timeout(..)` (default 5 s) for a fetch.
- **Failures.** On an error status, malformed JSON, a set with no usable keys, or a timeout, the cached keys are kept and keep verifying (stale on error). A token whose key the cache can't have while the provider is failing gets `503 jwks_unavailable`, not a misleading `401`.

The algorithm comes from the JWK's `alg`, or from its key type when `alg` is absent: RS256 for RSA, ES256/ES384 for P-256/P-384, EdDSA for Ed25519. Keys whose `alg` doesn't fit their key type, symmetric (`oct`) keys, encryption keys (`"use": "enc"`) and unknown key types are skipped. `.algorithms([Algorithm::RS256])` narrows the set further. HMAC algorithms are never accepted from a remote set, so a public key can't be turned into an HMAC secret.

| `Jwks` method | Effect |
|---|---|
| `Jwks::new(url)` / `Jwks::discover(issuer)` | Where the keys come from |
| `.ttl(d)` | How long fetched keys are fresh (default 10 min) |
| `.min_refresh_interval(d)` | Fetch at most this often (default 30 s) |
| `.timeout(d)` | The longest a request waits for a fetch (default 5 s) |
| `.algorithms([..])` | Accept only keys for these algorithms |
| `jwks.refresh().await` | Fetch now. Returns the number of usable keys. |
| `jwks.key_ids()` | The cached `kid`s |

`jwt.decode(token)` is synchronous and uses only cached keys. A key it is missing is fetched in the background. The `Jwt` extractor, `Authenticate::jwt()` and `jwt.verify(token).await` wait for the fetch.

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
