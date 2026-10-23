//! Snapshot tests from recorded traffic: the file format the
//! [dev console](crate::dev) records into, the comparison that tolerates
//! volatile values, and [`Replay`], which runs a directory of snapshots
//! through a [`TestClient`].
//!
//! A snapshot file (`tests/snapshots/<name>.json`) is a sequence of
//! request/response pairs, replayed in order:
//!
//! ```json
//! {
//!   "zapline_snapshot": 1,
//!   "exchanges": [
//!     {
//!       "request": { "method": "POST", "path": "/users",
//!                    "headers": { "content-type": "application/json" },
//!                    "json": { "name": "Ada" } },
//!       "response": { "status": 201, "content_type": "application/json",
//!                     "json": { "id": 7, "name": "Ada" } }
//!     }
//!   ]
//! }
//! ```
//!
//! Bodies are stored as `json` (JSON content types), `text` (UTF-8) or
//! `base64`. A response whose body was not recorded (streamed, compressed or
//! too large) has `body_not_recorded` instead, and only its status is
//! compared.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::{Future, IntoFuture};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use base64::Engine as _;
use bytes::Bytes;
use http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::test::TestClient;

/// The format version written to `zapline_snapshot`.
const VERSION: u32 = 1;

/// Bodies larger than this are not recorded.
pub(crate) const BODY_CAP: usize = 64 * 1024;

/// What a redacted header value is replaced with.
pub(crate) const REDACTED: &str = "[redacted]";

/// Set to `1` to make [`Replay`] rewrite snapshots whose responses changed
/// instead of failing.
///
/// ```sh
/// ZAPLINE_UPDATE_SNAPSHOTS=1 cargo test --test snapshots
/// ```
pub const ENV_UPDATE_SNAPSHOTS: &str = "ZAPLINE_UPDATE_SNAPSHOTS";

/// Request headers worth replaying (anything else, such as `user-agent`, is
/// noise in a test).
const KEPT_HEADERS: [&str; 9] = [
    "accept",
    "accept-language",
    "content-type",
    "host",
    "origin",
    "referer",
    "sec-fetch-site",
    "x-requested-with",
    "if-none-match",
];

/// Request headers whose values are credentials: recorded as `[redacted]`
/// and not sent on replay (set them with [`Replay::header`]).
const SECRET_HEADERS: [&str; 7] = [
    "authorization",
    "cookie",
    "proxy-authorization",
    "x-api-key",
    "x-auth-token",
    "x-csrf-token",
    "x-xsrf-token",
];

/// [`REDACTED`] as it appears in a query string or urlencoded form.
const REDACTED_FORM: &str = "%5Bredacted%5D";

// ---------------------------------------------------------------------------
// Secret fields

/// A body or query field whose value is a credential: the
/// [`is_secret_name`](crate::secret::is_secret_name) rule (`password`,
/// `secret`, `token`, `*_hash`) plus CSRF fields and API keys.
fn secret_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    crate::secret::is_secret_name(name)
        || ["csrf", "xsrf", "api_key", "apikey", "api-key"]
            .iter()
            .any(|s| lower.contains(s))
}

fn form_key(raw: &str) -> String {
    percent_encoding::percent_decode_str(&raw.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

/// A query string or urlencoded form with the values of secret fields
/// replaced by [`REDACTED_FORM`].
fn redact_form(form: &str) -> String {
    form.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) if !v.is_empty() && secret_field(&form_key(k)) => {
                format!("{k}={REDACTED_FORM}")
            }
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// JSON with the values of secret keys (anywhere in it) replaced by
/// [`REDACTED`]. `null` and booleans are kept: they hold no secret.
fn redact_json(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, item) in map.iter_mut() {
                if secret_field(k) && !matches!(item, Value::Null | Value::Bool(_)) {
                    *item = Value::String(REDACTED.to_owned());
                } else {
                    redact_json(item);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_json),
        _ => {}
    }
}

fn is_form(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| {
        ct.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/x-www-form-urlencoded")
    })
}

// ---------------------------------------------------------------------------
// The file format

/// One snapshot file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct SnapshotFile {
    pub(crate) zapline_snapshot: u32,
    pub(crate) exchanges: Vec<Exchange>,
}

impl SnapshotFile {
    pub(crate) fn new(exchanges: Vec<Exchange>) -> Self {
        SnapshotFile {
            zapline_snapshot: VERSION,
            exchanges,
        }
    }

    pub(crate) fn to_pretty(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_default();
        s.push('\n');
        s
    }
}

/// A request and the response the app gave.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Exchange {
    pub(crate) request: RecordedRequest,
    pub(crate) response: RecordedResponse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RecordedRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) query: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) headers: BTreeMap<String, String>,
    #[serde(flatten)]
    pub(crate) body: Payload,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RecordedResponse {
    pub(crate) status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_type: Option<String>,
    #[serde(flatten)]
    pub(crate) body: Payload,
    /// Why the body is missing (only the status is compared then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) body_not_recorded: Option<String>,
}

/// A body: at most one of the fields is set; none means empty.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Payload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base64: Option<String>,
}

fn is_json(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| {
        let mime = ct.split(';').next().unwrap_or("").trim();
        mime.eq_ignore_ascii_case("application/json") || mime.ends_with("+json")
    })
}

