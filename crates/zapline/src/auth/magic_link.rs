//! Signed, expiring tokens for links sent by email: passwordless sign-in,
//! email confirmation, account invitations (feature `cookies`).
//!
//! A token names a user, a purpose and an expiry, signed with the app's
//! cookie [`Key`](crate::cookies::Key). To make a link single-use without a
//! table of used tokens, bind it to a value that changes once it's used:
//! the user's last sign-in time for a sign-in link, the unconfirmed email
//! for a confirmation link. When the value changes, old links stop working.
//!
//! ```
//! use std::time::Duration;
//! use zapline::auth::magic_link::MagicLinks;
//! use zapline::cookies::Key;
//!
//! let links = MagicLinks::new(&Key::generate());
//! // In the "email me a sign-in link" handler:
//! let token = links.issue("user-42", "sign-in", "last-login=1731400000", Duration::from_secs(900));
//! let url = format!("https://shop.example/sign-in?token={token}");
//!
//! // In the handler the link opens: check it against the user's current value.
//! let user = links.verify(&token, "sign-in", |user_id| {
//!     assert_eq!(user_id, "user-42");
//!     Some("last-login=1731400000".to_owned())
//! })?;
//! assert_eq!(user, "user-42");
//! // After signing in, last-login changes, and the same link is refused.
//! assert!(links.verify(&token, "sign-in", |_| Some("last-login=1731400999".to_owned())).is_err());
//! # Ok::<(), zapline::Error>(())
//! ```

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use http::StatusCode;
use sha2::Sha256;

use crate::cookies::Key;
use crate::extract::FromRequestParts;
use crate::request::Request;
use crate::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

/// Issues and checks magic-link tokens. As a handler argument it uses the
/// app's cookie key (`App::cookie_key`).
#[derive(Clone)]
pub struct MagicLinks {
    key: Vec<u8>,
}

impl fmt::Debug for MagicLinks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MagicLinks").finish_non_exhaustive()
    }
}

impl MagicLinks {
    /// Tokens signed with a key derived from `key`, so they can't be
    /// mistaken for signed cookies or CSRF tokens.
    pub fn new(key: &Key) -> Self {
        let mut mac = HmacSha256::new_from_slice(key.signing()).expect("any key length");
        mac.update(b"zapline magic links v1");
        MagicLinks {
            key: mac.finalize().into_bytes().to_vec(),
        }
    }

