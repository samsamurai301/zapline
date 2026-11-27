//! Signed webhooks, received and sent (feature `webhooks`).
//!
//! **Receiving.** A [`Webhook`] holds a provider's signing secret and checks
//! that a request really came from that provider and hasn't been replayed.
//! It supports three schemes: [Standard Webhooks](https://www.standardwebhooks.com)
//! (Svix, Clerk, Resend, and many others), GitHub's `X-Hub-Signature-256`, and
//! Stripe's `Stripe-Signature`. The [`Signed`] extractor verifies the body
//! before your handler sees it:
//!
//! ```no_run
//! use serde::Deserialize;
//! use zapline::prelude::*;
//! use zapline::webhooks::{Signed, Webhook};
//!
//! #[derive(Deserialize)]
//! struct Event { r#type: String }
//!
//! async fn hook(Signed(event): Signed<Event>) -> StatusCode {
//!     println!("got {}", event.r#type);
//!     StatusCode::NO_CONTENT
//! }
//!
//! let app = App::new()
//!     .post("/webhooks/billing", hook)
//!     .meta(Webhook::stripe(std::env::var("STRIPE_WEBHOOK_SECRET").unwrap_or_default()));
//! ```
//!
//! **Sending.** A [`WebhookSender`] signs JSON payloads the Standard
//! Webhooks way and posts them, retrying failures with the same message id
//! so receivers can drop duplicates. Requests go through the
//! [outbound client](crate::outbound), which refuses private addresses, as
//! webhook URLs usually come from users.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::prelude::{Engine as _, BASE64_STANDARD};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use http::{HeaderMap, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::Sha256;

use crate::extract::FromRequest;
use crate::outbound::HttpClient;
use crate::request::Request;
use crate::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

/// How long a signed timestamp is accepted (either side of now), so a
/// captured request can't be replayed later.
const DEFAULT_TOLERANCE: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Standard,
    GitHub,
    Stripe,
}

/// A webhook provider's signing secret(s) and scheme, to verify incoming
/// requests. Register it for a route with `.meta(..)`, or for the whole app
/// with `App::state`, and take [`Signed`] in the handler; or call
/// [`verify`](Self::verify) yourself.
#[derive(Clone)]
pub struct Webhook {
    scheme: Scheme,
    keys: Vec<Vec<u8>>,
    tolerance: Duration,
}

impl fmt::Debug for Webhook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Webhook")
            .field("scheme", &self.scheme)
            .field("secrets", &self.keys.len())
            .field("tolerance", &self.tolerance)
            .finish()
    }
}

/// The key a Standard Webhooks secret stands for: `whsec_` and base64.
fn standard_key(secret: &str) -> Result<Vec<u8>> {
    let encoded = secret.strip_prefix("whsec_").unwrap_or(secret);
    BASE64_STANDARD
        .decode(encoded.trim())
        .map_err(|_| Error::internal("a Standard Webhooks secret is `whsec_` followed by base64"))
}

impl Webhook {
    fn new(scheme: Scheme, key: Vec<u8>) -> Self {
        Webhook {
            scheme,
            keys: vec![key],
            tolerance: DEFAULT_TOLERANCE,
        }
    }