impl Payload {
    /// Store `bytes` in the most readable form its content type allows.
    pub(crate) fn capture(content_type: Option<&str>, bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Payload::default();
        }
        if is_json(content_type) {
            match serde_json::from_slice::<Value>(bytes) {
                // `null` would read back as "no body".
                Ok(v) if !v.is_null() => {
                    return Payload {
                        json: Some(v),
                        ..Payload::default()
                    }
                }
                _ => {}
            }
        }
        match std::str::from_utf8(bytes) {
            Ok(s) => Payload {
                text: Some(s.to_owned()),
                ..Payload::default()
            },
            Err(_) => Payload {
                base64: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
                ..Payload::default()
            },
        }
    }

    /// [`capture`](Self::capture), with the values of secret-looking JSON
    /// keys and form fields (passwords, tokens, API keys) redacted.
    pub(crate) fn capture_redacted(content_type: Option<&str>, bytes: &[u8]) -> Self {
        let mut body = Payload::capture(content_type, bytes);
        if let Some(v) = &mut body.json {
            redact_json(v);
        }
        if is_form(content_type) {
            if let Some(t) = &mut body.text {
                *t = redact_form(t);
            }
        }
        body
    }

    /// The body as sent.
    pub(crate) fn bytes(&self) -> Vec<u8> {
        if let Some(v) = &self.json {
            serde_json::to_vec(v).unwrap_or_default()
        } else if let Some(t) = &self.text {
            t.clone().into_bytes()
        } else if let Some(b) = &self.base64 {
            base64::engine::general_purpose::STANDARD
                .decode(b)
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}

impl RecordedRequest {
    /// Record a request: the replay-worthy headers, credentials redacted
    /// (headers, and secret-looking fields of the query and body).
    pub(crate) fn new(method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Self {
        let mut kept = BTreeMap::new();
        for (name, value) in headers {
            let name = name.as_str();
            let value = if SECRET_HEADERS.contains(&name) {
                REDACTED.to_owned()
            } else if KEPT_HEADERS.contains(&name) || name.starts_with("hx-") {
                String::from_utf8_lossy(value.as_bytes()).into_owned()
            } else {
                continue;
            };
            kept.entry(name.to_owned())
                .and_modify(|v: &mut String| {
                    if v != REDACTED {
                        v.push_str(", ");
                        v.push_str(&value);
                    }
                })
                .or_insert(value);
        }
        let content_type = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
        RecordedRequest {
            method: method.as_str().to_owned(),
            path: uri.path().to_owned(),
            query: uri.query().map(redact_form),
            headers: kept,
            body: Payload::capture_redacted(content_type, body),
        }
    }

    fn uri(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{q}", self.path),
            None => self.path.clone(),
        }
    }

    /// The request with the redacted values of the fields in `secrets`
    /// filled in.
    fn with_secrets(&self, secrets: &[(String, String)]) -> RecordedRequest {
        let mut req = self.clone();
        if secrets.is_empty() {
            return req;
        }
        let lookup = |k: &str| secrets.iter().find(|(n, _)| n == k).map(|(_, v)| v);
        let fill_form = |form: &str| {
            form.split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((k, REDACTED_FORM)) => match lookup(&form_key(k)) {
                        Some(v) => format!(
                            "{k}={}",
                            percent_encoding::utf8_percent_encode(
                                v,
                                percent_encoding::NON_ALPHANUMERIC
                            )
                        ),
                        None => pair.to_owned(),
                    },
                    _ => pair.to_owned(),
                })
                .collect::<Vec<_>>()
                .join("&")
        };
        fn fill_json(v: &mut Value, lookup: &dyn Fn(&str) -> Option<String>) {
            match v {
                Value::Object(map) => {
                    for (k, item) in map.iter_mut() {
                        match (&*item, lookup(k)) {
                            (Value::String(s), Some(secret)) if s == REDACTED => {
                                *item = Value::String(secret);
                            }
                            _ => fill_json(item, lookup),
                        }
                    }
                }
                Value::Array(items) => items.iter_mut().for_each(|i| fill_json(i, lookup)),
                _ => {}
            }
        }
        req.query = req.query.map(|q| fill_form(&q));
        if let Some(v) = &mut req.body.json {
            fill_json(v, &|k| lookup(k).cloned());
        }
        let form = req
            .headers
            .get("content-type")
            .is_some_and(|ct| is_form(Some(ct)));
        if form {
            if let Some(t) = &mut req.body.text {
                *t = fill_form(t);
            }
        }
        req
    }

    /// The request with recorded ids replaced by the ones this run produced.
    fn rewritten(&self, ids: &IdMap) -> RecordedRequest {
        let mut req = self.clone();
        if ids.is_empty() {
            return req;
        }
        req.path = ids.rewrite_text(&req.path);
        req.query = req.query.map(|q| ids.rewrite_form(&q));
        if let Some(v) = &mut req.body.json {
            ids.rewrite_json(v, None);
        }
        let form = req
            .headers
            .get("content-type")
            .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
        if let Some(t) = &mut req.body.text {
            *t = if form {
                ids.rewrite_form(t)
            } else {
                ids.rewrite_text(t)
            };
        }
        req
    }
}

impl RecordedResponse {
    /// Record a response whose body is `body` (`None`: a streamed body).
    pub(crate) fn new(status: u16, headers: &HeaderMap, body: Option<&[u8]>) -> Self {
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let not_recorded = if headers.contains_key(CONTENT_ENCODING) {
            Some("compressed")
        } else {
            match body {
                None => Some("streamed"),
                Some(b) if b.len() > BODY_CAP => Some("larger than 64 KiB"),
                Some(_) => None,
            }
        };
        let body = match (not_recorded, body) {
            (None, Some(b)) => Payload::capture_redacted(content_type.as_deref(), b),
            _ => Payload::default(),
        };
        RecordedResponse {
            status,
            content_type,
            body,
            body_not_recorded: not_recorded.map(str::to_owned),
        }
    }
}

// ---------------------------------------------------------------------------
// Volatile values

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Volatile {
    Uuid,
    Timestamp,
    Token,
}

impl Volatile {
    fn placeholder(self) -> &'static str {
        match self {
            Volatile::Uuid => "[uuid]",
            Volatile::Timestamp => "[timestamp]",
            Volatile::Token => "[token]",
        }
    }
}

fn is_uuid(b: &[u8]) -> bool {
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The length of the ISO 8601 date-time at the start of `b`
/// (`2024-05-01T12:30:00.123Z`, `2024-05-01 12:30+02:00`, ...). A bare
/// date is not a timestamp.
fn timestamp_len(b: &[u8]) -> Option<usize> {
    fn digits(b: &[u8], at: usize, n: usize) -> bool {
        b.len() >= at + n && b[at..at + n].iter().all(u8::is_ascii_digit)
    }
    if !(digits(b, 0, 4)
        && b.get(4) == Some(&b'-')
        && digits(b, 5, 2)
        && b.get(7) == Some(&b'-')
        && digits(b, 8, 2)
        && matches!(b.get(10), Some(b'T' | b' '))
        && digits(b, 11, 2)
        && b.get(13) == Some(&b':')
        && digits(b, 14, 2))
    {
        return None;
    }
    let mut i = 16;
    if b.get(i) == Some(&b':') && digits(b, i + 1, 2) {
        i += 3;
        if b.get(i) == Some(&b'.') && digits(b, i + 1, 1) {
            i += 1;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
    }
    match b.get(i) {
        Some(b'Z') => i += 1,
        Some(b'+' | b'-') if digits(b, i + 1, 2) => {
            i += 3;
            if b.get(i) == Some(&b':') && digits(b, i + 1, 2) {
                i += 3;
            } else if digits(b, i, 2) {
                i += 2;
            }
        }
        _ => {}
    }
    Some(i)
}

fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'+' | b'=')
}

