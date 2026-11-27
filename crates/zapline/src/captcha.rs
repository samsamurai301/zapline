//! Captchas for public forms: Cloudflare Turnstile, hCaptcha and Google
//! reCAPTCHA (feature `captcha`).
//!
//! The page shows the provider's widget, which adds a token to the form as
//! a hidden field ([`Captcha::field`] names it). The handler hands that
//! token to [`Captcha::verify`], which asks the provider whether it is
//! valid. A missing or rejected token answers `400`; a provider that can't
//! be reached answers `503`, so a bot is never let through by an outage.
//!
//! ```no_run
//! use serde::Deserialize;
//! use zapline::captcha::Captcha;
//! use zapline::prelude::*;
//!
//! #[derive(Deserialize)]
//! struct Signup {
//!     email: String,
//!     #[serde(rename = "cf-turnstile-response", default)]
//!     captcha: String,
//! }
//!
//! async fn signup(captcha: Captcha, conn: ConnInfo, Form(form): Form<Signup>) -> Result<String> {
//!     captcha.verify(&form.captcha, conn.remote_addr.map(|a| a.ip())).await?;
//!     Ok(format!("welcome, {}", form.email))
//! }
//!
//! let app = App::new()
//!     .captcha(Captcha::turnstile(std::env::var("TURNSTILE_SECRET").unwrap_or_default()))
//!     .post("/signup", signup);
//! ```

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use http::{Method, Request as HttpRequest};
use http_body_util::Full;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

use crate::extract::FromRequestParts;
use crate::http_client::fetch_limited;
use crate::request::Request;
use crate::{App, Error, Result};

const TURNSTILE: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
const HCAPTCHA: &str = "https://api.hcaptcha.com/siteverify";
const RECAPTCHA: &str = "https://www.google.com/recaptcha/api/siteverify";
/// Longest token accepted; providers' tokens are a few kilobytes at most.
const MAX_TOKEN: usize = 8 * 1024;
/// Largest verification answer read; real ones are a few hundred bytes.
const MAX_ANSWER: usize = 64 * 1024;
/// reCAPTCHA v3's score threshold unless [`Captcha::min_score`] says
/// otherwise (Google's suggested default).
const DEFAULT_MIN_SCORE: f64 = 0.5;

/// A captcha provider and its secret key. Cheap to clone; a handler
/// argument once registered with [`App::captcha`].
#[derive(Clone)]
pub struct Captcha {
    inner: Arc<Inner>,
}

struct Inner {
    name: &'static str,
    field: &'static str,
    verify_url: String,
    secret: String,
    timeout: Duration,
    min_score: f64,
    action: Option<String>,
}

impl fmt::Debug for Captcha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Captcha")
            .field("provider", &self.inner.name)
            .field("verify_url", &self.inner.verify_url)
            .field("secret", &"[hidden]")
            .finish()
    }
}

impl Captcha {
    fn new(name: &'static str, field: &'static str, url: &str, secret: String) -> Self {
        Captcha {
            inner: Arc::new(Inner {
                name,
                field,
                verify_url: url.to_owned(),
                secret,
                timeout: Duration::from_secs(10),
                min_score: DEFAULT_MIN_SCORE,
                action: None,
            }),
        }
    }

    /// Cloudflare Turnstile, with the site's secret key. Its widget posts
    /// the token as `cf-turnstile-response`.
    ///
    /// ```
    /// use zapline::captcha::Captcha;
    /// let captcha = Captcha::turnstile("0x4AAA...");
    /// assert_eq!(captcha.field(), "cf-turnstile-response");
    /// ```
    pub fn turnstile(secret: impl Into<String>) -> Self {
        Self::new(
            "Turnstile",
            "cf-turnstile-response",
            TURNSTILE,
            secret.into(),
        )
    }

    /// hCaptcha, with the account's secret key. Its widget posts the token
    /// as `h-captcha-response`.
    ///
    /// ```
    /// use zapline::captcha::Captcha;
    /// let captcha = Captcha::hcaptcha("0x0000...");
    /// assert_eq!(captcha.field(), "h-captcha-response");
    /// ```
    pub fn hcaptcha(secret: impl Into<String>) -> Self {
        Self::new("hCaptcha", "h-captcha-response", HCAPTCHA, secret.into())
    }

