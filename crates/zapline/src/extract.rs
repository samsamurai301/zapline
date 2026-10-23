//! Extractors: typed values pulled out of a request by handler arguments.
//!
//! Every handler argument except the last implements [`FromRequestParts`]
//! (headers, path, query, state...). The last one may also consume the body
//! via [`FromRequest`] (`Json`, `Form`, `Bytes`, `String`).
//!
//! # Rejections
//!
//! When the request does not match the expected type, the built-in
//! extractors reject it with a structured [`Error`] whose `code` says what
//! went wrong:
//!
//! | Extractor | Status | `code` | `details` (`json` feature) |
//! |---|---|---|---|
//! | [`Path`] | 400 | `invalid_path` | `{"param": "id"}` when one parameter is at fault |
//! | [`Query`] | 400 | `invalid_query` | |
//! | [`Form`] | 422 | `invalid_form` | |
//! | [`Json`] | 422 | `invalid_json`: malformed JSON | `{"line": 1, "column": 9}` |
//! | [`Json`] | 422 | `invalid_body`: valid JSON, wrong shape | `{"line": 1, "column": 9, "path": "items[0].qty"}` |
//! | [`Json`], [`Form`] | 415 | `unsupported_media_type` | |
//!
//! ```text
//! POST /orders  {"items": [{"qty": "x"}]}
//!
//! HTTP/1.1 422 Unprocessable Entity
//! {"error":"invalid request body: invalid type: string \"x\", expected u32",
//!  "code":"invalid_body","details":{"column":22,"line":1,"path":"items[0].qty"}}
//! ```
//!
//! `line` (1-based) and `column` (in bytes) are where the parser stopped.
//! `path` is where the problem was found: the offending value, or for a
//! missing field, the object that lacks it (the message names the field). It
//! is omitted at the top level.

use std::future::Future;
use std::ops::{Deref, DerefMut};

use bytes::Bytes;
use http::{HeaderMap, Method, Uri};
use serde::de::DeserializeOwned;

use crate::request::Request;
use crate::Error;

/// Extract a value without consuming the body.
pub trait FromRequestParts: Sized + Send {
    fn from_request_parts(req: &mut Request) -> impl Future<Output = Result<Self, Error>> + Send;
}

pub mod private {
    pub enum ViaParts {}
    pub enum ViaRequest {}
}

/// Extract a value, possibly consuming the body. Only valid as the last
/// handler argument. The marker parameter lets every `FromRequestParts` type
/// be used in last position too.
pub trait FromRequest<M = private::ViaRequest>: Sized + Send {
    fn from_request(req: Request) -> impl Future<Output = Result<Self, Error>> + Send;
}

impl<T: FromRequestParts> FromRequest<private::ViaParts> for T {
    #[inline]
    async fn from_request(mut req: Request) -> Result<Self, Error> {
        T::from_request_parts(&mut req).await
    }
}

impl FromRequest for Request {
    #[inline]
    async fn from_request(req: Request) -> Result<Self, Error> {
        Ok(req)
    }
}

// ---------------------------------------------------------------------------
// Request metadata

impl FromRequestParts for Method {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        Ok(req.head.method.clone())
    }
}

impl FromRequestParts for Uri {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        Ok(req.head.uri.clone())
    }
}

impl FromRequestParts for HeaderMap {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        Ok(req.head.headers.clone())
    }
}

impl FromRequestParts for crate::request::ConnInfo {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        Ok(req.conn)
    }
}

/// The matched route pattern, e.g. `/users/{id}`. Fails with 500 when used
/// outside a matched route (e.g. in a fallback handler).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchedPath(pub &'static str);

impl MatchedPath {
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl FromRequestParts for MatchedPath {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        req.matched_path().map(MatchedPath).ok_or_else(|| {
            Error::internal("no matched route for this request").diagnosed("ZL0013", None)
        })
    }
}

/// Optional extraction never fails: `None` when the inner extractor errors.
impl<T: FromRequestParts> FromRequestParts for Option<T> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        Ok(T::from_request_parts(req).await.ok())
    }
}

/// Get the extraction error yourself instead of the default error response.
impl<T: FromRequestParts> FromRequestParts for Result<T, Error> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        Ok(T::from_request_parts(req).await)
    }
}