/// A random-looking string: session ids, API keys, JWTs, content hashes.
/// Long, made of token characters, letters and digits mixed often enough
/// that it is not a word or a slug (`my-article-2024` is not a token). Hex
/// digests of 16 digits or more always are.
fn is_token(b: &[u8]) -> bool {
    if b.len() >= 16 && is_hex_digest(b) {
        return true;
    }
    if b.len() < 20 || !b.iter().all(|c| is_token_char(*c) || *c == b'/') {
        return false;
    }
    let class = |c: u8| match c {
        b'0'..=b'9' => 1,
        b'a'..=b'z' => 2,
        b'A'..=b'Z' => 3,
        _ => 0,
    };
    let alnum: Vec<u8> = b.iter().map(|c| class(*c)).filter(|k| *k != 0).collect();
    let has_digit = alnum.contains(&1);
    let has_letter = alnum.iter().any(|k| *k > 1);
    let switches = alnum.windows(2).filter(|w| w[0] != w[1]).count();
    has_digit && has_letter && switches * 8 >= alnum.len()
}

fn is_hex_digest(b: &[u8]) -> bool {
    b.iter().all(u8::is_ascii_hexdigit)
        && b.iter().any(u8::is_ascii_digit)
        && b.iter().any(u8::is_ascii_alphabetic)
}

/// Looser than [`is_token`], for two values in the same place: both long,
/// unspaced, with letters and digits, of the same length and different in
/// most positions, like two outputs of one generator. (An edit such as
/// `release-2024` -> `release-2025` changes one position, and is caught.)
fn both_generated(r: &str, a: &str) -> bool {
    let generated = |b: &[u8]| {
        b.len() >= 16
            && b.iter().all(|c| is_token_char(*c))
            && b.iter().any(u8::is_ascii_digit)
            && b.iter().any(u8::is_ascii_alphabetic)
    };
    let (r, a) = (r.as_bytes(), a.as_bytes());
    let differing = r.iter().zip(a).filter(|(x, y)| x != y).count();
    generated(r) && generated(a) && r.len() == a.len() && differing * 2 >= r.len()
}

fn classify(s: &str) -> Option<Volatile> {
    let b = s.as_bytes();
    if is_uuid(b) {
        Some(Volatile::Uuid)
    } else if timestamp_len(b) == Some(b.len()) || httpdate::parse_http_date(s).is_ok() {
        Some(Volatile::Timestamp)
    } else if is_token(b) {
        Some(Volatile::Token)
    } else {
        None
    }
}

/// Replace every UUID, timestamp and token inside `s` with a placeholder.
fn normalize_text(s: &str) -> String {
    if httpdate::parse_http_date(s).is_ok() {
        return Volatile::Timestamp.placeholder().to_owned();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0 || !is_token_char(b[i - 1]);
        if !boundary || !is_token_char(b[i]) {
            i += 1;
            continue;
        }
        let rest = &b[i..];
        let found = if rest.len() >= 36 && is_uuid(&rest[..36]) {
            Some((36, Volatile::Uuid))
        } else if let Some(n) = timestamp_len(rest) {
            Some((n, Volatile::Timestamp))
        } else {
            let n = rest.iter().take_while(|c| is_token_char(**c)).count();
            is_token(&rest[..n]).then_some((n, Volatile::Token))
        };
        match found {
            Some((n, kind)) => {
                out.push_str(&s[copied..i]);
                out.push_str(kind.placeholder());
                i += n;
                copied = i;
            }
            None => {
                let n = rest
                    .iter()
                    .take_while(|c| is_token_char(**c))
                    .count()
                    .max(1);
                let word = &s[i..i + n];
                if let Some(unhashed) = without_content_hash(word) {
                    out.push_str(&s[copied..i]);
                    out.push_str(&unhashed);
                    copied = i + n;
                }
                i += n;
            }
        }
    }
    out.push_str(&s[copied..]);
    out
}

/// `app.3f2a9c41.css` -> `app.[hash].css`: asset URLs carry a hash of the
/// file, which changes whenever the file does. A hash is a dot-separated
/// part after the first of at least 8 hex digits, mixing digits and letters.
fn without_content_hash(word: &str) -> Option<String> {
    let is_hash = |part: &str| {
        part.len() >= 8
            && part.bytes().all(|c| c.is_ascii_hexdigit())
            && part.bytes().any(|c| c.is_ascii_digit())
            && part.bytes().any(|c| c.is_ascii_alphabetic())
    };
    let mut parts = word.split('.');
    let first = parts.next()?;
    let rest: Vec<&str> = parts.collect();
    if !rest.iter().any(|p| is_hash(p)) {
        return None;
    }
    let mut out = first.to_owned();
    for part in rest {
        out.push('.');
        out.push_str(if is_hash(part) { "[hash]" } else { part });
    }
    Some(out)
}

/// `id`, `user_id`, `userId`, `uuid`.
fn id_like(key: &str) -> bool {
    key.eq_ignore_ascii_case("id")
        || key.eq_ignore_ascii_case("uuid")
        || key.ends_with("_id")
        || key.ends_with("Id")
        || key.ends_with("_uuid")
}

