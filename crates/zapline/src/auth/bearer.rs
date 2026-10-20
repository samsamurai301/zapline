//! `Authorization: Bearer <token>`.

use std::fmt;

use http::HeaderValue;

use super::{authorization, codes, missing_credentials, strip_scheme, unauthorized};
use crate::extract::FromRequestParts;
use crate::{Error, Request};

/// Longest bearer token accepted, in bytes.
const MAX_TOKEN_LEN: usize = 16 * 1024;

const CHALLENGE: HeaderValue = HeaderValue::from_static("Bearer");
const CHALLENGE_INVALID: HeaderValue = HeaderValue::from_static("Bearer error=\"invalid_request\"");

/// The token from an `Authorization: Bearer <token>` header (scheme matched
/// case-insensitively).
///
/// Rejects with `401` and `WWW-Authenticate: Bearer`: code
/// `missing_credentials` when there is no bearer token, and
/// `invalid_credentials` when it is malformed. Use `Option<BearerToken>` for
/// optional authentication.
///
/// ```
/// use zapline::auth::{constant_time_eq, BearerToken};
/// use zapline::prelude::*;
///
/// async fn admin(token: BearerToken) -> Result<&'static str> {
///     if !constant_time_eq(token.token(), "let-me-in") {
///         return Err(Error::forbidden());
///     }
///     Ok("welcome")
/// }
/// # let _ = App::new().get("/admin", admin);
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    /// The raw token.
    pub fn token(&self) -> &str {
        &self.0
    }

    /// The raw token (alias of [`token`](Self::token)).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The raw token, owned.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(<redacted>)")
    }
}

impl AsRef<str> for BearerToken {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromRequestParts for BearerToken {
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        match bearer(req) {
            Ok(Some(token)) => Ok(BearerToken(token.to_owned())),
            Ok(None) => Err(missing_credentials(Some(CHALLENGE))),
            Err(e) => Err(e),
        }
    }
}

/// The bearer token of `req`: `Ok(None)` when there is no `Authorization`
/// header or it uses another scheme, `Err` when it is malformed.
pub(crate) fn bearer(req: &Request) -> Result<Option<&str>, Error> {
    let invalid = || unauthorized(codes::INVALID, "malformed bearer token", CHALLENGE_INVALID);
    let value = match authorization(req.headers()) {
        Ok(Some(v)) => v,
        Ok(None) => return Ok(None),
        Err(()) => return Err(invalid()),
    };
    if value.eq_ignore_ascii_case("bearer") {
        return Err(invalid());
    }
    let Some(token) = strip_scheme(value, "bearer") else {
        return Ok(None);
    };
    if is_token68(token) {
        Ok(Some(token))
    } else {
        Err(invalid())
    }
}

/// RFC 7235 `token68`: `1*( ALPHA / DIGIT / "-" / "." / "_" / "~" / "+" / "/" ) *"="`.
fn is_token68(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_TOKEN_LEN {
        return false;
    }
    let body = s.trim_end_matches('=');
    !body.is_empty()
        && body.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
}

#[cfg(test)]
mod tests {
    use super::is_token68;

    #[test]
    fn token68() {
        assert!(is_token68("abc.DEF-ghi_~+/=="));
        assert!(!is_token68(""));
        assert!(!is_token68("=="));
        assert!(!is_token68("a b"));
        assert!(!is_token68("a=b"));
        assert!(!is_token68("a\"b"));
    }
}
