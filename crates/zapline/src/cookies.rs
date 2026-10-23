//! Cookies (feature `cookies`): the [`CookieJar`] extractor/response, signed
//! and encrypted jars, and secure defaults.
//!
//! Reading and writing cookies:
//!
//! ```
//! use zapline::cookies::{secure_cookie, CookieJar};
//! use zapline::prelude::*;
//!
//! async fn visit(jar: CookieJar) -> (CookieJar, String) {
//!     let n: u32 = jar.get("visits").and_then(|c| c.value().parse().ok()).unwrap_or(0) + 1;
//!     // Returning the jar sends `Set-Cookie` for the changes only.
//!     (jar.add(secure_cookie("visits", n.to_string())), format!("visit #{n}"))
//! }
//! # let _ = App::new().get("/", visit);
//! ```
//!
//! Tamper-proof ([`SignedCookieJar`]) and confidential ([`PrivateCookieJar`])
//! cookies need a [`Key`] registered with [`App::cookie_key`]:
//!
//! ```no_run
//! use zapline::cookies::{Key, PrivateCookieJar};
//! use zapline::prelude::*;
//!
//! async fn remember(jar: PrivateCookieJar) -> (PrivateCookieJar, &'static str) {
//!     (jar.add(("prefs", "dark-mode")), "saved")
//! }
//!
//! App::new()
//!     .cookie_key(Key::generate()) // load a stable 64-byte key in production
//!     .post("/prefs", remember)
//!     .run("0.0.0.0:8080")
//!     .unwrap();
//! ```

use std::borrow::Cow;
use std::fmt;

use http::header::{COOKIE, SET_COOKIE};
use http::{HeaderMap, HeaderValue, StatusCode};

pub use cookie::{time, Cookie, CookieBuilder, Expiration, Key, SameSite};

use crate::extract::FromRequestParts;
use crate::{App, Error, IntoResponse, Request, Response, Result};

/// Most cookies read from one request; the rest are ignored.
const MAX_COOKIES: usize = 256;

/// A cookie with secure defaults: `HttpOnly`, `Secure`, `SameSite=Lax`,
/// `Path=/`. Adjust it further with the [`CookieBuilder`] methods.
///
/// ```
/// use zapline::cookies::{secure_cookie, time::Duration};
/// let c = secure_cookie("theme", "dark").max_age(Duration::days(30)).build();
/// assert_eq!(c.to_string(), "theme=dark; HttpOnly; SameSite=Lax; Secure; Path=/; Max-Age=2592000");
/// ```
pub fn secure_cookie(
    name: impl Into<Cow<'static, str>>,
    value: impl Into<Cow<'static, str>>,
) -> CookieBuilder<'static> {
    Cookie::build((name, value))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .path("/")
}

/// Parse every `Cookie` header (percent-decoded). When a name repeats, the
/// first occurrence wins; malformed pairs are skipped.
fn parse(headers: &HeaderMap) -> cookie::CookieJar {
    let mut jar = cookie::CookieJar::new();
    let mut count = 0;
    for value in headers.get_all(COOKIE) {
        let Ok(s) = value.to_str() else { continue };
        for c in Cookie::split_parse_encoded(s).flatten() {
            if jar.get(c.name()).is_none() {
                jar.add_original(c.into_owned());
            }
            count += 1;
            if count >= MAX_COOKIES {
                return jar;
            }
        }
    }
    jar
}