/// `created_at`, `updatedAt`, `timestamp`, JWT `exp`/`iat`/`nbf`.
fn time_like(key: &str) -> bool {
    key.ends_with("_at")
        || key.ends_with("At")
        || matches!(
            key,
            "timestamp" | "time" | "date" | "now" | "expires" | "exp" | "iat" | "nbf"
        )
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Ids that differ between the recording and the replay

/// Recorded value -> the value this replay produced in the same place. A
/// recorded `POST /users` answered `{"id": 7}`; on replay it answers
/// `{"id": 1}`, so the next request's `/users/7` is sent as `/users/1`.
#[derive(Default, Debug)]
struct IdMap(HashMap<String, String>);

impl IdMap {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn learn(&mut self, recorded: &Value, actual: &Value) {
        if let (Some(r), Some(a)) = (scalar_text(recorded), scalar_text(actual)) {
            if r != a && !r.is_empty() {
                self.0.insert(r, a);
            }
        }
    }

    /// Replace the recorded ids in free text: a word (run of
    /// `[A-Za-z0-9_-]`) that is a recorded id and either follows a `/` (a
    /// path segment, `/users/7`) or is distinctive (a UUID, a token, a number
    /// of three digits or more). A bare `7` elsewhere is left alone: it is
    /// more likely a count or a page than the id.
    fn rewrite_text(&self, s: &str) -> String {
        if self.0.is_empty() {
            return s.to_owned();
        }
        let word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        let mut out = String::with_capacity(s.len());
        let mut start = None;
        for (i, c) in s.char_indices().chain(std::iter::once((s.len(), ' '))) {
            let end = i == s.len();
            match (word(c) && !end, start) {
                (true, None) => start = Some(i),
                (true, Some(_)) => {}
                (false, st) => {
                    if let Some(st) = st {
                        let w = &s[st..i];
                        let segment = s[..st].ends_with('/');
                        match self.0.get(w) {
                            Some(new) if segment || distinctive(w) => out.push_str(new),
                            _ => out.push_str(w),
                        }
                        start = None;
                    }
                    if !end {
                        out.push(c);
                    }
                }
            }
        }
        out
    }

    /// A query string or urlencoded form: the values of id fields
    /// (`user_id=7`) are replaced whole; other values as free text.
    fn rewrite_form(&self, form: &str) -> String {
        if self.0.is_empty() {
            return form.to_owned();
        }
        form.split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) => match self.0.get(v) {
                    Some(new) if id_like(k) => format!("{k}={new}"),
                    _ => format!("{k}={}", self.rewrite_text(v)),
                },
                None => pair.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// JSON: id fields are replaced whole (keeping numbers numbers), other
    /// strings as free text.
    fn rewrite_json(&self, v: &mut Value, key: Option<&str>) {
        if self.0.is_empty() {
            return;
        }
        match v {
            Value::String(_) | Value::Number(_) if key.is_some_and(id_like) => {
                let Some(new) = scalar_text(v).and_then(|old| self.0.get(&old)) else {
                    return;
                };
                *v = match (&*v, new.parse::<serde_json::Number>()) {
                    (Value::Number(_), Ok(n)) => Value::Number(n),
                    _ => Value::String(new.clone()),
                };
            }
            Value::String(s) => *s = self.rewrite_text(s),
            Value::Array(items) => items.iter_mut().for_each(|i| self.rewrite_json(i, None)),
            Value::Object(map) => map
                .iter_mut()
                .for_each(|(k, i)| self.rewrite_json(i, Some(k))),
            Value::Number(_) | Value::Null | Value::Bool(_) => {}
        }
    }
}

/// An id that is safe to replace wherever it appears as a word.
fn distinctive(id: &str) -> bool {
    id.len() >= 3 && (id.len() >= 8 || id.bytes().all(|c| c.is_ascii_digit()))
}

/// Normalize the recorded and actual JSON side by side: volatile strings
/// become placeholders, id fields that differ become `"[id]"` (and teach
/// `ids` the new value), time fields become `"[timestamp]"`.
fn pair(recorded: &Value, actual: &Value, key: Option<&str>, ids: &mut IdMap) -> (Value, Value) {
    use Value::{Array, Number, Object, String as Str};
    let same_scalar = matches!(
        (recorded, actual),
        (Number(_), Number(_)) | (Str(_), Str(_))
    );
    if same_scalar && key.is_some_and(id_like) {
        let recorded = rewrite(recorded, key, ids);
        if recorded == *actual {
            return (recorded, actual.clone());
        }
        ids.learn(&recorded, actual);
        return (Str("[id]".into()), Str("[id]".into()));
    }
    if same_scalar && key.is_some_and(time_like) {
        return (Str("[timestamp]".into()), Str("[timestamp]".into()));
    }
    match (recorded, actual) {
        (Object(r), Object(a)) => {
            let (mut nr, mut na) = (serde_json::Map::new(), serde_json::Map::new());
            for (k, rv) in r {
                match a.get(k) {
                    Some(av) => {
                        let (x, y) = pair(rv, av, Some(k), ids);
                        nr.insert(k.clone(), x);
                        na.insert(k.clone(), y);
                    }
                    None => {
                        nr.insert(k.clone(), solo(&rewrite(rv, Some(k), ids), Some(k)));
                    }
                }
            }
            for (k, av) in a {
                if !r.contains_key(k) {
                    na.insert(k.clone(), solo(av, Some(k)));
                }
            }
            (Object(nr), Object(na))
        }
        (Array(r), Array(a)) => {
            let mut nr = Vec::with_capacity(r.len());
            let mut na = Vec::with_capacity(a.len());
            for i in 0..r.len().max(a.len()) {
                match (r.get(i), a.get(i)) {
                    (Some(x), Some(y)) => {
                        let (x, y) = pair(x, y, None, ids);
                        nr.push(x);
                        na.push(y);
                    }
                    (Some(x), None) => nr.push(solo(&rewrite(x, None, ids), None)),
                    (None, Some(y)) => na.push(solo(y, None)),
                    (None, None) => {}
                }
            }
            (Array(nr), Array(na))
        }
        // Secrets are redacted on both sides; a snapshot recorded before
        // redaction (or edited by hand) still matches.
        (Str(r), Str(a)) if r == REDACTED || a == REDACTED => {
            (Str(REDACTED.into()), Str(REDACTED.into()))
        }
        (Str(r), Str(a)) => {
            let r = ids.rewrite_text(r);
            if r != *a {
                match (classify(&r), classify(a)) {
                    (Some(x), Some(y)) if x == y && x != Volatile::Timestamp => {
                        ids.learn(&Str(r.clone()), actual);
                    }
                    (None, _) | (_, None) if both_generated(&r, a) => {
                        ids.learn(&Str(r.clone()), actual);
                        let token = Str(Volatile::Token.placeholder().into());
                        return (token.clone(), token);
                    }
                    _ => {}
                }
            }
            (Str(normalize_text(&r)), Str(normalize_text(a)))
        }
        _ => (solo(&rewrite(recorded, key, ids), key), solo(actual, key)),
    }
}

/// Normalize one side alone (a value the other side does not have).
fn solo(v: &Value, key: Option<&str>) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), solo(v, Some(k))))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| solo(v, None)).collect()),
        Value::String(_) | Value::Number(_) if key.is_some_and(time_like) => {
            Value::String("[timestamp]".into())
        }
        Value::String(s) => Value::String(normalize_text(s)),
        other => other.clone(),
    }
}