macro_rules! newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct $name<T>(pub T);

        impl<T> Deref for $name<T> {
            type Target = T;
            #[inline]
            fn deref(&self) -> &T { &self.0 }
        }
        impl<T> DerefMut for $name<T> {
            #[inline]
            fn deref_mut(&mut self) -> &mut T { &mut self.0 }
        }
        impl<T> $name<T> {
            #[inline]
            pub fn into_inner(self) -> T { self.0 }
        }
    };
}

// ---------------------------------------------------------------------------
// State & extensions

newtype!(
    /// Application state registered with `App::state`. `T` is cloned per
    /// request, so wrap large or mutable data in an `Arc`.
    State
);

impl<T: Clone + Send + Sync + 'static> FromRequestParts for State<T> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        match req.shared.state::<T>() {
            Some(v) => Ok(State(v.clone())),
            None => {
                let name = std::any::type_name::<T>();
                Err(
                    Error::internal(format!("state `{name}` was not registered with App::state"))
                        .diagnosed("ZL0001", Some(name.to_owned())),
                )
            }
        }
    }
}

newtype!(
    /// A value inserted into request extensions, typically by middleware.
    Extension
);

impl<T: Clone + Send + Sync + 'static> FromRequestParts for Extension<T> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        match req.head.extensions.get::<T>() {
            Some(v) => Ok(Extension(v.clone())),
            None => {
                let name = std::any::type_name::<T>();
                Err(
                    Error::internal(format!("extension `{name}` missing from request"))
                        .diagnosed("ZL0005", Some(name.to_owned())),
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Path

newtype!(
    /// Route parameters. `T` can be a single value (`Path<u64>`), a tuple
    /// (`Path<(String, u32)>`) or a struct whose fields match the names.
    ///
    /// Rejects with `400 invalid_path`, naming the parameter at fault in
    /// `details` (see [rejections](self#rejections)).
    Path
);

impl<T: DeserializeOwned + Send> FromRequestParts for Path<T> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        let params = path_de::ParamsDe {
            len: req.params.len(),
            iter: req.param_values(),
        };
        T::deserialize(params).map(Path).map_err(path_error)
    }
}

#[cold]
fn path_error(e: path_de::DeError) -> Error {
    let err = Error::bad_request(format!("invalid path parameter: {e}"))
        .with_code("invalid_path")
        .diagnosed("ZL0060", None);
    #[cfg(feature = "json")]
    if let Some(param) = e.param {
        return err.with_details(serde_json::json!({ "param": param }));
    }
    err
}

// ---------------------------------------------------------------------------
// Query

#[cfg(feature = "query")]
newtype!(
    /// URL query string, deserialized with `serde_urlencoded`.
    ///
    /// Rejects with `400 invalid_query` (see [rejections](self#rejections)).
    Query
);

#[cfg(feature = "query")]
impl<T: DeserializeOwned + Send> FromRequestParts for Query<T> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        let q = req.head.uri.query().unwrap_or("");
        serde_urlencoded::from_str(q).map(Query).map_err(|e| {
            Error::bad_request(format!("invalid query string: {e}"))
                .with_code("invalid_query")
                .diagnosed("ZL0061", None)
        })
    }
}

// ---------------------------------------------------------------------------
// Bodies

async fn take_body(mut req: Request) -> Result<Bytes, Error> {
    req.body_bytes().await
}

impl FromRequest for Bytes {
    #[inline]
    async fn from_request(req: Request) -> Result<Self, Error> {
        take_body(req).await
    }
}

impl FromRequest for String {
    async fn from_request(req: Request) -> Result<Self, Error> {
        let b = take_body(req).await?;
        String::from_utf8(b.into())
            .map_err(|_| Error::bad_request("request body is not valid UTF-8"))
    }
}

#[cfg(any(feature = "json", feature = "form"))]
fn content_type_is(req: &Request, accept: fn(&str) -> bool) -> bool {
    req.head
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| accept(mime_essence(v).as_str()))
}