/// Removal cookies default to `Path=/` (where `secure_cookie` puts them).
fn removal(cookie: impl Into<Cookie<'static>>) -> Cookie<'static> {
    let mut c = cookie.into();
    if c.path().is_none() {
        c.set_path("/");
    }
    c
}

/// Append a `Set-Cookie` header for every change in `jar`.
fn apply(jar: &cookie::CookieJar, res: &mut Response) {
    for c in jar.delta() {
        if let Ok(v) = HeaderValue::try_from(c.encoded().to_string()) {
            res.headers_mut().append(SET_COOKIE, v);
        }
    }
}

fn key(req: &Request) -> Result<Key> {
    req.state::<Key>().cloned().ok_or_else(|| {
        Error::internal("no cookie key: register one with `App::cookie_key(Key::..)`")
            .with_code("cookie_key_missing")
            .diagnosed("ZL0073", None)
    })
}

/// The request's cookies. Changes made with [`add`](Self::add) and
/// [`remove`](Self::remove) are sent back as `Set-Cookie` headers when the
/// jar is returned from the handler: `(jar, body)` or
/// `(StatusCode, jar, body)`.
#[derive(Clone, Default)]
pub struct CookieJar {
    jar: cookie::CookieJar,
}

impl CookieJar {
    /// An empty jar.
    pub fn new() -> Self {
        Self::default()
    }

    /// A jar holding the cookies of these request headers.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        CookieJar {
            jar: parse(headers),
        }
    }

    /// The cookie named `name`, including ones added in this request.
    pub fn get(&self, name: &str) -> Option<&Cookie<'static>> {
        self.jar.get(name)
    }

    /// Add (or replace) a cookie.
    #[must_use = "return the jar from the handler to send the cookie"]
    #[allow(clippy::should_implement_trait)] // mirrors `cookie::CookieJar::add`
    pub fn add(mut self, cookie: impl Into<Cookie<'static>>) -> Self {
        self.jar.add(cookie);
        self
    }

    /// Tell the client to delete a cookie. Pass a name, or a cookie with the
    /// same path/domain it was set with (the path defaults to `/`).
    #[must_use = "return the jar from the handler to send the removal"]
    pub fn remove(mut self, cookie: impl Into<Cookie<'static>>) -> Self {
        self.jar.remove(removal(cookie));
        self
    }

    /// All cookies currently in the jar.
    pub fn iter(&self) -> impl Iterator<Item = &Cookie<'static>> {
        self.jar.iter()
    }

    /// The pending changes (what will be sent as `Set-Cookie`).
    pub fn delta(&self) -> impl Iterator<Item = &Cookie<'static>> {
        self.jar.delta()
    }

    /// A signed view of this jar using `key`.
    pub fn into_signed(self, key: Key) -> SignedCookieJar {
        SignedCookieJar { jar: self.jar, key }
    }

    /// An encrypted view of this jar using `key`.
    pub fn into_private(self, key: Key) -> PrivateCookieJar {
        PrivateCookieJar { jar: self.jar, key }
    }
}

impl fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.jar.iter().map(|c| c.name()))
            .finish()
    }
}

impl FromRequestParts for CookieJar {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        Ok(Self::from_headers(req.headers()))
    }
}

/// Cookies signed with the app's [`Key`] (HMAC-SHA256): readable by the
/// client but tamper-proof. [`get`](Self::get) returns `None` for missing or
/// tampered cookies. Needs [`App::cookie_key`] (otherwise `500`).
#[derive(Clone)]
pub struct SignedCookieJar {
    jar: cookie::CookieJar,
    key: Key,
}

/// Cookies encrypted and authenticated with the app's [`Key`]
/// (AES-256-GCM): the client can neither read nor modify them.
/// [`get`](Self::get) returns `None` for missing or tampered cookies. Needs
/// [`App::cookie_key`] (otherwise `500`).
#[derive(Clone)]
pub struct PrivateCookieJar {
    jar: cookie::CookieJar,
    key: Key,
}

macro_rules! secure_jar {
    ($ty:ident, $view:ident, $view_mut:ident) => {
        impl $ty {
            /// A jar for these request headers.
            pub fn from_headers(headers: &HeaderMap, key: Key) -> Self {
                $ty {
                    jar: parse(headers),
                    key,
                }
            }

            /// The verified cookie named `name`, with its plaintext value.
            pub fn get(&self, name: &str) -> Option<Cookie<'static>> {
                self.jar.$view(&self.key).get(name)
            }

            /// Add (or replace) a cookie; its value is protected on the way out.
            #[must_use = "return the jar from the handler to send the cookie"]
            #[allow(clippy::should_implement_trait)]
            pub fn add(mut self, cookie: impl Into<Cookie<'static>>) -> Self {
                self.jar.$view_mut(&self.key).add(cookie);
                self
            }

            /// Tell the client to delete a cookie (path defaults to `/`).
            #[must_use = "return the jar from the handler to send the removal"]
            pub fn remove(mut self, cookie: impl Into<Cookie<'static>>) -> Self {
                self.jar.$view_mut(&self.key).remove(removal(cookie));
                self
            }

            /// The pending changes, as sent (protected values).
            pub fn delta(&self) -> impl Iterator<Item = &Cookie<'static>> {
                self.jar.delta()
            }

            /// The underlying jar, with raw (protected) values.
            pub fn into_inner(self) -> CookieJar {
                CookieJar { jar: self.jar }
            }
        }

        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_list()
                    .entries(self.jar.iter().map(|c| c.name()))
                    .finish()
            }
        }

        impl FromRequestParts for $ty {
            async fn from_request_parts(req: &mut Request) -> Result<Self> {
                let key = key(req)?;
                Ok(Self::from_headers(req.headers(), key))
            }
        }
    };
}
secure_jar!(SignedCookieJar, signed, signed_mut);
secure_jar!(PrivateCookieJar, private, private_mut);