fn rewrite(v: &Value, key: Option<&str>, ids: &IdMap) -> Value {
    let mut v = v.clone();
    ids.rewrite_json(&mut v, key);
    v
}

// ---------------------------------------------------------------------------
// Comparison and diffs

/// Why a response does not match its recording, as readable text.
fn compare(
    recorded: &RecordedResponse,
    actual: &RecordedResponse,
    ids: &mut IdMap,
) -> Option<String> {
    let mut problems = String::new();
    if recorded.status != actual.status {
        problems.push_str(&format!(
            "  status: expected {}, got {}\n",
            recorded.status, actual.status
        ));
    }
    let body = if recorded.body_not_recorded.is_some() {
        None
    } else {
        compare_bodies(&recorded.body, &actual.body, ids)
    };
    if let Some(diff) = body {
        problems.push_str("  body (- recorded, + actual):\n");
        problems.push_str(&diff);
    }
    (!problems.is_empty()).then_some(problems)
}

fn compare_bodies(recorded: &Payload, actual: &Payload, ids: &mut IdMap) -> Option<String> {
    if let (Some(r), Some(a)) = (&recorded.json, &actual.json) {
        let (r, a) = pair(r, a, None, ids);
        if r == a {
            return None;
        }
        let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
        return Some(diff(&pretty(&r), &pretty(&a)));
    }
    if recorded.base64.is_some() || actual.base64.is_some() {
        let (r, a) = (recorded.bytes(), actual.bytes());
        return (r != a).then(|| {
            format!(
                "    binary bodies differ ({} bytes recorded, {} bytes now)\n",
                r.len(),
                a.len()
            )
        });
    }
    let as_text = |p: &Payload| match (&p.json, &p.text) {
        (Some(v), _) => serde_json::to_string_pretty(v).unwrap_or_default(),
        (None, Some(t)) => t.clone(),
        (None, None) => String::new(),
    };
    let r = normalize_text(&ids.rewrite_text(&as_text(recorded)));
    let a = normalize_text(&as_text(actual));
    (r != a).then(|| diff(&r, &a))
}

/// A line diff with two lines of context around each change.
fn diff(recorded: &str, actual: &str) -> String {
    const CONTEXT: usize = 2;
    const MAX_LINES: usize = 80;
    let r: Vec<&str> = recorded.lines().collect();
    let a: Vec<&str> = actual.lines().collect();
    let ops = if r.len().saturating_mul(a.len()) > 4_000_000 {
        // Too big for a table: show both sides.
        r.iter()
            .map(|l| ('-', *l))
            .chain(a.iter().map(|l| ('+', *l)))
            .collect()
    } else {
        lcs_ops(&r, &a)
    };
    let changed: Vec<bool> = ops.iter().map(|(op, _)| *op != ' ').collect();
    let near = |i: usize| {
        let lo = i.saturating_sub(CONTEXT);
        let hi = (i + CONTEXT + 1).min(changed.len());
        changed[lo..hi].iter().any(|c| *c)
    };
    let mut out = String::new();
    let mut lines = 0;
    let mut skipped = false;
    for (i, (op, line)) in ops.iter().enumerate() {
        if !near(i) {
            skipped = true;
            continue;
        }
        if lines == MAX_LINES {
            out.push_str("    ... (diff cut)\n");
            break;
        }
        if skipped && lines > 0 {
            out.push_str("    ...\n");
        }
        skipped = false;
        out.push_str(&format!("    {op} {line}\n"));
        lines += 1;
    }
    if out.is_empty() {
        // Only trailing whitespace or newlines differ.
        out.push_str("    (bodies differ in whitespace only)\n");
    }
    out
}

fn lcs_ops<'a>(r: &[&'a str], a: &[&'a str]) -> Vec<(char, &'a str)> {
    let (n, m) = (r.len(), a.len());
    // lens[i][j]: LCS length of r[i..] and a[j..].
    let mut lens = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lens[at(i, j)] = if r[i] == a[j] {
                lens[at(i + 1, j + 1)] + 1
            } else {
                lens[at(i + 1, j)].max(lens[at(i, j + 1)])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::with_capacity(n + m);
    while i < n && j < m {
        if r[i] == a[j] {
            ops.push((' ', r[i]));
            i += 1;
            j += 1;
        } else if lens[at(i + 1, j)] >= lens[at(i, j + 1)] {
            ops.push(('-', r[i]));
            i += 1;
        } else {
            ops.push(('+', a[j]));
            j += 1;
        }
    }
    ops.extend(r[i..].iter().map(|l| ('-', *l)));
    ops.extend(a[j..].iter().map(|l| ('+', *l)));
    ops
}

// ---------------------------------------------------------------------------
// Replay

/// Replays recorded snapshots through a [`TestClient`]; made by
/// [`TestClient::replay_snapshots`]. Awaiting it panics with a diff for
/// every response that changed; [`report`](Self::report) returns the
/// outcome instead.
///
/// ```
/// # use zapline::prelude::*;
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// # let dir = tempfile::tempdir().unwrap();
/// # std::fs::write(dir.path().join("hello.json"), r#"{"zapline_snapshot": 1, "exchanges": [
/// #   {"request": {"method": "GET", "path": "/"}, "response": {"status": 200, "text": "hi"}}]}"#).unwrap();
/// # let snapshots = dir.path();
/// let client = App::new().get("/", || async { "hi" }).test();
/// client.replay_snapshots(snapshots).await;
/// # }
/// ```
#[must_use = "await the replay to run it"]
pub struct Replay {
    client: TestClient,
    path: PathBuf,
    headers: Vec<(HeaderName, HeaderValue)>,
    secrets: Vec<(String, String)>,
    update: bool,
}

/// The outcome of a [`Replay`], from [`Replay::report`]. Its `Display` is
/// the message an awaited replay panics with.
///
/// ```no_run
/// # async fn t(client: zapline::test::TestClient) {
/// let report = client.replay_snapshots("tests/snapshots").report().await;
/// assert_eq!(report.failures.len(), 0, "{report}");
/// println!("{} requests replayed from {} files", report.exchanges, report.files);
/// # }
/// ```
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct ReplayReport {
    /// Snapshot files replayed.
    pub files: usize,
    /// Requests sent.
    pub exchanges: usize,
    /// Exchanges rewritten in update mode.
    pub updated: usize,
    /// One readable message (with a diff) per mismatch or unreadable file.
    pub failures: Vec<String>,
}

impl ReplayReport {
    /// Whether every response matched (or was updated).
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

impl fmt::Display for ReplayReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.failures.is_empty() {
            write!(
                f,
                "{} snapshot file(s), {} request(s) replayed: all match",
                self.files, self.exchanges
            )?;
            if self.updated > 0 {
                write!(f, " ({} updated)", self.updated)?;
            }
            return Ok(());
        }
        writeln!(
            f,
            "{} of {} recorded response(s) changed:\n",
            self.failures.len(),
            self.exchanges
        )?;
        for failure in &self.failures {
            writeln!(f, "{failure}")?;
        }
        write!(
            f,
            "If the new responses are right, accept them with \
             {ENV_UPDATE_SNAPSHOTS}=1 cargo test"
        )
    }
}