/// The lowercased `type/subtype` of a Content-Type, without parameters.
/// Matching on the essence (not a substring) matters for CSRF: browsers send
/// `text/plain; x=json` cross-origin without a CORS preflight.
#[cfg(any(feature = "json", feature = "form"))]
fn mime_essence(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

#[cfg(feature = "json")]
fn is_json(essence: &str) -> bool {
    essence == "application/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

#[cfg(feature = "form")]
fn is_form(essence: &str) -> bool {
    essence == "application/x-www-form-urlencoded"
}

#[cfg(any(feature = "json", feature = "form"))]
#[cold]
fn unsupported_media_type(expected: &'static str) -> Error {
    Error::unsupported_media_type(expected)
        .with_code("unsupported_media_type")
        .diagnosed("ZL0064", None)
}

#[cfg(feature = "json")]
newtype!(
    /// JSON request body (as an argument) or JSON response (as a return value).
    ///
    /// Rejects with `415 unsupported_media_type` without a JSON content type,
    /// and `422 invalid_json` / `422 invalid_body` with the error's line,
    /// column and path in `details` (see [rejections](self#rejections)).
    Json
);

#[cfg(feature = "json")]
impl<T: DeserializeOwned + Send> FromRequest for Json<T> {
    async fn from_request(req: Request) -> Result<Self, Error> {
        if !content_type_is(&req, is_json) {
            return Err(unsupported_media_type(
                "expected `Content-Type: application/json`",
            ));
        }
        let b = take_body(req).await?;
        serde_json::from_slice(&b)
            .map(Json)
            .map_err(|e| json_error(&e, &b))
    }
}

/// The `422` for a JSON body that failed to parse (`invalid_json`) or did not
/// match `T` (`invalid_body`), located by line, column and path.
#[cfg(feature = "json")]
#[cold]
fn json_error(e: &serde_json::Error, body: &[u8]) -> Error {
    let (code, what, diagnostic) = if e.is_data() {
        ("invalid_body", "invalid request body", "ZL0063")
    } else {
        ("invalid_json", "invalid JSON body", "ZL0062")
    };
    let (line, column) = (e.line(), e.column());
    // serde_json appends the position; it goes into `details` instead.
    let msg = e.to_string();
    let msg = msg
        .strip_suffix(&format!(" at line {line} column {column}"))
        .unwrap_or(&msg);
    let err = Error::unprocessable(format!("{what}: {msg}"))
        .with_code(code)
        .diagnosed(diagnostic, None);
    if line == 0 {
        return err;
    }
    let mut details = serde_json::Map::new();
    details.insert("line".into(), line.into());
    details.insert("column".into(), column.into());
    if e.is_data() {
        let path = json_path::locate(body, line, column);
        if !path.is_empty() {
            details.insert("path".into(), path.into());
        }
    }
    err.with_details(details)
}

#[cfg(feature = "json")]
impl<T: serde::Serialize> crate::IntoResponse for Json<T> {
    #[inline]
    fn into_response(self) -> crate::Response {
        let mut buf = Vec::with_capacity(128);
        match serde_json::to_writer(&mut buf, &self.0) {
            Ok(()) => {
                let mut r = crate::Response::new(buf.into());
                r.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                r
            }
            Err(e) => Error::internal(format!("failed to serialize JSON: {e}")).into_response(),
        }
    }
}

#[cfg(feature = "form")]
newtype!(
    /// `application/x-www-form-urlencoded` request body.
    ///
    /// Rejects with `415 unsupported_media_type` or `422 invalid_form` (see
    /// [rejections](self#rejections)).
    Form
);

#[cfg(feature = "form")]
impl<T: DeserializeOwned + Send> FromRequest for Form<T> {
    async fn from_request(req: Request) -> Result<Self, Error> {
        if !content_type_is(&req, is_form) {
            return Err(unsupported_media_type(
                "expected `Content-Type: application/x-www-form-urlencoded`",
            ));
        }
        let b = take_body(req).await?;
        serde_urlencoded::from_bytes(&b).map(Form).map_err(|e| {
            Error::unprocessable(format!("invalid form body: {e}"))
                .with_code("invalid_form")
                .diagnosed("ZL0065", None)
        })
    }
}

/// Serde helper for HTML forms: an empty field becomes `None` instead of a
/// parse error (browsers send `age=` for an empty number input). Works for
/// JSON bodies too (`null`, numbers, strings), so one struct can serve both.
///
/// ```
/// # use serde::Deserialize;
/// #[derive(Deserialize)]
/// struct Profile {
///     #[serde(default, deserialize_with = "zapline::extract::empty_as_none")]
///     age: Option<u32>,
/// }
/// ```
#[cfg(feature = "form")]
pub fn empty_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    deserializer.deserialize_any(form_helpers::Loose(std::marker::PhantomData))
}

/// Serde helper for HTML checkboxes: present (`on`, `true`, `1`, `yes`)
/// means `true`, absent or empty means `false`. Also accepts JSON booleans.
/// Pair it with `#[serde(default)]` so a missing (unchecked) box is `false`.
#[cfg(feature = "form")]
pub fn checkbox<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_any(form_helpers::Checkbox)
}

