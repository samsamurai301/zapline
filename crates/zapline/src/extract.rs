//! Extractors: typed values pulled out of a request by handler arguments.
//!
//! Every handler argument except the last implements [`FromRequestParts`]
//! (headers, path, query, state...). The last one may also consume the body
//! via [`FromRequest`] (`Json`, `Form`, `Bytes`, `String`).

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
            None => Err(Error::internal(format!(
                "state `{}` was not registered with App::state",
                std::any::type_name::<T>()
            ))),
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
            None => Err(Error::internal(format!(
                "extension `{}` missing from request",
                std::any::type_name::<T>()
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Path

newtype!(
    /// Route parameters. `T` can be a single value (`Path<u64>`), a tuple
    /// (`Path<(String, u32)>`) or a struct whose fields match the names.
    Path
);

impl<T: DeserializeOwned + Send> FromRequestParts for Path<T> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        let mut params = Vec::with_capacity(req.params.len());
        params.extend(req.param_values());
        T::deserialize(path_de::ParamsDe { params })
            .map(Path)
            .map_err(|e| Error::bad_request(format!("invalid path parameter: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Query

#[cfg(feature = "query")]
newtype!(
    /// URL query string, deserialized with `serde_urlencoded`.
    Query
);

#[cfg(feature = "query")]
impl<T: DeserializeOwned + Send> FromRequestParts for Query<T> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        let q = req.head.uri.query().unwrap_or("");
        serde_urlencoded::from_str(q)
            .map(Query)
            .map_err(|e| Error::bad_request(format!("invalid query string: {e}")))
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
fn content_type_is(req: &Request, needle: &str) -> bool {
    req.head
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains(needle))
}

#[cfg(feature = "json")]
newtype!(
    /// JSON request body (as an argument) or JSON response (as a return value).
    Json
);

#[cfg(feature = "json")]
impl<T: DeserializeOwned + Send> FromRequest for Json<T> {
    async fn from_request(req: Request) -> Result<Self, Error> {
        if !content_type_is(&req, "json") {
            return Err(Error::new(
                http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected `Content-Type: application/json`",
            ));
        }
        let b = take_body(req).await?;
        serde_json::from_slice(&b)
            .map(Json)
            .map_err(|e| Error::unprocessable(format!("invalid JSON body: {e}")))
    }
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
    Form
);

#[cfg(feature = "form")]
impl<T: DeserializeOwned + Send> FromRequest for Form<T> {
    async fn from_request(req: Request) -> Result<Self, Error> {
        if !content_type_is(&req, "application/x-www-form-urlencoded") {
            return Err(Error::new(
                http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected `Content-Type: application/x-www-form-urlencoded`",
            ));
        }
        let b = take_body(req).await?;
        serde_urlencoded::from_bytes(&b)
            .map(Form)
            .map_err(|e| Error::unprocessable(format!("invalid form body: {e}")))
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
    pub struct DeError(String);

    impl fmt::Display for DeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for DeError {}
    impl de::Error for DeError {
        fn custom<T: fmt::Display>(msg: T) -> Self {
            DeError(msg.to_string())
        }
    }

    pub struct ParamsDe<'a> {
        pub params: Vec<(&'static str, Cow<'a, str>)>,
    }

    impl<'a> ParamsDe<'a> {
        fn single(self) -> Result<ValueDe<'a>, DeError> {
            let n = self.params.len();
            match self.params.into_iter().next() {
                Some((name, v)) if n == 1 => Ok(ValueDe { name, value: v }),
                _ => Err(DeError(format!("expected 1 path parameter, route has {n}"))),
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

    impl<'de, 'a> de::Deserializer<'de> for ParamsDe<'a> {
        type Error = DeError;

        fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            if self.params.len() == 1 {
                self.single()?.deserialize_any(v)
            } else {
                self.deserialize_map(v)
            }
        }
        fn deserialize_map<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            v.visit_map(Map {
                iter: self.params.into_iter(),
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
            v.visit_seq(Seq {
                iter: self.params.into_iter(),
            })
        }
        fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, v: V) -> Result<V::Value, DeError> {
            if self.params.len() != len {
                return Err(DeError(format!(
                    "expected {len} path parameters, route has {}",
                    self.params.len()
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

    struct Map<'a> {
        iter: std::vec::IntoIter<(&'static str, Cow<'a, str>)>,
        value: Option<(&'static str, Cow<'a, str>)>,
    }

    impl<'de, 'a> MapAccess<'de> for Map<'a> {
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

    struct Seq<'a> {
        iter: std::vec::IntoIter<(&'static str, Cow<'a, str>)>,
    }

    impl<'de, 'a> SeqAccess<'de> for Seq<'a> {
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
                    Ok(x) => v.$visit(x),
                    Err(_) => Err(DeError(format!(
                        "`{}` = {:?} is not a valid {}",
                        self.name, self.value, stringify!($visit).trim_start_matches("visit_")
                    ))),
                }
            }
        )*};
    }

    impl<'de, 'a> de::Deserializer<'de> for ValueDe<'a> {
        type Error = DeError;

        fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            v.visit_str(&self.value)
        }
        fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            v.visit_some(self)
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
            _: &'static str,
            _: &'static [&'static str],
            v: V,
        ) -> Result<V::Value, DeError> {
            let s: &str = &self.value;
            v.visit_enum(s.into_deserializer())
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