impl TestClient {
    /// Replay the snapshots recorded from real traffic (see the
    /// [dev console](crate::dev)): every `*.json` file in `path` (or `path`
    /// itself, if it is a file), in name order, each with a fresh cookie jar
    /// like a new browser. Each response must have the recorded status and
    /// body, give or take timestamps, UUIDs, tokens and ids; ids the app
    /// hands out differently this time are substituted into the requests
    /// that follow.
    ///
    /// Awaiting panics with a diff when something changed, unless
    /// [`ENV_UPDATE_SNAPSHOTS`] is `1`: then the changed snapshots are
    /// rewritten with the new responses.
    ///
    /// ```no_run
    /// # use zapline::prelude::*;
    /// # fn app() -> App { App::new() }
    /// #[tokio::test]
    /// async fn recorded_traffic() {
    ///     app().test().replay_snapshots("tests/snapshots").await;
    /// }
    /// ```
    pub fn replay_snapshots(&self, path: impl AsRef<Path>) -> Replay {
        Replay {
            client: self.clone(),
            path: path.as_ref().to_path_buf(),
            headers: Vec::new(),
            secrets: Vec::new(),
            update: std::env::var_os(ENV_UPDATE_SNAPSHOTS).is_some_and(|v| v == "1"),
        }
    }
}

impl Replay {
    /// Send this header with every replayed request, replacing the recorded
    /// one. Credential headers are never recorded, so this is how a replay
    /// authenticates.
    ///
    /// ```no_run
    /// # use zapline::prelude::*;
    /// # async fn t(client: zapline::test::TestClient) {
    /// client
    ///     .replay_snapshots("tests/snapshots")
    ///     .header("authorization", "Bearer test-token")
    ///     .await;
    /// # }
    /// ```
    ///
    /// # Panics
    /// If the name or value is not a valid header.
    pub fn header<K, V>(mut self, name: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: fmt::Debug,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: fmt::Debug,
    {
        let name = HeaderName::try_from(name).expect("invalid header name");
        let value = HeaderValue::try_from(value).expect("invalid header value");
        self.headers.push((name, value));
        self
    }

    /// Fill in the redacted value of the body or query field `name` (a JSON
    /// key or form field) in every replayed request. Secret-looking fields
    /// (`password`, `*token*`, `*secret*`, CSRF fields, API keys) are
    /// recorded as `"[redacted]"`, so this is how a recorded login replays.
    ///
    /// ```no_run
    /// # async fn t(client: zapline::test::TestClient) {
    /// client
    ///     .replay_snapshots("tests/snapshots")
    ///     .secret("password", "test-password")
    ///     .await;
    /// # }
    /// ```
    pub fn secret(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets.push((name.into(), value.into()));
        self
    }

    /// Rewrite changed snapshots instead of failing (the default comes from
    /// [`ENV_UPDATE_SNAPSHOTS`]).
    ///
    /// ```no_run
    /// # async fn t(client: zapline::test::TestClient) {
    /// let report = client.replay_snapshots("tests/snapshots").update(true).report().await;
    /// println!("{report}");
    /// # }
    /// ```
    pub fn update(mut self, update: bool) -> Self {
        self.update = update;
        self
    }

    /// Run the replay and return what happened, without panicking.
    ///
    /// ```no_run
    /// # async fn t(client: zapline::test::TestClient) {
    /// let report = client.replay_snapshots("tests/snapshots").report().await;
    /// assert!(report.passed(), "{report}");
    /// # }
    /// ```
    pub async fn report(self) -> ReplayReport {
        let mut report = ReplayReport::default();
        let files = match snapshot_files(&self.path) {
            Ok(files) => files,
            Err(e) => {
                report.failures.push(e);
                return report;
            }
        };
        for file in files {
            report.files += 1;
            self.replay_file(&file, &mut report).await;
        }
        report
    }

    async fn replay_file(&self, file: &Path, report: &mut ReplayReport) {
        let shown = file.display();
        let snapshot = match std::fs::read_to_string(file)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<SnapshotFile>(&s).map_err(|e| e.to_string()))
        {
            Ok(s) if s.zapline_snapshot == VERSION => s,
            Ok(s) => {
                report.failures.push(format!(
                    "{shown}: unsupported snapshot version {}",
                    s.zapline_snapshot
                ));
                return;
            }
            Err(e) => {
                report.failures.push(format!("{shown}: cannot read: {e}"));
                return;
            }
        };
        // A new browser per file; the app and its data are shared.
        let client = self.client.clone().with_cookies();
        let mut ids = IdMap::default();
        let mut rewritten = snapshot.clone();
        let mut changed = 0;
        for (n, exchange) in snapshot.exchanges.iter().enumerate() {
            report.exchanges += 1;
            let request = exchange.request.rewritten(&ids);
            let actual = match self.send(&client, &request).await {
                Ok(a) => a,
                Err(e) => {
                    report.failures.push(format!(
                        "{shown} #{} {} {}: {e}\n",
                        n + 1,
                        request.method,
                        request.uri()
                    ));
                    continue;
                }
            };
            let problem = compare(&exchange.response, &actual, &mut ids);
            let fill_in =
                exchange.response.body_not_recorded.is_some() && actual.body_not_recorded.is_none();
            if self.update && (problem.is_some() || fill_in) {
                rewritten.exchanges[n] = Exchange {
                    request,
                    response: actual,
                };
                changed += 1;
            } else if let Some(problem) = problem {
                report.failures.push(format!(
                    "{shown} #{} {} {}\n{problem}",
                    n + 1,
                    request.method,
                    request.uri()
                ));
            }
        }
        if changed > 0 {
            match std::fs::write(file, rewritten.to_pretty()) {
                Ok(()) => report.updated += changed,
                Err(e) => report.failures.push(format!("{shown}: cannot update: {e}")),
            }
        }
    }