    /// The [Standard Webhooks](https://www.standardwebhooks.com) scheme:
    /// `webhook-id`, `webhook-timestamp` and `webhook-signature: v1,<base64>`
    /// headers, with a `whsec_...` secret.
    ///
    /// # Errors
    ///
    /// When `secret` isn't `whsec_` followed by base64.
    ///
    /// ```
    /// use zapline::webhooks::Webhook;
    /// let hook = Webhook::standard("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw")?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn standard(secret: &str) -> Result<Self> {
        Ok(Self::new(Scheme::Standard, standard_key(secret)?))
    }

    /// GitHub's scheme: `X-Hub-Signature-256: sha256=<hex>` over the body,
    /// with the secret set on the webhook. GitHub signs no timestamp, so
    /// replays can't be detected; use the `X-GitHub-Delivery` id to skip
    /// duplicates.
    ///
    /// ```
    /// use zapline::webhooks::Webhook;
    /// let hook = Webhook::github("my-webhook-secret");
    /// ```
    pub fn github(secret: impl AsRef<[u8]>) -> Self {
        Self::new(Scheme::GitHub, secret.as_ref().to_vec())
    }

    /// Stripe's scheme: `Stripe-Signature: t=<time>,v1=<hex>` over
    /// `<time>.<body>`, with the endpoint's `whsec_...` signing secret.
    ///
    /// ```
    /// use zapline::webhooks::Webhook;
    /// let hook = Webhook::stripe("whsec_test_secret");
    /// ```
    pub fn stripe(secret: impl AsRef<[u8]>) -> Self {
        Self::new(Scheme::Stripe, secret.as_ref().to_vec())
    }

    /// Also accept signatures made with `secret`: while rotating secrets,
    /// both the old and the new one verify. For the standard scheme it is a
    /// `whsec_` secret; an invalid one is ignored.
    ///
    /// ```
    /// use zapline::webhooks::Webhook;
    /// let hook = Webhook::github("new-secret").also("old-secret");
    /// ```
    #[must_use]
    pub fn also(mut self, secret: impl AsRef<[u8]>) -> Self {
        let key = match self.scheme {
            Scheme::Standard => std::str::from_utf8(secret.as_ref())
                .ok()
                .and_then(|s| standard_key(s).ok()),
            Scheme::GitHub | Scheme::Stripe => Some(secret.as_ref().to_vec()),
        };
        if let Some(key) = key {
            self.keys.push(key);
        }
        self
    }

    /// How far a signed timestamp may be from now (default 5 minutes).
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::webhooks::Webhook;
    /// let hook = Webhook::stripe("whsec_x").tolerance(Duration::from_secs(60));
    /// ```
    #[must_use]
    pub fn tolerance(mut self, tolerance: Duration) -> Self {
        self.tolerance = tolerance;
        self
    }

    /// Check that `body` was signed by the provider, recently.
    ///
    /// # Errors
    ///
    /// `400 webhook_signature_missing` when the signature headers are
    /// absent or malformed, `401 webhook_timestamp_stale` for a timestamp
    /// outside the tolerance, `401 webhook_signature_invalid` when no
    /// signature matches.
    ///
    /// ```
    /// use zapline::webhooks::{Webhook, WebhookSender};
    /// use zapline::HeaderMap;
    ///
    /// let secret = WebhookSender::generate_secret();
    /// let hook = Webhook::standard(&secret)?;
    /// let headers = HeaderMap::new();
    /// assert!(hook.verify(&headers, b"{}").is_err());
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()> {
        self.verify_at(headers, body, now())
    }

    fn verify_at(&self, headers: &HeaderMap, body: &[u8], now: i64) -> Result<()> {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| missing(name))
        };
        match self.scheme {
            Scheme::Standard => {
                let id = header("webhook-id")?;
                let timestamp = header("webhook-timestamp")?;
                let signatures = header("webhook-signature")?;
                let ts: i64 = timestamp
                    .parse()
                    .map_err(|_| missing("webhook-timestamp"))?;
                self.check_time(ts, now)?;
                let candidates: Vec<Vec<u8>> = signatures
                    .split(' ')
                    .filter_map(|s| s.strip_prefix("v1,"))
                    .filter_map(|b| BASE64_STANDARD.decode(b).ok())
                    .collect();
                let signed = [id.as_bytes(), b".", timestamp.as_bytes(), b".", body].concat();
                self.check_macs(&signed, &candidates)
            }
            Scheme::GitHub => {
                let value = header("x-hub-signature-256")?;
                let hex = value
                    .strip_prefix("sha256=")
                    .ok_or_else(|| missing("x-hub-signature-256"))?;
                let candidates: Vec<Vec<u8>> = decode_hex(hex).into_iter().collect();
                self.check_macs(body, &candidates)
            }
            Scheme::Stripe => {
                let value = header("stripe-signature")?;
                let mut ts = None;
                let mut candidates = Vec::new();
                for part in value.split(',') {
                    match part.trim().split_once('=') {
                        Some(("t", t)) => ts = t.parse::<i64>().ok(),
                        Some(("v1", sig)) => candidates.extend(decode_hex(sig)),
                        _ => {}
                    }
                }
                let ts = ts.ok_or_else(|| missing("stripe-signature"))?;
                self.check_time(ts, now)?;
                let signed = [ts.to_string().as_bytes(), b".", body].concat();
                self.check_macs(&signed, &candidates)
            }
        }
    }

    fn check_time(&self, ts: i64, now: i64) -> Result<()> {
        let tolerance = i64::try_from(self.tolerance.as_secs()).unwrap_or(i64::MAX);
        if now.abs_diff(ts) > tolerance.unsigned_abs() {
            return Err(Error::unauthorized_with(
                "the webhook's timestamp is too old or in the future",
                "webhook_timestamp_stale",
            ));
        }
        Ok(())
    }

    fn check_macs(&self, signed: &[u8], candidates: &[Vec<u8>]) -> Result<()> {
        for key in &self.keys {
            for sig in candidates {
                let Ok(mut mac) = HmacSha256::new_from_slice(key) else {
                    continue;
                };
                mac.update(signed);
                // Constant-time comparison.
                if mac.verify_slice(sig).is_ok() {
                    return Ok(());
                }
            }
        }
        Err(Error::unauthorized_with(
            "the webhook's signature doesn't match",
            "webhook_signature_invalid",
        ))
    }
}