/// Serde helper for `<input type="datetime-local">`: accepts what browsers
/// send (`2026-01-31T09:30`, read as UTC) as well as RFC 3339
/// (`2026-01-31T09:30:00Z`, `2026-01-31T10:30:00+01:00`), so one struct can
/// take both forms and JSON.
///
/// ```
/// # use serde::Deserialize;
/// use zapline::chrono::{DateTime, Utc};
///
/// #[derive(Deserialize)]
/// struct Meeting {
///     #[serde(deserialize_with = "zapline::extract::datetime")]
///     starts_at: DateTime<Utc>,
/// }
///
/// let m: Meeting = serde_json::from_str(r#"{"starts_at": "2026-01-31T09:30"}"#).unwrap();
/// assert_eq!(m.starts_at.to_rfc3339(), "2026-01-31T09:30:00+00:00");
/// ```
#[cfg(all(feature = "form", feature = "chrono"))]
pub fn datetime<'de, D>(deserializer: D) -> Result<chrono::DateTime<chrono::Utc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match optional_datetime(deserializer)? {
        Some(t) => Ok(t),
        None => Err(serde::de::Error::custom("a date and time is required")),
    }
}

/// [`datetime`] for an optional field: empty or `null` is `None`. Pair it
/// with `#[serde(default)]` so a missing field is `None` too.
///
/// ```
/// # use serde::Deserialize;
/// use zapline::chrono::{DateTime, Utc};
///
/// #[derive(Deserialize)]
/// struct Task {
///     #[serde(default, deserialize_with = "zapline::extract::optional_datetime")]
///     due_at: Option<DateTime<Utc>>,
/// }
///
/// let t: Task = serde_json::from_str(r#"{"due_at": ""}"#).unwrap();
/// assert!(t.due_at.is_none());
/// ```
#[cfg(all(feature = "form", feature = "chrono"))]
pub fn optional_datetime<'de, D>(
    deserializer: D,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let loose = deserializer.deserialize_any(
        form_helpers::Loose::<form_helpers::LocalDateTime>(std::marker::PhantomData),
    )?;
    Ok(loose.map(|t| t.0))
}

#[cfg(feature = "form")]
mod form_helpers {
    use std::fmt;
    use std::marker::PhantomData;
    use std::str::FromStr;

    use serde::de::{self, Deserializer, Visitor};

    pub(super) struct Loose<T>(pub(super) PhantomData<T>);

    impl<T: FromStr> Loose<T>
    where
        T::Err: fmt::Display,
    {
        fn parse<E: de::Error>(s: &str) -> Result<Option<T>, E> {
            match s.trim() {
                "" => Ok(None),
                s => s.parse().map(Some).map_err(E::custom),
            }
        }
    }