    async fn send(
        &self,
        client: &TestClient,
        req: &RecordedRequest,
    ) -> Result<RecordedResponse, String> {
        let req = &req.with_secrets(&self.secrets);
        let method = Method::from_bytes(req.method.as_bytes())
            .map_err(|_| format!("invalid method {:?}", req.method))?;
        let uri = req.uri();
        uri.parse::<Uri>()
            .map_err(|e| format!("invalid path {uri:?}: {e}"))?;
        let mut test = client.request(method, &uri);
        let headers = test
            .builder
            .headers_mut()
            .ok_or_else(|| "invalid request".to_owned())?;
        for (name, value) in &req.headers {
            if value == REDACTED {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::try_from(name.as_str()),
                HeaderValue::try_from(value.as_str()),
            ) {
                headers.append(n, v);
            }
        }
        for (name, value) in &self.headers {
            headers.insert(name.clone(), value.clone());
        }
        let res = test.body(Bytes::from(req.body.bytes())).send().await;
        Ok(RecordedResponse::new(
            res.status().as_u16(),
            res.headers(),
            Some(res.bytes()),
        ))
    }
}

impl IntoFuture for Replay {
    type Output = ReplayReport;
    type IntoFuture = Pin<Box<dyn Future<Output = ReplayReport> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let report = self.report().await;
            if !report.passed() {
                panic!("{report}");
            }
            report
        })
    }
}

/// The snapshot files at `path`: the file itself, or a directory's `*.json`
/// files by name.
fn snapshot_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let entries = std::fs::read_dir(path).map_err(|e| {
        format!(
            "cannot read the snapshot directory {}: {e}\n\
             hint: record traffic in the dev console (/_dev), then save it as a snapshot",
            path.display()
        )
    })?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    Ok(files)
}

// ---------------------------------------------------------------------------
// Writing snapshots into a project

/// What [`save`] wrote, relative to the project root.
#[derive(Debug, Serialize)]
pub(crate) struct Saved {
    pub(crate) file: String,
    pub(crate) exchanges: usize,
    /// `tests/snapshots.rs`, when this save created it.
    pub(crate) harness: Option<String>,
    /// What to do by hand, when the harness could not be written.
    pub(crate) note: Option<String>,
}

/// Whether `name` can be a snapshot file name: letters, digits, `-`, `_`.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// The nearest directory at or above the current one with a `Cargo.toml`
/// that has a `[package]` (the current directory if there is none).
pub(crate) fn project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = Some(cwd.as_path());
    while let Some(d) = dir {
        if std::fs::read_to_string(d.join("Cargo.toml"))
            .is_ok_and(|m| m.lines().any(|l| l.trim() == "[package]"))
        {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    cwd
}

/// Write `snapshot` to `<root>/tests/snapshots/<name>.json`, and
/// `tests/snapshots.rs` (which replays them) if the project has none yet.
pub(crate) fn save(root: &Path, name: &str, snapshot: &SnapshotFile) -> std::io::Result<Saved> {
    let dir = root.join("tests").join("snapshots");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{name}.json")), snapshot.to_pretty())?;
    let mut saved = Saved {
        file: format!("tests/snapshots/{name}.json"),
        exchanges: snapshot.exchanges.len(),
        harness: None,
        note: None,
    };
    let harness = root.join("tests").join("snapshots.rs");
    if harness.exists() {
        return Ok(saved);
    }
    match harness_source(root) {
        Ok(source) => {
            std::fs::write(&harness, source)?;
            saved.harness = Some("tests/snapshots.rs".into());
        }
        Err(note) => saved.note = Some(note),
    }
    Ok(saved)
}

/// The test file that replays `tests/snapshots/`, for the library crate at
/// `root` (whose `app` function builds the app).
fn harness_source(root: &Path) -> Result<String, String> {
    let by_hand = "add a test that calls \
                   `<your app>.test().replay_snapshots(\"tests/snapshots\").await`";
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|_| format!("no Cargo.toml: {by_hand}"))?;
    let lib = std::fs::read_to_string(root.join("src").join("lib.rs"))
        .map_err(|_| format!("no src/lib.rs to build the app from: {by_hand}"))?;
    let crate_name = manifest_name(&manifest, "lib")
        .or_else(|| manifest_name(&manifest, "package"))
        .ok_or_else(|| format!("no package name in Cargo.toml: {by_hand}"))?
        .replace('-', "_");
    let compact: String = lib.chars().filter(|c| !c.is_whitespace()).collect();
    let app = if compact.contains("pubfnapp()") {
        "app()"
    } else if compact.contains("pubfnapp(db:Database)") {
        "app(zapline::db::Database::sqlite_memory())"
    } else {
        return Err(format!("src/lib.rs has no `pub fn app()`: {by_hand}"));
    };
    Ok(format!(
        "//! Regression tests recorded from real traffic: every file in\n\
         //! tests/snapshots/ is replayed through the app in order, and each\n\
         //! response must keep its status and body (timestamps, UUIDs, tokens and\n\
         //! ids may differ). Record more in the dev console at /_dev, or with\n\
         //! `zapline dev --record` and `zapline snap <name>`.\n\
         //!\n\
         //! Accept intended changes: ZAPLINE_UPDATE_SNAPSHOTS=1 cargo test --test snapshots\n\
         \n\
         #[tokio::test]\n\
         async fn recorded_traffic() {{\n\
         \x20   {crate_name}::{app}\n\
         \x20       .test()\n\
         \x20       .replay_snapshots(\"tests/snapshots\")\n\
         \x20       .await;\n\
         }}\n"
    ))
}