fn missing(header: &str) -> Error {
    Error::bad_request(format!("missing or malformed `{header}` header"))
        .with_code("webhook_signature_missing")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

trait Unauthorized {
    fn unauthorized_with(message: &'static str, code: &'static str) -> Error;
}

impl Unauthorized for Error {
    fn unauthorized_with(message: &'static str, code: &'static str) -> Error {
        Error::new(StatusCode::UNAUTHORIZED, message).with_code(code)
    }
}

/// A verified webhook body, parsed as JSON into `T` (use
/// `Signed<serde_json::Value>` to take it as is). Must be the last handler
/// argument. Uses the route's [`Webhook`] (`.meta(..)`), else the app's
/// (`App::state`).
///
/// Rejects with the errors of [`Webhook::verify`], or `422` when the body
/// isn't the expected JSON.
#[derive(Debug, Clone)]
pub struct Signed<T>(pub T);

impl<T: DeserializeOwned + Send> FromRequest for Signed<T> {
    async fn from_request(mut req: Request) -> Result<Self> {
        let hook = req
            .route()
            .and_then(|r| r.meta::<Webhook>())
            .or_else(|| req.state::<Webhook>())
            .cloned()
            .ok_or_else(|| {
                Error::internal(
                    "no webhook secret for this route: add `.meta(Webhook::...)` after it",
                )
            })?;
        let body = req.body_bytes().await?;
        hook.verify(req.headers(), &body)?;
        serde_json::from_slice(&body).map(Signed).map_err(|e| {
            Error::unprocessable("the webhook body isn't the expected JSON")
                .with_code("invalid_body")
                .with_source(e)
        })
    }
}

/// Sends webhooks signed the Standard Webhooks way, retrying failures with
/// the same message id. Cheap to clone.
#[derive(Clone)]
pub struct WebhookSender {
    key: Vec<u8>,
    http: HttpClient,
    retries: u32,
    backoff: Duration,
}

impl fmt::Debug for WebhookSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookSender")
            .field("retries", &self.retries)
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

/// How a webhook was delivered.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Delivery {
    /// The `webhook-id`, the same on every attempt (receivers drop repeats).
    pub id: String,
    /// The receiver's answer to the last attempt.
    pub status: StatusCode,
    /// How many requests were sent.
    pub attempts: u32,
}

