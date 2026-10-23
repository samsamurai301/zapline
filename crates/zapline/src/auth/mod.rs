//! Authentication and authorization.
//!
//! - **Credential extractors** (always on): [`BearerToken`], [`BasicAuth`],
//!   [`ApiKey`]. Missing or malformed credentials are rejected with `401`, a
//!   `WWW-Authenticate` challenge and the codes `missing_credentials` /
//!   `invalid_credentials`.
//! - **JWT** (feature `jwt`): [`JwtConfig`], [`Claims`], the [`Jwt`]
//!   extractor and [`JwtAuthenticator`].
//! - **Pluggable authentication**: implement [`Authenticator`] (or pass a
//!   closure) and install [`Authenticate`] as route middleware; handlers read
//!   the user with [`CurrentUser`].
//! - **Authorization**: per-route policies (`.require_auth()`,
//!   `.require_roles([..])`, `.require_scopes([..])`, `.allow_anonymous()`)
//!   enforced by [`Authenticate`] and [`Guard`]; `403` with code `forbidden`.
//! - **CSRF** (feature `cookies`): the double-submit [`Csrf`] middleware and
//!   the [`CsrfToken`] extractor.
//! - **Passwords** (feature `password`): Argon2id hashing in [`password`].
//!
//! ```no_run
//! # #[cfg(feature = "jwt")] {
//! use zapline::prelude::*;
//! use zapline::auth::{Authenticate, Claims, CurrentUser, JwtConfig};
//!
//! async fn me(CurrentUser(user): CurrentUser<Claims>) -> String {
//!     format!("hello {}", user.sub)
//! }
//! async fn purge() -> &'static str { "purged" }
//!
//! let api = Routes::new()
//!     .get("/me", me)
//!     .delete("/cache", purge).require_roles(["admin"]);
//!
//! App::new()
//!     .jwt(JwtConfig::hs256("an-example-secret-of-at-least-32-bytes"))
//!     .nest("/api", api.wrap(Authenticate::jwt()))
//!     .run("0.0.0.0:8080")
//!     .unwrap();
//! # }
//! ```

use http::header::WWW_AUTHENTICATE;
use http::{HeaderValue, StatusCode};

use crate::Error;

mod api_key;
mod authenticate;
mod basic;
mod bearer;
#[cfg(feature = "cookies")]
mod csrf;
mod guard;
#[cfg(feature = "jwt")]
pub mod jwt;
#[cfg(feature = "oauth")]
pub mod oauth;
#[cfg(feature = "password")]
pub mod password;

pub use api_key::{ApiKey, ApiKeyConfig};
pub use authenticate::{Authenticate, Authenticator, CurrentUser};
pub use basic::{BasicAuth, BasicAuthConfig};
pub use bearer::BearerToken;
#[cfg(feature = "cookies")]
pub(crate) use csrf::CsrfBinding;
#[cfg(feature = "cookies")]
pub use csrf::{Csrf, CsrfToken};
pub use guard::{
    AllowAnonymous, Guard, HasRoles, RequireAuth, RequireRoles, RequireScopes, RoutePolicy,
};
#[cfg(feature = "jwt")]
pub use jwt::{Algorithm, Claims, DecodingKey, EncodingKey, Jwt, JwtAuthenticator, JwtConfig};

/// Compare two byte strings in constant time (with respect to their
/// contents), so secrets such as API keys and tokens cannot be recovered
/// through timing. Inputs of different length compare unequal immediately;
/// only the length can leak.
///
/// ```
/// use zapline::auth::constant_time_eq;
/// assert!(constant_time_eq("s3cret", "s3cret"));
/// assert!(!constant_time_eq("s3cret", "s3creT"));
/// ```
pub fn constant_time_eq(a: impl AsRef<[u8]>, b: impl AsRef<[u8]>) -> bool {
    let (a, b) = (a.as_ref(), b.as_ref());
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// Stable error codes used by this module.
pub(crate) mod codes {
    pub const MISSING: &str = "missing_credentials";
    pub const INVALID: &str = "invalid_credentials";
    pub const FORBIDDEN: &str = "forbidden";
}

/// A `401` with a `WWW-Authenticate` challenge.
pub(crate) fn unauthorized(
    code: &'static str,
    message: &'static str,
    challenge: HeaderValue,
) -> Error {
    Error::new(StatusCode::UNAUTHORIZED, message)
        .with_code(code)
        .with_header(WWW_AUTHENTICATE, challenge)
}

/// `401 missing_credentials`, optionally with a challenge.
pub(crate) fn missing_credentials(challenge: Option<HeaderValue>) -> Error {
    let err = Error::new(StatusCode::UNAUTHORIZED, "missing credentials").with_code(codes::MISSING);
    match challenge {
        Some(c) => err.with_header(WWW_AUTHENTICATE, c),
        None => err,
    }
}

/// Request marker: the challenge of the `Authenticate` middleware that ran,
/// so later 401s (guards, policies, `CurrentUser`) can send the same one.
#[derive(Clone, Debug)]
pub(crate) struct AuthChallenge(pub(crate) HeaderValue);

/// The `WWW-Authenticate` challenge for a 401 on `req`: the one set by
/// `Authenticate`, else `Bearer` (a 401 must always carry a challenge).
pub(crate) fn challenge_for(req: &crate::Request) -> HeaderValue {
    req.extensions()
        .get::<AuthChallenge>()
        .map_or_else(|| HeaderValue::from_static("Bearer"), |c| c.0.clone())
}

/// `403 forbidden`.
pub(crate) fn forbidden() -> Error {
    Error::forbidden().with_code(codes::FORBIDDEN)
}

/// The value of the single `Authorization` header. More than one header is
/// ambiguous and treated as malformed (`Err`).
pub(crate) fn authorization(headers: &http::HeaderMap) -> Result<Option<&str>, ()> {
    let mut values = headers.get_all(http::header::AUTHORIZATION).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    first.to_str().map(Some).map_err(|_| ())
}

/// Split `"<scheme> <credentials>"`, matching the scheme case-insensitively.
pub(crate) fn strip_scheme<'a>(value: &'a str, scheme: &str) -> Option<&'a str> {
    let (s, rest) = value.split_once(' ')?;
    s.eq_ignore_ascii_case(scheme)
        .then(|| rest.trim_matches(' '))
}