    /// Google reCAPTCHA (v2 or v3), with the site's secret key. Its widget
    /// posts the token as `g-recaptcha-response`. v3 answers carry a score:
    /// below [`min_score`](Self::min_score) (default 0.5) the token is
    /// rejected; set [`action`](Self::action) to check it too.
    ///
    /// ```
    /// use zapline::captcha::Captcha;
    /// let captcha = Captcha::recaptcha("6Lc...");
    /// assert_eq!(captcha.field(), "g-recaptcha-response");
    /// ```
    pub fn recaptcha(secret: impl Into<String>) -> Self {
        Self::new(
            "reCAPTCHA",
            "g-recaptcha-response",
            RECAPTCHA,
            secret.into(),
        )
    }

    /// Send verifications to `url` instead of the provider's own endpoint:
    /// a compatible self-hosted service, or a test double.
    ///
    /// ```
    /// use zapline::captcha::Captcha;
    /// let captcha = Captcha::turnstile("secret").verify_url("http://127.0.0.1:9000/siteverify");
    /// ```
    #[must_use]
    pub fn verify_url(self, url: impl Into<String>) -> Self {
        self.with(|inner| inner.verify_url = url.into())
    }

    /// How long a verification may take before it answers `503` (default
    /// 10 s).
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::captcha::Captcha;
    /// let captcha = Captcha::turnstile("secret").timeout(Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn timeout(self, timeout: Duration) -> Self {
        self.with(|inner| inner.timeout = timeout)
    }

    /// The lowest reCAPTCHA v3 score accepted, from 0.0 (a bot) to 1.0
    /// (a person); default 0.5. Answers without a score (v2, Turnstile,
    /// hCaptcha's free tier) are judged by `success` alone.
    ///
    /// ```
    /// use zapline::captcha::Captcha;
    /// let captcha = Captcha::recaptcha("6Lc...").min_score(0.7);
    /// ```
    #[must_use]
    pub fn min_score(self, score: f64) -> Self {
        self.with(|inner| inner.min_score = score)
    }

    /// The action the widget was rendered with (`grecaptcha.execute(site,
    /// {action: "signup"})`, Turnstile's `data-action`). A token minted for
    /// another action, or with none, is rejected, so a token from one form
    /// can't pass another.
    ///
    /// ```
    /// use zapline::captcha::Captcha;
    /// let captcha = Captcha::recaptcha("6Lc...").action("signup");
    /// ```
    #[must_use]
    pub fn action(self, action: impl Into<String>) -> Self {
        let action = action.into();
        self.with(|inner| inner.action = Some(action))
    }

    fn with(self, f: impl FnOnce(&mut Inner)) -> Self {
        let mut inner = Arc::try_unwrap(self.inner).unwrap_or_else(|shared| Inner {
            name: shared.name,
            field: shared.field,
            verify_url: shared.verify_url.clone(),
            secret: shared.secret.clone(),
            timeout: shared.timeout,
            min_score: shared.min_score,
            action: shared.action.clone(),
        });
        f(&mut inner);
        Captcha {
            inner: Arc::new(inner),
        }
    }

    /// The form field the provider's widget puts its token in.
    pub fn field(&self) -> &'static str {
        self.inner.field
    }

    /// Ask the provider whether `token` (from the form's [`field`](Self::field))
    /// is valid. `remote_ip` is the client's address, which providers use as
    /// one more signal; pass `None` when unknown.
    ///
    /// # Errors
    ///
    /// `400` with code `captcha_missing` for an empty token, `captcha_failed`
    /// when the provider rejects it (or its score or action doesn't match);
    /// `503` with code `captcha_unavailable` when the provider can't be
    /// reached, answers nonsense, or blames the site's own configuration
    /// (a wrong secret key): fail closed, without blaming the visitor.
    ///
    /// ```no_run
    /// # async fn run() -> zapline::Result<()> {
    /// use zapline::captcha::Captcha;
    /// let captcha = Captcha::turnstile("secret");
    /// captcha.verify("token-from-the-form", None).await?;
    /// # Ok(()) }
    /// ```
    pub async fn verify(&self, token: &str, remote_ip: Option<IpAddr>) -> Result<()> {
        let token = token.trim();
        if token.is_empty() {
            return Err(
                Error::bad_request("please complete the captcha").with_code("captcha_missing")
            );
        }
        if token.len() > MAX_TOKEN {
            return Err(failed());
        }
        let inner = &self.inner;
        let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
        let mut form = format!("secret={}&response={}", enc(&inner.secret), enc(token));
        if let Some(ip) = remote_ip {
            form.push_str("&remoteip=");
            form.push_str(&enc(&ip.to_string()));
        }
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri(&inner.verify_url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, "zapline")
            .body(Full::new(Bytes::from(form)))
            .map_err(|e| unavailable(inner.name).with_source(e))?;
        let what = format!("the {} verification service", inner.name);
        let res = tokio::time::timeout(inner.timeout, fetch_limited(request, &what, MAX_ANSWER))
            .await
            .map_err(|_| unavailable(inner.name))?
            .map_err(|e| unavailable(inner.name).with_source(e))?;
        if !res.status.is_success() {
            return Err(unavailable(inner.name)
                .with_source(std::io::Error::other(format!("HTTP {}", res.status))));
        }
        let body: Value = serde_json::from_slice(&res.body)
            .map_err(|e| unavailable(inner.name).with_source(e))?;
        match body.get("success").and_then(Value::as_bool) {
            Some(true) => {}
            Some(false) => {
                return match site_error(&body) {
                    Some(code) => Err(unavailable(inner.name).with_source(std::io::Error::other(
                        format!("the provider reports a site problem: {code}"),
                    ))),
                    None => Err(failed()),
                };
            }
            None => {
                return Err(unavailable(inner.name)
                    .with_source(std::io::Error::other("the answer has no `success` field")))
            }
        }
        if let Some(score) = body.get("score") {
            let score = score.as_f64().ok_or_else(|| {
                unavailable(inner.name).with_source(std::io::Error::other("a non-numeric score"))
            })?;
            if score < inner.min_score {
                return Err(failed());
            }
        }
        if let Some(expected) = &inner.action {
            if body.get("action").and_then(Value::as_str) != Some(expected.as_str()) {
                return Err(failed());
            }
        }
        Ok(())
    }
}