    impl<'de, T: FromStr> Visitor<'de> for Loose<T>
    where
        T::Err: fmt::Display,
    {
        type Value = Option<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a value or an empty string")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Self::parse(v)
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Self::parse(if v { "true" } else { "false" })
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Self::parse(&v.to_string())
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Self::parse(&v.to_string())
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Self::parse(&v.to_string())
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_any(self)
        }
    }

    /// A UTC time parsed from RFC 3339 or a `datetime-local` value.
    #[cfg(feature = "chrono")]
    pub(super) struct LocalDateTime(pub(super) chrono::DateTime<chrono::Utc>);

    #[cfg(feature = "chrono")]
    impl FromStr for LocalDateTime {
        type Err = String;
        fn from_str(s: &str) -> Result<Self, String> {
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
                return Ok(LocalDateTime(t.with_timezone(&chrono::Utc)));
            }
            for format in [
                "%Y-%m-%dT%H:%M:%S%.f",
                "%Y-%m-%dT%H:%M",
                "%Y-%m-%d %H:%M:%S%.f",
                "%Y-%m-%d %H:%M",
            ] {
                if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, format) {
                    return Ok(LocalDateTime(t.and_utc()));
                }
            }
            Err(format!(
                "`{s}` is not a date and time (expected e.g. `2026-01-31T09:30` or `2026-01-31T09:30:00Z`)"
            ))
        }
    }

    pub(super) struct Checkbox;

    impl<'de> Visitor<'de> for Checkbox {
        type Value = bool;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a boolean or a checkbox value")
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
            Ok(matches!(v.trim(), "on" | "true" | "1" | "yes"))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<bool, E> {
            Ok(v != 0)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<bool, E> {
            Ok(v != 0)
        }
        fn visit_none<E: de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
            d.deserialize_any(self)
        }
    }
}

// ---------------------------------------------------------------------------
// A small serde deserializer for route parameters.

mod path_de {
    use std::borrow::Cow;
    use std::fmt;