    fn mac(&self, payload: &[u8]) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("any key length");
        mac.update(payload);
        mac
    }

    /// A URL-safe token for `user` and `purpose` (`"sign-in"`,
    /// `"confirm-email"`, ...), valid for `ttl` and only while the user's
    /// binding value is still `binding`.
    pub fn issue(&self, user: &str, purpose: &str, binding: &str, ttl: Duration) -> String {
        let expires = now().saturating_add(ttl.as_secs());
        let payload = format!("{}.{}.{expires}", b64(user), b64(purpose));
        let mut mac = self.mac(payload.as_bytes());
        mac.update(b".");
        mac.update(binding.as_bytes());
        let sig = BASE64_URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{payload}.{sig}")
    }

    /// The user `token` names, **unverified**: only to look up that user's
    /// binding value (from a database, say) before calling
    /// [`verify`](Self::verify). Never trust it on its own.
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::auth::magic_link::MagicLinks;
    /// use zapline::cookies::Key;
    ///
    /// let links = MagicLinks::new(&Key::generate());
    /// let token = links.issue("42", "sign-in", "3", Duration::from_secs(60));
    /// assert_eq!(MagicLinks::user_of(&token).as_deref(), Some("42"));
    /// assert_eq!(MagicLinks::user_of("junk"), None);
    /// ```
    pub fn user_of(token: &str) -> Option<String> {
        unb64(token.trim().split('.').next()?)
    }

    /// The user a valid `token` for `purpose` names. `binding` is called
    /// with that user and returns their current binding value (`None` when
    /// the user no longer exists).
    ///
    /// # Errors
    ///
    /// `400`, code `link_expired` for an expired token, `link_invalid` for
    /// anything else (tampered, another purpose, used, unknown user). The
    /// two are kept apart so the page can offer to send a new link.
    pub fn verify(
        &self,
        token: &str,
        purpose: &str,
        binding: impl FnOnce(&str) -> Option<String>,
    ) -> Result<String> {
        let invalid = || {
            Error::new(
                StatusCode::BAD_REQUEST,
                "this link is invalid or was already used",
            )
            .with_code("link_invalid")
        };
        let mut parts = token.trim().rsplitn(2, '.');
        let (Some(sig), Some(payload)) = (parts.next(), parts.next()) else {
            return Err(invalid());
        };
        let fields: Vec<&str> = payload.split('.').collect();
        let [user, token_purpose, expires] = fields[..] else {
            return Err(invalid());
        };
        let user = unb64(user).ok_or_else(invalid)?;
        let token_purpose = unb64(token_purpose).ok_or_else(invalid)?;
        let expires: u64 = expires.parse().map_err(|_| invalid())?;
        let sig = BASE64_URL_SAFE_NO_PAD.decode(sig).map_err(|_| invalid())?;
        let current = binding(&user).ok_or_else(invalid)?;
        let mut mac = self.mac(payload.as_bytes());
        mac.update(b".");
        mac.update(current.as_bytes());
        // Constant-time; also fails when the binding value has changed.
        mac.verify_slice(&sig).map_err(|_| invalid())?;
        if token_purpose != purpose {
            return Err(invalid());
        }
        if now() > expires {
            return Err(Error::new(StatusCode::BAD_REQUEST, "this link has expired")
                .with_code("link_expired"));
        }
        Ok(user)
    }
}

impl FromRequestParts for MagicLinks {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Key>().map(MagicLinks::new).ok_or_else(|| {
            Error::internal("no cookie key: register one with `App::cookie_key(Key::..)`")
        })
    }
}

fn b64(s: &str) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(s.as_bytes())
}

fn unb64(s: &str) -> Option<String> {
    String::from_utf8(BASE64_URL_SAFE_NO_PAD.decode(s).ok()?).ok()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links() -> MagicLinks {
        MagicLinks::new(&Key::generate())
    }

    #[test]
    fn tampering_purpose_expiry_and_other_keys_are_refused() {
        let l = links();
        let t = l.issue("u1", "sign-in", "b", Duration::from_secs(60));
        assert_eq!(l.verify(&t, "sign-in", |_| Some("b".into())).unwrap(), "u1");
        let code = |r: Result<String>| r.unwrap_err().code().map(str::to_owned);
        // Another purpose, a deleted user, a changed binding.
        assert_eq!(
            code(l.verify(&t, "confirm-email", |_| Some("b".into()))).as_deref(),
            Some("link_invalid")
        );
        assert_eq!(
            code(l.verify(&t, "sign-in", |_| None)).as_deref(),
            Some("link_invalid")
        );
        // A token for user u1 can't be turned into one for u2.
        let forged = t.replacen(&b64("u1"), &b64("u2"), 1);
        assert_eq!(
            code(l.verify(&forged, "sign-in", |_| Some("b".into()))).as_deref(),
            Some("link_invalid")
        );
        // Another app's key.
        assert!(links().verify(&t, "sign-in", |_| Some("b".into())).is_err());
        // Expired.
        let old = l.issue("u1", "sign-in", "b", Duration::ZERO);
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(
            code(l.verify(&old, "sign-in", |_| Some("b".into()))).as_deref(),
            Some("link_expired")
        );
        for junk in ["", "a.b", "a.b.c.d.e", "!!!.x.y.z"] {
            assert!(
                l.verify(junk, "sign-in", |_| Some("b".into())).is_err(),
                "{junk}"
            );
        }
    }
}
