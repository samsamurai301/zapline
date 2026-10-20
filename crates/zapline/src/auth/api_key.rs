//! API keys from a header (default `x-api-key`) or a query parameter.

use std::fmt;

use http::{HeaderName, HeaderValue};

use super::{codes, constant_time_eq, missing_credentials, unauthorized};
use crate::extract::FromRequestParts;
use crate::{Error, Request};

/// Longest API key accepted, in bytes.
const MAX_KEY_LEN: usize = 4 * 1024;

/// An API key sent by the client, read from the header configured by
/// [`ApiKeyConfig`] (default `x-api-key`) and, if enabled, a query
/// parameter.
///
/// Rejects with `401` (`WWW-Authenticate: ApiKey header="..."`): code
/// `missing_credentials` when absent, `invalid_credentials` when malformed.
/// Compare with [`verify`](Self::verify), which runs in constant time.
///
/// ```
/// use zapline::auth::ApiKey;
/// use zapline::prelude::*;
///
/// async fn report(key: ApiKey) -> Result<&'static str> {
///     if !key.verify("k-123") {
///         return Err(key.reject());
///     }
///     Ok("report")
/// }
/// # let _ = App::new().get("/report", report);
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// The raw key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The raw key, owned.
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Constant-time comparison with the expected key.
    pub fn verify(&self, expected: impl AsRef<[u8]>) -> bool {
        constant_time_eq(&self.0, expected)
    }

    /// `true` if the key equals any of `keys` (each compared in constant
    /// time; all are checked).
    pub fn verify_any<I>(&self, keys: I) -> bool
    where
        I: IntoIterator,
        I::Item: AsRef<[u8]>,
    {
        keys.into_iter()
            .fold(false, |found, k| found | constant_time_eq(&self.0, k))
    }

    /// A `401 invalid_credentials`, for rejecting an unknown key.
    pub fn reject(&self) -> Error {
        invalid(&ApiKeyConfig::default())
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Where [`ApiKey`] looks for the key. Register with `App::state(..)`;
/// without it the key is read from the `x-api-key` header only.
///
/// ```
/// use zapline::auth::ApiKeyConfig;
/// # use zapline::prelude::*;
/// let app = App::new().state(ApiKeyConfig::new().header("x-token").query_param("api_key"));
/// ```
#[derive(Clone, Debug)]
pub struct ApiKeyConfig {
    /// Header carrying the key.
    pub header: HeaderName,
    /// Query parameter carrying the key, checked when the header is absent.
    /// Keys in URLs end up in logs and browser history; prefer the header.
    pub query_param: Option<String>,
}

impl Default for ApiKeyConfig {
    fn default() -> Self {
        ApiKeyConfig {
            header: HeaderName::from_static("x-api-key"),
            query_param: None,
        }
    }
}

impl ApiKeyConfig {
    /// The default: the `x-api-key` header only.
    pub fn new() -> Self {
        Self::default()
    }

    /// # Panics
    /// If `name` is not a valid header name.
    pub fn header(mut self, name: &str) -> Self {
        self.header = HeaderName::try_from(name)
            .unwrap_or_else(|_| panic!("invalid API key header name: {name:?}"));
        self
    }

    /// Also accept the key from this query parameter.
    pub fn query_param(mut self, name: impl Into<String>) -> Self {
        self.query_param = Some(name.into());
        self
    }

    fn challenge(&self) -> HeaderValue {
        HeaderValue::try_from(format!("ApiKey header=\"{}\"", self.header))
            .unwrap_or(HeaderValue::from_static("ApiKey"))
    }
}

fn invalid(cfg: &ApiKeyConfig) -> Error {
    unauthorized(codes::INVALID, "invalid API key", cfg.challenge())
}

impl FromRequestParts for ApiKey {
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        let default = ApiKeyConfig::default();
        let cfg = req.state::<ApiKeyConfig>().unwrap_or(&default);
        let mut values = req.headers().get_all(&cfg.header).iter();
        let key = match (values.next(), values.next()) {
            (Some(v), None) => v.to_str().map_err(|_| invalid(cfg))?.to_owned(),
            (Some(_), Some(_)) => return Err(invalid(cfg)),
            (None, _) => match cfg.query_param.as_deref().and_then(|p| query_value(req, p)) {
                Some(v) => v,
                None => return Err(missing_credentials(Some(cfg.challenge()))),
            },
        };
        let key = key.trim();
        if key.is_empty() || key.len() > MAX_KEY_LEN || key.chars().any(char::is_control) {
            return Err(invalid(cfg));
        }
        Ok(ApiKey(key.to_owned()))
    }
}

/// The first value of query parameter `name`, percent-decoded.
fn query_value(req: &Request, name: &str) -> Option<String> {
    let query = req.query_string()?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (form_decode(k)? == name).then(|| form_decode(v)).flatten()
    })
}

fn form_decode(s: &str) -> Option<String> {
    let s = s.replace('+', " ");
    percent_encoding::percent_decode_str(&s)
        .decode_utf8()
        .ok()
        .map(|c| c.into_owned())
}