    use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};
    use serde::forward_to_deserialize_any;

    #[derive(Debug)]
    pub struct DeError {
        msg: String,
        /// The parameter being deserialized when the error happened.
        pub param: Option<&'static str>,
    }

    impl DeError {
        fn new(msg: String) -> Self {
            DeError { msg, param: None }
        }

        /// Blame `name` unless a parameter is already blamed.
        fn at(mut self, name: &'static str) -> Self {
            self.param.get_or_insert(name);
            self
        }
    }

    impl fmt::Display for DeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.msg)
        }
    }
    impl std::error::Error for DeError {}
    impl de::Error for DeError {
        fn custom<T: fmt::Display>(msg: T) -> Self {
            DeError::new(msg.to_string())
        }
    }

    /// Route parameters, decoded as they are visited: `len` of them in `iter`.
    pub struct ParamsDe<I> {
        pub len: usize,
        pub iter: I,
    }

    impl<'a, I: Iterator<Item = (&'static str, Cow<'a, str>)>> ParamsDe<I> {
        fn single(mut self) -> Result<ValueDe<'a>, DeError> {
            let n = self.len;
            match self.iter.next() {
                Some((name, v)) if n == 1 => Ok(ValueDe { name, value: v }),
                _ => Err(DeError::new(format!(
                    "expected 1 path parameter, route has {n}"
                ))),
            }
        }
    }

    macro_rules! single {
        ($($m:ident)*) => {$(
            fn $m<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
                self.single()?.$m(v)
            }
        )*};
    }

    impl<'de, 'a, I> de::Deserializer<'de> for ParamsDe<I>
    where
        I: Iterator<Item = (&'static str, Cow<'a, str>)>,
    {
        type Error = DeError;

        fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            if self.len == 1 {
                self.single()?.deserialize_any(v)
            } else {
                self.deserialize_map(v)
            }
        }
        fn deserialize_map<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            v.visit_map(Map {
                iter: self.iter,
                value: None,
            })
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            _: &'static [&'static str],
            v: V,
        ) -> Result<V::Value, DeError> {
            self.deserialize_map(v)
        }
        fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            v.visit_seq(Seq { iter: self.iter })
        }
        fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, v: V) -> Result<V::Value, DeError> {
            if self.len != len {
                return Err(DeError::new(format!(
                    "expected {len} path parameters, route has {}",
                    self.len
                )));
            }
            self.deserialize_seq(v)
        }
        fn deserialize_tuple_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            len: usize,
            v: V,
        ) -> Result<V::Value, DeError> {
            self.deserialize_tuple(len, v)
        }
        fn deserialize_newtype_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            v: V,
        ) -> Result<V::Value, DeError> {
            v.visit_newtype_struct(self)
        }
        fn deserialize_enum<V: Visitor<'de>>(
            self,
            name: &'static str,
            variants: &'static [&'static str],
            v: V,
        ) -> Result<V::Value, DeError> {
            self.single()?.deserialize_enum(name, variants, v)
        }
        fn deserialize_unit_struct<V: Visitor<'de>>(
            self,
            name: &'static str,
            v: V,
        ) -> Result<V::Value, DeError> {
            self.single()?.deserialize_unit_struct(name, v)
        }

        single! {
            deserialize_bool deserialize_i8 deserialize_i16 deserialize_i32 deserialize_i64
            deserialize_i128 deserialize_u8 deserialize_u16 deserialize_u32 deserialize_u64
            deserialize_u128 deserialize_f32 deserialize_f64 deserialize_char deserialize_str
            deserialize_string deserialize_bytes deserialize_byte_buf deserialize_option
            deserialize_unit deserialize_identifier deserialize_ignored_any
        }
    }

    struct Map<'a, I> {
        iter: I,
        value: Option<(&'static str, Cow<'a, str>)>,
    }

    impl<'de, 'a, I> MapAccess<'de> for Map<'a, I>
    where
        I: Iterator<Item = (&'static str, Cow<'a, str>)>,
    {
        type Error = DeError;
        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, DeError> {
            match self.iter.next() {
                Some((name, value)) => {
                    self.value = Some((name, value));
                    seed.deserialize(name.into_deserializer()).map(Some)
                }
                None => Ok(None),
            }
        }
        fn next_value_seed<V: DeserializeSeed<'de>>(
            &mut self,
            seed: V,
        ) -> Result<V::Value, DeError> {
            let (name, value) = self.value.take().expect("value requested before key");
            seed.deserialize(ValueDe { name, value })
        }
    }

    struct Seq<I> {
        iter: I,
    }

    impl<'de, 'a, I> SeqAccess<'de> for Seq<I>
    where
        I: Iterator<Item = (&'static str, Cow<'a, str>)>,
    {
        type Error = DeError;
        fn next_element_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<Option<T::Value>, DeError> {
            match self.iter.next() {
                Some((name, value)) => seed.deserialize(ValueDe { name, value }).map(Some),
                None => Ok(None),
            }
        }
    }

    struct ValueDe<'a> {
        name: &'static str,
        value: Cow<'a, str>,
    }

    macro_rules! parse {
        ($($m:ident => $visit:ident),* $(,)?) => {$(
            fn $m<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
                match self.value.parse() {
                    Ok(x) => v.$visit(x).map_err(|e: DeError| e.at(self.name)),
                    Err(_) => Err(DeError::new(format!(
                        "`{}` = {:?} is not a valid {}",
                        self.name, self.value, stringify!($visit).trim_start_matches("visit_")
                    ))
                    .at(self.name)),
                }
            }
        )*};
    }

    impl<'de, 'a> de::Deserializer<'de> for ValueDe<'a> {
        type Error = DeError;

        fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            v.visit_str(&self.value)
                .map_err(|e: DeError| e.at(self.name))
        }
        fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            let name = self.name;
            v.visit_some(self).map_err(|e: DeError| e.at(name))
        }
        fn deserialize_newtype_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            v: V,
        ) -> Result<V::Value, DeError> {
            let name = self.name;
            v.visit_newtype_struct(self)
                .map_err(|e: DeError| e.at(name))
        }
        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _: &'static str,
            _: &'static [&'static str],
            v: V,
        ) -> Result<V::Value, DeError> {
            let s: &str = &self.value;
            v.visit_enum(s.into_deserializer())
                .map_err(|e: DeError| e.at(self.name))
        }

        parse! {
            deserialize_bool => visit_bool,
            deserialize_i8 => visit_i8, deserialize_i16 => visit_i16,
            deserialize_i32 => visit_i32, deserialize_i64 => visit_i64,
            deserialize_i128 => visit_i128,
            deserialize_u8 => visit_u8, deserialize_u16 => visit_u16,
            deserialize_u32 => visit_u32, deserialize_u64 => visit_u64,
            deserialize_u128 => visit_u128,
            deserialize_f32 => visit_f32, deserialize_f64 => visit_f64,
            deserialize_char => visit_char,
        }

        forward_to_deserialize_any! {
            str string bytes byte_buf unit unit_struct seq tuple tuple_struct
            map struct identifier ignored_any
        }
    }
}

