//! How handler argument and return types document themselves.

use std::borrow::Cow;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};

use super::{AuthKind, OperationDoc};
use crate::extract::{Extension, Json, MatchedPath, Path, State};
use crate::{auth, Html, Redirect};

/// A handler argument type that documents what it reads from the request.
///
/// The route macros call this for every argument whose type implements it;
/// other types (state, custom extractors without docs, types without
/// `JsonSchema`) are silently skipped. Implement it for your own extractors:
///
/// ```
/// use zapline::openapi::{ApiInput, OperationDoc, ParamIn};
///
/// struct ApiKey(String);
///
/// impl ApiInput for ApiKey {
///     fn describe_input(op: &mut OperationDoc) {
///         op.param::<String>(ParamIn::Header, "x-api-key", true);
///     }
/// }
/// ```
pub trait ApiInput {
    /// Add this input's parameters/body/responses to `op`.
    fn describe_input(op: &mut OperationDoc);
}

/// A handler return type that documents the responses it produces.
///
/// See [`ApiInput`]; types that don't implement it contribute nothing and
/// the route gets a generic success response.
pub trait ApiOutput {
    /// Add this output's responses to `op`.
    fn describe_output(op: &mut OperationDoc);
}

// ---------------------------------------------------------------------------
// Inputs

/// Path and query extractors answer `400` when parameters don't parse.
const INVALID_PARAMS: &str = "Invalid request parameters";

impl<T: JsonSchema> ApiInput for Path<T> {
    fn describe_input(op: &mut OperationDoc) {
        op.path_params::<T>().error_response(400, INVALID_PARAMS);
    }
}

#[cfg(feature = "query")]
impl<T: JsonSchema> ApiInput for crate::extract::Query<T> {
    fn describe_input(op: &mut OperationDoc) {
        op.query_params::<T>().error_response(400, INVALID_PARAMS);
    }
}

impl<T: JsonSchema> ApiInput for Json<T> {
    fn describe_input(op: &mut OperationDoc) {
        op.request_body::<T>()
            .error_response(422, "Invalid request body");
    }
}

#[cfg(feature = "form")]
impl<T: JsonSchema> ApiInput for crate::extract::Form<T> {
    fn describe_input(op: &mut OperationDoc) {
        op.request_body_as::<T>("application/x-www-form-urlencoded")
            .error_response(422, "Invalid request body");
    }
}

impl ApiInput for Bytes {
    fn describe_input(op: &mut OperationDoc) {
        op.request_body_raw("application/octet-stream");
    }
}

impl ApiInput for String {
    fn describe_input(op: &mut OperationDoc) {
        op.request_body_as::<String>("text/plain");
    }
}

/// Optional extractors make their parameters and body optional.
impl<T: ApiInput> ApiInput for Option<T> {
    fn describe_input(op: &mut OperationDoc) {
        let mark = op.input_mark();
        T::describe_input(op);
        op.make_optional_since(mark);
    }
}

impl<T: ApiInput> ApiInput for Result<T, crate::Error> {
    fn describe_input(op: &mut OperationDoc) {
        T::describe_input(op);
    }
}

