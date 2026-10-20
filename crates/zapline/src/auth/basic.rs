//! `Authorization: Basic <base64(username:password)>`.

use std::fmt;

use base64::prelude::{Engine as _, BASE64_STANDARD};
use http::HeaderValue;

use super::{authorization, codes, constant_time_eq, missing_credentials, strip_scheme};
use crate::extract::FromRequestParts;
use crate::{Error, Request};

/// Longest encoded credentials accepted, in bytes.
const MAX_ENCODED_LEN: usize = 4 * 1024;

/// HTTP Basic credentials (RFC 7617).
///
/// Rejects with `401` and `WWW-Authenticate: Basic realm="..."`
/// (configure the realm with [`BasicAuthConfig`]): code `missing_credentials`
/// when absent, `invalid_credentials` when malformed. Check the password with
/// [`verify`](Self::verify), which compares in constant time.
///
/// ```
/// use zapline::auth::BasicAuth;
/// use zapline::prelude::*;
///
/// async fn metrics(auth: BasicAuth) -> Result<&'static str> {
///     if !auth.verify("ops", "correct horse battery staple") {
///         return Err(auth.reject());
///     }
///     Ok("# metrics")
/// }
/// # let _ = App::new().get("/metrics", metrics);
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct BasicAuth {
    /// The user name (never contains `:`).
    pub username: String,
    /// The password.
    pub password: String,
}

impl BasicAuth {
    /// `true` if both username and password match. Both comparisons always
    /// run, in constant time.
    pub fn verify(&self, username: impl AsRef<[u8]>, password: impl AsRef<[u8]>) -> bool {
        let user_ok = constant_time_eq(&self.username, username);
        let pass_ok = constant_time_eq(&self.password, password);
        user_ok & pass_ok
    }

    /// A `401 invalid_credentials` with the default Basic challenge, for
    /// rejecting wrong credentials from a handler.
    pub fn reject(&self) -> Error {
        invalid(&BasicAuthConfig::default())
    }
}

impl fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BasicAuth")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Configuration for [`BasicAuth`]: the realm sent in the challenge.
/// Register with `App::state(BasicAuthConfig::new("admin area"))`; the
/// default realm is `"Restricted"`.
#[derive(Clone, Debug)]
pub struct BasicAuthConfig {
    challenge: HeaderValue,
}

impl BasicAuthConfig {
    /// # Panics
    /// If `realm` contains characters not allowed in a header value or `"`.
    pub fn new(realm: &str) -> Self {
        assert!(
            !realm.contains(['"', '\\']),
            "invalid Basic realm: {realm:?}"
        );
        let challenge =
            HeaderValue::try_from(format!("Basic realm=\"{realm}\", charset=\"UTF-8\""))
                .unwrap_or_else(|_| panic!("invalid Basic realm: {realm:?}"));
        BasicAuthConfig { challenge }
    }

    /// The `WWW-Authenticate` value.
    pub fn challenge(&self) -> &HeaderValue {
        &self.challenge
    }
}

impl Default for BasicAuthConfig {
    fn default() -> Self {
        BasicAuthConfig {
            challenge: HeaderValue::from_static("Basic realm=\"Restricted\", charset=\"UTF-8\""),
        }
    }
}

fn invalid(cfg: &BasicAuthConfig) -> Error {
    super::unauthorized(codes::INVALID, "invalid credentials", cfg.challenge.clone())
}

impl FromRequestParts for BasicAuth {
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        let default = BasicAuthConfig::default();
        let cfg = req.state::<BasicAuthConfig>().unwrap_or(&default);
        let value = match authorization(req.headers()) {
            Ok(Some(v)) => v,
            Ok(None) => return Err(missing_credentials(Some(cfg.challenge.clone()))),
            Err(()) => return Err(invalid(cfg)),
        };
        let Some(encoded) = strip_scheme(value, "basic") else {
            return Err(missing_credentials(Some(cfg.challenge.clone())));
        };
        decode(encoded).ok_or_else(|| invalid(cfg))
    }
}

fn decode(encoded: &str) -> Option<BasicAuth> {
    if encoded.is_empty() || encoded.len() > MAX_ENCODED_LEN {
        return None;
    }
    let raw = BASE64_STANDARD.decode(encoded).ok()?;
    let raw = String::from_utf8(raw).ok()?;
    let (username, password) = raw.split_once(':')?;
    if username.chars().any(char::is_control) || password.chars().any(char::is_control) {
        return None;
    }
    Some(BasicAuth {
        username: username.to_owned(),
        password: password.to_owned(),
    })
}