// ---------------------------------------------------------------------------
// Locating JSON data errors.

/// Turns a serde_json error position into a path like `items[2].price`.
///
/// Runs only after deserialization failed: it replays the document's
/// structure up to the error offset, so the success path pays nothing.
#[cfg(feature = "json")]
mod json_path {
    use std::fmt::Write;

    enum Frame {
        /// The current key, and whether the next string is a key.
        Object(Option<String>, bool),
        Array(usize),
    }

    /// The path of the value being read at serde_json's 1-based `line` and
    /// byte `column` (the position just after the offending token).
    pub(super) fn locate(input: &[u8], line: usize, column: usize) -> String {
        let line_start = if line <= 1 {
            0
        } else {
            input
                .iter()
                .enumerate()
                .filter(|&(_, &b)| b == b'\n')
                .nth(line - 2)
                .map_or(input.len(), |(i, _)| i + 1)
        };
        let end = (line_start + column).min(input.len());
        path_at(&input[..end])
    }

    fn path_at(input: &[u8]) -> String {
        let mut stack = Vec::new();
        let mut i = 0;
        while let Some(&b) = input.get(i) {
            i += 1;
            match b {
                b'"' => {
                    let start = i - 1;
                    while let Some(&c) = input.get(i) {
                        i += 1;
                        match c {
                            b'\\' => i += 1,
                            b'"' => break,
                            _ => {}
                        }
                    }
                    if let Some(Frame::Object(key, expect_key @ true)) = stack.last_mut() {
                        *key = Some(decode_key(&input[start..i.min(input.len())]));
                        *expect_key = false;
                    }
                }
                b'{' => stack.push(Frame::Object(None, true)),
                b'[' => stack.push(Frame::Array(0)),
                b'}' | b']' => {
                    stack.pop();
                }
                b',' => match stack.last_mut() {
                    Some(Frame::Object(key, expect_key)) => {
                        *key = None;
                        *expect_key = true;
                    }
                    Some(Frame::Array(index)) => *index += 1,
                    None => {}
                },
                _ => {}
            }
        }
        let mut path = String::new();
        for frame in &stack {
            match frame {
                Frame::Object(Some(key), _) => {
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(key);
                }
                Frame::Object(None, _) => {}
                Frame::Array(index) => {
                    let _ = write!(path, "[{index}]");
                }
            }
        }
        path
    }

    /// Unescape a (possibly truncated) JSON string token.
    fn decode_key(raw: &[u8]) -> String {
        serde_json::from_slice(raw).unwrap_or_else(|_| {
            let inner = raw.strip_prefix(b"\"").unwrap_or(raw);
            let inner = inner.strip_suffix(b"\"").unwrap_or(inner);
            String::from_utf8_lossy(inner).into_owned()
        })
    }

    #[cfg(test)]
    mod tests {
        use super::{locate, path_at};

        #[test]
        fn paths() {
            assert_eq!(path_at(br#"{"a":1"#), "a");
            assert_eq!(path_at(br#"{"a":{"b":[1,{"c":"x""#), "a.b[1].c");
            assert_eq!(path_at(br#"{"a":{"b":1},"c":"#), "c");
            // A closed object reports its parent's key.
            assert_eq!(path_at(br#"{"user":{"id":1}"#), "user");
            assert_eq!(path_at(br#"[{"id":1},{"id":"x""#), "[1].id");
            // Brackets, quotes and commas inside strings are not structure.
            assert_eq!(path_at(br#"{"k,[{":"v\",]}","n":"#), "n");
            assert_eq!(path_at(br#"{"a\"b":"#), "a\"b");
            assert_eq!(path_at(br#"{"#), "");
            assert_eq!(path_at(b""), "");
        }

        #[test]
        fn lines_and_columns() {
            let doc = b"{\n  \"a\": 1,\n  \"b\": \"x\"\n}";
            // Line 3, column 10 is just after `"x"`.
            assert_eq!(locate(doc, 3, 10), "b");
            assert_eq!(locate(doc, 2, 8), "a");
            // Out-of-range positions are clamped.
            assert_eq!(locate(doc, 9, 99), "");
        }
    }
}