impl WebhookSender {
    /// A sender signing with `secret` (`whsec_...`; give the same secret to
    /// the receiver).
    ///
    /// # Errors
    ///
    /// When `secret` isn't `whsec_` followed by base64.
    ///
    /// ```
    /// use zapline::webhooks::WebhookSender;
    /// let sender = WebhookSender::new(&WebhookSender::generate_secret())?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn new(secret: &str) -> Result<Self> {
        Ok(WebhookSender {
            key: standard_key(secret)?,
            http: HttpClient::new().retries(0),
            retries: 3,
            backoff: Duration::from_secs(1),
        })
    }

    /// A new random secret (32 bytes), `whsec_` and base64: store one per
    /// subscriber and show it to them once.
    ///
    /// ```
    /// use zapline::webhooks::WebhookSender;
    /// assert!(WebhookSender::generate_secret().starts_with("whsec_"));
    /// ```
    pub fn generate_secret() -> String {
        let mut key = [0u8; 32];
        if getrandom::getrandom(&mut key).is_err() {
            for chunk in key.chunks_mut(8) {
                chunk.copy_from_slice(&fastrand::u64(..).to_le_bytes());
            }
        }
        format!("whsec_{}", BASE64_STANDARD.encode(key))
    }

    /// How many times a failed delivery is retried (default 3): after a
    /// connection failure, a timeout, `408`, `429` or a `5xx`. Waits
    /// [`backoff`](Self::backoff), then twice that, and so on.
    ///
    /// ```
    /// use zapline::webhooks::WebhookSender;
    /// let sender = WebhookSender::new(&WebhookSender::generate_secret())?.retries(5);
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[must_use]
    pub fn retries(mut self, n: u32) -> Self {
        self.retries = n;
        self
    }

    /// The first wait between attempts (default 1 s; it doubles each time).
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::webhooks::WebhookSender;
    /// let sender = WebhookSender::new(&WebhookSender::generate_secret())?
    ///     .backoff(Duration::from_millis(500));
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[must_use]
    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Send with `http` (its timeouts and network rules) instead of the
    /// default client, which refuses private addresses.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// use zapline::webhooks::WebhookSender;
    /// let sender = WebhookSender::new(&WebhookSender::generate_secret())?
    ///     .http_client(HttpClient::new().timeout(std::time::Duration::from_secs(5)));
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[must_use]
    pub fn http_client(mut self, http: HttpClient) -> Self {
        self.http = http.retries(0);
        self
    }

    fn sign(&self, id: &str, timestamp: i64, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC takes any key length");
        mac.update(id.as_bytes());
        mac.update(b".");
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        format!("v1,{}", BASE64_STANDARD.encode(mac.finalize().into_bytes()))
    }

    /// POST `payload` as JSON to `url`, signed, until the receiver answers
    /// 2xx or the retries run out. Each attempt is signed with the current
    /// time and the same `webhook-id`.
    ///
    /// # Errors
    ///
    /// `400 upstream_blocked` / `upstream_invalid_url` for a refused URL (not
    /// retried), and `502 webhook_delivery_failed` once every attempt failed.
    ///
    /// ```no_run
    /// # async fn run(secret: String) -> zapline::Result<()> {
    /// use zapline::webhooks::WebhookSender;
    /// let sender = WebhookSender::new(&secret)?;
    /// let delivery = sender
    ///     .send("https://customer.example/hooks", &serde_json::json!({"type": "invoice.paid"}))
    ///     .await?;
    /// println!("{} delivered in {} attempt(s)", delivery.id, delivery.attempts);
    /// # Ok(()) }
    /// ```
    pub async fn send(&self, url: &str, payload: &(impl Serialize + ?Sized)) -> Result<Delivery> {
        let body =
            Bytes::from(serde_json::to_vec(payload).map_err(|e| {
                Error::internal("cannot serialize a webhook payload").with_source(e)
            })?);
        let id = message_id();
        let mut attempts = 0;
        loop {
            attempts += 1;
            let last;
            let timestamp = now();
            let result = self
                .http
                .post(url)
                .header("content-type", "application/json")
                .header("webhook-id", &id)
                .header("webhook-timestamp", &timestamp.to_string())
                .header("webhook-signature", &self.sign(&id, timestamp, &body))
                .body(body.clone())
                .send()
                .await;
            let retryable = match &result {
                Ok(res) if res.status().is_success() => {
                    return Ok(Delivery {
                        id,
                        status: res.status(),
                        attempts,
                    });
                }
                Ok(res) => {
                    last = format!("answered {}", res.status());
                    matches!(res.status().as_u16(), 408 | 429) || res.status().is_server_error()
                }
                Err(e) if e.status() == StatusCode::BAD_REQUEST => {
                    // A refused or invalid URL won't get better.
                    return Err(result.unwrap_err());
                }
                Err(e) => {
                    last = e.to_string();
                    true
                }
            };
            if !retryable || attempts > self.retries {
                return Err(Error::new(
                    StatusCode::BAD_GATEWAY,
                    format!("webhook {id} to {url} failed after {attempts} attempt(s): {last}"),
                )
                .with_code("webhook_delivery_failed"));
            }
            let wait = self.backoff.saturating_mul(1 << (attempts - 1).min(10));
            tokio::time::sleep(wait).await;
        }
    }
}