/// An `error-codes` entry that is the site's fault, not the visitor's: a
/// missing or wrong secret, a site key that doesn't match it, or the
/// provider's own failure.
fn site_error(body: &Value) -> Option<String> {
    const SITE: &[&str] = &[
        "missing-input-secret",
        "invalid-input-secret",
        "sitekey-secret-mismatch",
        "invalid-or-already-seen-secret",
        "internal-error",
        "bad-request",
    ];
    body.get("error-codes")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find(|c| SITE.contains(c))
        .map(str::to_owned)
}

fn failed() -> Error {
    Error::bad_request("the captcha was not accepted; please try again").with_code("captcha_failed")
}

fn unavailable(name: &str) -> Error {
    Error::service_unavailable(format!("{name} can't be reached; please try again"))
        .with_code("captcha_unavailable")
}

impl FromRequestParts for Captcha {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Captcha>().cloned().ok_or_else(|| {
            Error::internal(
                "no captcha registered: add `.captcha(Captcha::turnstile(secret))` to the app",
            )
        })
    }
}

impl App {
    /// Register `captcha` for handlers ([`Captcha`] extractor).
    ///
    /// ```
    /// use zapline::captcha::Captcha;
    /// use zapline::prelude::*;
    ///
    /// let app = App::new().captcha(Captcha::hcaptcha("secret"));
    /// ```
    pub fn captcha(self, captcha: Captcha) -> Self {
        self.state(captcha)
    }
}