/// `name = "..."` in the `[section]` of a manifest.
fn manifest_name(manifest: &str, section: &str) -> Option<String> {
    let header = format!("[{section}]");
    let mut inside = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == header;
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "name" {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                return (!value.is_empty()).then(|| value.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_volatile_values() {
        assert_eq!(
            classify("67e55044-10b1-426f-9247-bb680e5fe0c8"),
            Some(Volatile::Uuid)
        );
        for ts in [
            "2024-05-01T12:30:00Z",
            "2024-05-01T12:30:00.123456+02:00",
            "2024-05-01 12:30",
            "Thu, 01 Jan 1970 00:00:00 GMT",
        ] {
            assert_eq!(classify(ts), Some(Volatile::Timestamp), "{ts}");
        }
        for token in [
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl9x",
            "3f2a9c41b7e8d05f6a1c2b3d4e5f60718293a4b5",
            "kq7Zp2Lm9Xw4Rt8Vn3Bc6Yh1",
            "4631347ebf970956277678594225bd00",
            "tok4631347ebf970956A277678594225bd00",
        ] {
            assert_eq!(classify(token), Some(Volatile::Token), "{token}");
        }
        for plain in [
            "2024-05-01",
            "Ada Lovelace",
            "my-very-long-article-title-2024",
            "ada@example.com",
            "hello_world_this_is_long",
        ] {
            assert_eq!(classify(plain), None, "{plain}");
        }
    }

    #[test]
    fn normalizes_text() {
        assert_eq!(
            normalize_text(
                "<p id=\"67e55044-10b1-426f-9247-bb680e5fe0c8\">at 2024-05-01T12:30:00Z</p>\
                 <link href=\"/static/app.3f2a9c41b7e8d05f6a1c.css\">"
            ),
            "<p id=\"[uuid]\">at [timestamp]</p><link href=\"/static/[token]\">"
        );
        assert_eq!(normalize_text("nothing here"), "nothing here");
        assert_eq!(
            normalize_text(r#"<link href="/static/app.73cd192d.css"> v1.2.3 a1b2c3d4e5"#),
            r#"<link href="/static/app.[hash].css"> v1.2.3 a1b2c3d4e5"#
        );
    }

    #[test]
    fn ids_are_learned_and_rewritten() {
        let mut ids = IdMap::default();
        let (r, a) = pair(
            &json!({"id": 7, "owner_id": "u-1", "name": "Ada", "createdAt": 1}),
            &json!({"id": 1, "owner_id": "u-1", "name": "Ada", "createdAt": 2}),
            None,
            &mut ids,
        );
        assert_eq!(r, a);
        assert_eq!(r["id"], "[id]");
        assert_eq!(r["owner_id"], "u-1");
        assert_eq!(r["createdAt"], "[timestamp]");
        // Generated values in the same place match even when one alone
        // would not look random enough.
        let (r, a) = pair(
            &json!({"key": "tok4631347ebf970956A2776785942"}),
            &json!({"key": "tokA1b2c3d4e5f6a7b8c9d0e1f2a3b"}),
            None,
            &mut ids,
        );
        assert_eq!(
            (&r, &a),
            (&json!({"key": "[token]"}), &json!({"key": "[token]"}))
        );
        let (r, _) = pair(
            &json!("Ada Lovelace 1815"),
            &json!("Ada Lovelace 1816"),
            None,
            &mut ids,
        );
        assert_eq!(r, "Ada Lovelace 1815");
        for (x, y) in [
            ("release-2024-final-x", "release-2025-final-x"),
            (
                "summary-for-week-one-of-2024",
                "summary-for-week-two-of-2024",
            ),
        ] {
            let (r, a) = pair(&json!(x), &json!(y), None, &mut ids);
            assert_ne!(r, a, "{x} vs {y}");
        }
        assert_eq!(ids.rewrite_text("/users/7/posts"), "/users/1/posts");
        // A bare 7 is a count or a page more likely than the id.
        assert_eq!(
            ids.rewrite_text("7 items, see /users/7"),
            "7 items, see /users/1"
        );
        assert_eq!(
            ids.rewrite_form("user_id=7&page=7&next=%2Fusers%2F7&to=/users/7"),
            "user_id=1&page=7&next=%2Fusers%2F7&to=/users/1"
        );
        let mut body = json!({"user_id": 7, "count": 7, "path": "/users/7", "n": 70});
        ids.rewrite_json(&mut body, None);
        assert_eq!(
            body,
            json!({"user_id": 1, "count": 7, "path": "/users/1", "n": 70})
        );
        // Long ids are replaced anywhere.
        ids.learn(&json!("u-12345678"), &json!("u-87654321"));
        assert_eq!(ids.rewrite_text("owner u-12345678."), "owner u-87654321.");
    }

    #[test]
    fn diffs_show_changed_lines_with_context() {
        let d = diff("a\nb\nc\nd\ne\nf\ng", "a\nb\nc\nD\ne\nf\ng");
        assert_eq!(d, "      b\n      c\n    - d\n    + D\n      e\n      f\n");
        let d = diff(
            &format!("x\n{}", "s\n".repeat(10)),
            &format!("{}y", "s\n".repeat(10)),
        );
        assert!(d.starts_with("    - x\n      s\n      s\n    ...\n"), "{d}");
        assert!(d.ends_with("      s\n    + y\n"), "{d}");
    }

    #[test]
    fn payloads_round_trip() {
        let j = Payload::capture(Some("application/json; charset=utf-8"), br#"{"a":1}"#);
        assert_eq!(j.json, Some(json!({"a": 1})));
        assert_eq!(j.bytes(), br#"{"a":1}"#);
        let t = Payload::capture(Some("text/html"), b"<p>");
        assert_eq!(t.text.as_deref(), Some("<p>"));
        let b = Payload::capture(None, &[0xff, 0x00]);
        assert_eq!(b.bytes(), [0xff, 0x00]);
        assert_eq!(Payload::capture(None, b""), Payload::default());
        // Invalid JSON is kept as text.
        let bad = Payload::capture(Some("application/json"), b"{nope");
        assert_eq!(bad.text.as_deref(), Some("{nope"));
    }

    #[test]
    fn manifest_names() {
        let m = "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[lib]\nname = \"core_lib\"\n";
        assert_eq!(manifest_name(m, "package").as_deref(), Some("my-app"));
        assert_eq!(manifest_name(m, "lib").as_deref(), Some("core_lib"));
        assert_eq!(manifest_name("[package]\n", "lib"), None);
        assert!(valid_name("sign-up_2"));
        assert!(!valid_name("../x") && !valid_name("") && !valid_name("a b"));
    }
}