/// `msg_` and 16 random bytes in hex.
fn message_id() -> String {
    let mut raw = [0u8; 16];
    if getrandom::getrandom(&mut raw).is_err() {
        raw[..8].copy_from_slice(&fastrand::u64(..).to_le_bytes());
        raw[8..].copy_from_slice(&fastrand::u64(..).to_le_bytes());
    }
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    format!("msg_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn hmac_hex(key: &[u8], data: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(key).unwrap();
        mac.update(data);
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn headers(pairs: &[(&'static str, String)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    /// The Standard Webhooks spec's published test vector.
    #[test]
    fn standard_webhooks_test_vector() {
        let hook = Webhook::standard("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
        let body = br#"{"test": 2432232314}"#;
        let h = headers(&[
            ("webhook-id", "msg_p5jXN8AQM9LWM0D4loKWxJek".into()),
            ("webhook-timestamp", "1614265330".into()),
            (
                "webhook-signature",
                "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=".into(),
            ),
        ]);
        hook.verify_at(&h, body, 1_614_265_330).unwrap();
        // Five minutes later still passes; a day later is a replay.
        hook.verify_at(&h, body, 1_614_265_330 + 300).unwrap();
        let err = hook
            .verify_at(&h, body, 1_614_265_330 + 86_400)
            .unwrap_err();
        assert_eq!(err.code(), Some("webhook_timestamp_stale"));
        // A changed body fails.
        let err = hook.verify_at(&h, b"{}", 1_614_265_330).unwrap_err();
        assert_eq!(err.code(), Some("webhook_signature_invalid"));
    }

    #[test]
    fn sender_signatures_verify_and_rotation_works() {
        let old = WebhookSender::generate_secret();
        let new = WebhookSender::generate_secret();
        let sender = WebhookSender::new(&old).unwrap();
        let ts = now();
        let sig = sender.sign("msg_1", ts, b"{\"a\":1}");
        let h = headers(&[
            ("webhook-id", "msg_1".into()),
            ("webhook-timestamp", ts.to_string()),
            ("webhook-signature", format!("v1,bogus {sig}")),
        ]);
        let hook = Webhook::standard(&new).unwrap();
        assert!(hook.verify(&h, b"{\"a\":1}").is_err());
        hook.also(old.as_str()).verify(&h, b"{\"a\":1}").unwrap();
    }

    #[test]
    fn github_and_stripe() {
        let body = b"{\"zen\":\"keep it simple\"}";
        let gh = Webhook::github("s3cret");
        let h = headers(&[(
            "x-hub-signature-256",
            format!("sha256={}", hmac_hex(b"s3cret", body)),
        )]);
        gh.verify(&h, body).unwrap();
        assert!(gh.verify(&h, b"{}").is_err());
        let err = gh.verify(&HeaderMap::new(), body).unwrap_err();
        assert_eq!(err.code(), Some("webhook_signature_missing"));

        let stripe = Webhook::stripe("whsec_test");
        let t = now();
        let sig = hmac_hex(
            b"whsec_test",
            format!("{t}.")
                .as_bytes()
                .iter()
                .chain(body)
                .copied()
                .collect::<Vec<_>>()
                .as_slice(),
        );
        let h = headers(&[(
            "stripe-signature",
            format!("t={t},v1=deadbeef,v1={sig},v0=x"),
        )]);
        stripe.verify(&h, body).unwrap();
        let stale = headers(&[("stripe-signature", format!("t={},v1={sig}", t - 3600))]);
        assert_eq!(
            stripe.verify(&stale, body).unwrap_err().code(),
            Some("webhook_timestamp_stale")
        );
    }

    #[test]
    fn bad_secrets_and_ids() {
        assert!(Webhook::standard("whsec_***").is_err());
        let a = message_id();
        assert!(a.starts_with("msg_") && a.len() == 36);
        assert_ne!(a, message_id());
    }
}