/// Extractors that read nothing the spec describes. Implementing
/// [`ApiInput`] anyway marks them as known to the docs.
macro_rules! undocumented_input {
    ($($(#[$cfg:meta])* [$($g:tt)*] $t:ty),* $(,)?) => {$(
        $(#[$cfg])*
        impl<$($g)*> ApiInput for $t {
            #[inline]
            fn describe_input(_: &mut OperationDoc) {}
        }
    )*};
}
undocumented_input!(
    [] Method,
    [] Uri,
    [] HeaderMap,
    [] crate::Request,
    [] crate::ConnInfo,
    [] MatchedPath,
    [T] State<T>,
    [T] Extension<T>,
    [T: ?Sized] crate::di::Dep<T>,
    [] crate::tasks::BackgroundTasks,
    [] crate::middleware::RequestId,
    #[cfg(feature = "cookies")]
    [] crate::cookies::CookieJar,
    #[cfg(feature = "cookies")]
    [] crate::cookies::SignedCookieJar,
    #[cfg(feature = "cookies")]
    [] crate::cookies::PrivateCookieJar,
    #[cfg(feature = "cookies")]
    [] crate::session::Session,
    #[cfg(feature = "cookies")]
    [] auth::CsrfToken,
);

/// A cached extractor documents like the extractor itself.
impl<T: ApiInput> ApiInput for crate::di::Cached<T> {
    fn describe_input(op: &mut OperationDoc) {
        T::describe_input(op);
    }
}

/// A WebSocket handshake answers `101 Switching Protocols`.
#[cfg(feature = "ws")]
impl ApiInput for crate::ws::WebSocketUpgrade {
    fn describe_input(op: &mut OperationDoc) {
        op.response_empty(101, "Switching Protocols");
    }
}

impl ApiInput for crate::stream::BodyStream {
    fn describe_input(op: &mut OperationDoc) {
        op.request_body_raw("application/octet-stream");
    }
}

#[cfg(feature = "multipart")]
impl ApiInput for crate::multipart::Multipart {
    fn describe_input(op: &mut OperationDoc) {
        fn any_object(_: &mut SchemaGenerator) -> Schema {
            json_schema!({ "type": "object" })
        }
        op.add_body("multipart/form-data".into(), Some(any_object));
    }
}

/// Credential extractors require the app's scheme for their kind of
/// credentials and reject with `401`.
macro_rules! credentials_input {
    ($($(#[$cfg:meta])* [$($g:tt)*] $t:ty => $kind:expr),* $(,)?) => {$(
        $(#[$cfg])*
        impl<$($g)*> ApiInput for $t {
            fn describe_input(op: &mut OperationDoc) {
                op.security_kind($kind)
                    .error_response(401, "Unauthorized");
            }
        }
    )*};
}
credentials_input!(
    [] auth::BearerToken => AuthKind::Bearer,
    [] auth::BasicAuth => AuthKind::Basic,
    [] auth::ApiKey => AuthKind::ApiKey,
    #[cfg(feature = "jwt")]
    [C] auth::Jwt<C> => AuthKind::Bearer,
);

/// The authenticated user: however the app authenticates users (the
/// group's or the spec's security), bearer by default.
impl<U> ApiInput for auth::CurrentUser<U> {
    fn describe_input(op: &mut OperationDoc) {
        op.security_user().error_response(401, "Unauthorized");
    }
}

// ---------------------------------------------------------------------------
// Outputs

impl<T: JsonSchema> ApiOutput for Json<T> {
    fn describe_output(op: &mut OperationDoc) {
        op.success::<T>("application/json");
    }
}

macro_rules! text_output {
    ($($t:ty),*) => {$(
        impl ApiOutput for $t {
            fn describe_output(op: &mut OperationDoc) {
                op.success::<String>("text/plain");
            }
        }
    )*};
}
text_output!(String, &'static str, Cow<'static, str>);

macro_rules! bytes_output {
    ($($t:ty),*) => {$(
        impl ApiOutput for $t {
            fn describe_output(op: &mut OperationDoc) {
                op.success_raw("application/octet-stream");
            }
        }
    )*};
}
bytes_output!(Bytes, Vec<u8>, &'static [u8]);

impl<T> ApiOutput for Html<T> {
    fn describe_output(op: &mut OperationDoc) {
        op.success::<String>("text/html");
    }
}

impl ApiOutput for () {
    fn describe_output(op: &mut OperationDoc) {
        op.success_empty();
    }
}

impl ApiOutput for StatusCode {
    fn describe_output(op: &mut OperationDoc) {
        op.success_empty();
    }
}

impl ApiOutput for Redirect {
    fn describe_output(op: &mut OperationDoc) {
        op.redirect();
    }
}

/// `None` becomes a 404.
impl<T: ApiOutput> ApiOutput for Option<T> {
    fn describe_output(op: &mut OperationDoc) {
        T::describe_output(op);
        op.error_response(404, "Not Found");
    }
}

/// Errors are documented as the `default` response.
impl<T: ApiOutput, E> ApiOutput for Result<T, E> {
    fn describe_output(op: &mut OperationDoc) {
        T::describe_output(op);
        op.default_error();
    }
}

impl<T: ApiOutput> ApiOutput for (StatusCode, T) {
    fn describe_output(op: &mut OperationDoc) {
        T::describe_output(op);
    }
}

impl<T: ApiOutput, const N: usize> ApiOutput for ([(HeaderName, HeaderValue); N], T) {
    fn describe_output(op: &mut OperationDoc) {
        T::describe_output(op);
    }
}

impl<T: ApiOutput, const N: usize> ApiOutput for (StatusCode, [(HeaderName, HeaderValue); N], T) {
    fn describe_output(op: &mut OperationDoc) {
        T::describe_output(op);
    }
}

impl<S> ApiOutput for crate::sse::Sse<S> {
    fn describe_output(op: &mut OperationDoc) {
        op.success::<String>("text/event-stream");
    }
}

impl<S> ApiOutput for crate::stream::StreamBody<S> {
    fn describe_output(op: &mut OperationDoc) {
        op.success_raw("application/octet-stream");
    }
}

impl ApiOutput for crate::stream::Ndjson {
    fn describe_output(op: &mut OperationDoc) {
        op.success_raw("application/x-ndjson");
    }
}

/// Cookie jars set cookies on the response they are returned with.
#[cfg(feature = "cookies")]
macro_rules! jar_output {
    ($($jar:ty),*) => {$(
        impl ApiOutput for $jar {
            fn describe_output(op: &mut OperationDoc) {
                op.success_empty();
            }
        }

        impl<T: ApiOutput> ApiOutput for ($jar, T) {
            fn describe_output(op: &mut OperationDoc) {
                T::describe_output(op);
            }
        }

        impl<T: ApiOutput> ApiOutput for (StatusCode, $jar, T) {
            fn describe_output(op: &mut OperationDoc) {
                T::describe_output(op);
            }
        }
    )*};
}
#[cfg(feature = "cookies")]
jar_output!(
    crate::cookies::CookieJar,
    crate::cookies::SignedCookieJar,
    crate::cookies::PrivateCookieJar
);

// ---------------------------------------------------------------------------
// The error body

/// The JSON body of every [`Error`](crate::Error) response, published as the
/// `Error` component. Use it in docs: `.response::<Error>(409, "Conflict")`.
impl JsonSchema for crate::Error {
    fn schema_name() -> Cow<'static, str> {
        "Error".into()
    }

    fn schema_id() -> Cow<'static, str> {
        "zapline::Error".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "description": "Error response body.",
            "required": ["error"],
            "properties": {
                "error": {
                    "type": "string",
                    "description": "Human-readable message."
                },
                "code": {
                    "type": "string",
                    "description": "Stable, machine-readable error code."
                },
                "details": {
                    "description": "Structured details, e.g. per-field validation errors."
                }
            }
        })
    }
}