/// Responses: a jar alone, `(jar, body)` and `(StatusCode, jar, body)`.
macro_rules! jar_responses {
    ($($ty:ident),*) => {$(
        impl IntoResponse for $ty {
            fn into_response(self) -> Response {
                let mut res = ().into_response();
                apply(&self.jar, &mut res);
                res
            }
        }

        impl<T: IntoResponse> IntoResponse for ($ty, T) {
            fn into_response(self) -> Response {
                let mut res = self.1.into_response();
                apply(&self.0.jar, &mut res);
                res
            }
        }

        impl<T: IntoResponse> IntoResponse for (StatusCode, $ty, T) {
            fn into_response(self) -> Response {
                let mut res = (self.1, self.2).into_response();
                *res.status_mut() = self.0;
                res
            }
        }
    )*};
}
jar_responses!(CookieJar, SignedCookieJar, PrivateCookieJar);

/// Development secret for [`key_from_env`] in debug builds. Public on
/// purpose: it only keeps dev sessions alive across restarts.
const DEV_SECRET: &str = "zapline insecure development key: set a real secret in production";

/// The cookie [`Key`] derived from the secret in environment variable `var`
/// (any string of at least 32 bytes, e.g. `openssl rand -base64 48`).
///
/// When `var` is unset or empty, debug builds use a fixed development key, so signed
/// cookies and sessions survive `zapline dev` restarts, and release builds
/// fail: a production app must not start with a guessable key.
///
/// ```
/// # use zapline::prelude::*;
/// let app = App::new().cookie_key(zapline::cookies::key_from_env("SESSION_SECRET")?);
/// # Ok::<(), zapline::Error>(())
/// ```
pub fn key_from_env(var: &str) -> Result<Key> {
    match std::env::var(var).ok().filter(|s| !s.is_empty()) {
        Some(secret) => key_from_secret(&secret).map_err(|e| {
            let msg = format!("{var}: {}", e.message());
            Error::internal(msg).with_code("cookie_key_invalid")
        }),
        None if cfg!(debug_assertions) => key_from_secret(DEV_SECRET),
        None => Err(Error::internal(format!(
            "{var} is not set: release builds need a secret of at least 32 bytes for cookies and sessions"
        ))
        .with_code("cookie_key_missing")),
    }
}

/// A 64-byte [`Key`] derived from `secret` (HMAC-SHA256 expansion). The
/// secret must be at least 32 bytes.
pub fn key_from_secret(secret: &str) -> Result<Key> {
    use hmac::{Hmac, Mac};
    if secret.len() < 32 {
        return Err(Error::internal("the secret must be at least 32 bytes long"));
    }
    let mut bytes = [0u8; 64];
    for (i, chunk) in bytes.chunks_mut(32).enumerate() {
        let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret.as_bytes())
            .map_err(|_| Error::internal("invalid secret"))?;
        mac.update(b"zapline.cookie-key.v1");
        mac.update(&[i as u8]);
        chunk.copy_from_slice(&mac.finalize().into_bytes());
    }
    Ok(Key::from(&bytes))
}

impl App {
    /// Register the [`Key`] used by signed/private cookie jars, sessions and
    /// CSRF protection. Use [`Key::generate`] for development and a stable,
    /// secret 64-byte key ([`Key::from`]) in production so cookies survive
    /// restarts.
    pub fn cookie_key(self, key: Key) -> Self {
        self.state(key)
    }
}

/// 32 bytes from the operating system's CSPRNG, base64url-encoded
/// (43 characters, 256 bits).
pub(crate) fn random_token() -> Result<String> {
    use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| {
        Error::internal("secure random numbers are unavailable")
            .with_source(std::io::Error::other(e.to_string()))
    })?;
    Ok(BASE64_URL_SAFE_NO_PAD.encode(buf))
}
