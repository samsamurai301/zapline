//! Request validation with [`validator`].
//!
//! Wrap an extractor in [`Valid`] to run `validator::Validate` on its value
//! before the handler sees it. Failures become a `422` with every problem
//! listed:
//!
//! ```no_run
//! use serde::Deserialize;
//! use validator::Validate;
//! use zapline::prelude::*;
//! use zapline::validate::Valid;
//!
//! #[derive(Deserialize, Validate)]
//! struct SignUp {
//!     #[validate(email)]
//!     email: String,
//!     #[validate(length(min = 8, message = "must be at least 8 characters"))]
//!     password: String,
//! }
//!
//! #[post("/signup")]
//! async fn signup(Valid(Json(form)): Valid<Json<SignUp>>) -> StatusCode {
//!     StatusCode::CREATED
//! }
//! ```
//!
//! ```text
//! HTTP/1.1 422 Unprocessable Entity
//! {"error":"validation failed","code":"validation_error","details":[
//!   {"field":"email","code":"email","message":"must be a valid email address","params":{}}]}
//! ```
//!
//! The derive expands to paths in the `validator` crate, so add
//! `validator = { version = "0.21", features = ["derive"] }` to your
//! `Cargo.toml` (or use `#[validate(crate = "zapline::validate::validator")]`).
//! Rejected values are never echoed back in the error details.

use std::borrow::Cow;
use std::ops::{Deref, DerefMut};

use serde::Serialize;
use serde_json::{Map, Value};

pub use validator::{self, Validate, ValidationError, ValidationErrors, ValidationErrorsKind};

use crate::extract::private::ViaRequest;
use crate::extract::{FromRequest, FromRequestParts};
use crate::request::Request;
use crate::Error;

/// Validates the value of the wrapped extractor (`Json<T>`, `Form<T>`,
/// `Query<T>` or `Path<T>`, where `T: Validate`).
///
/// Derefs to the wrapped extractor (which derefs to `T`), and destructures
/// like `Valid(Json(body))`.
///
/// ```
/// # use serde::Deserialize;
/// # use validator::Validate;
/// # use zapline::prelude::*;
/// use zapline::validate::Valid;
///
/// #[derive(Deserialize, Validate)]
/// struct Page {
///     #[validate(range(min = 1, max = 100))]
///     size: u32,
/// }
///
/// async fn list(Valid(Query(page)): Valid<Query<Page>>) -> String {
///     format!("size={}", page.size)
/// }
/// # let _ = App::new().get("/", list);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Valid<T>(pub T);

impl<T> Valid<T> {
    /// The wrapped extractor.
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for Valid<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Valid<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// Extractors whose value [`Valid`] can validate. Implement it for your own
/// extractors to make them usable with `Valid`.
pub trait Validatable {
    /// The value to validate.
    type Value: Validate + ?Sized;
    /// Borrow the value to validate.
    fn validatable(&self) -> &Self::Value;
}

macro_rules! validatable {
    ($($(#[$cfg:meta])* $ty:ident),*) => {$(
        $(#[$cfg])*
        impl<T: Validate> Validatable for crate::extract::$ty<T> {
            type Value = T;
            #[inline]
            fn validatable(&self) -> &T {
                &self.0
            }
        }
    )*};
}

validatable!(
    #[cfg(feature = "json")]
    Json,
    #[cfg(feature = "form")]
    Form,
    #[cfg(feature = "query")]
    Query,
    Path
);

/// Parts extractors (`Query`, `Path`): usable in any argument position.
impl<E> FromRequestParts for Valid<E>
where
    E: FromRequestParts + Validatable,
{
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        let inner = E::from_request_parts(req).await?;
        check(inner.validatable())?;
        Ok(Valid(inner))
    }
}

/// Body extractors (`Json`, `Form`): last argument only.
impl<E> FromRequest<ViaRequest> for Valid<E>
where
    E: FromRequest<ViaRequest> + Validatable,
{
    async fn from_request(req: Request) -> Result<Self, Error> {
        let inner = E::from_request(req).await?;
        check(inner.validatable())?;
        Ok(Valid(inner))
    }
}

fn check<T: Validate + ?Sized>(value: &T) -> Result<(), Error> {
    value.validate().map_err(validation_error)
}

/// The `422` error [`Valid`] returns, for validating by hand:
/// `form.validate().map_err(validation_error)?`.
///
/// (A plain `?` on `ValidationErrors` would produce a 500, like any other
/// `std::error::Error`.)
pub fn validation_error(errors: ValidationErrors) -> Error {
    Error::unprocessable("validation failed")
        .with_code("validation_error")
        .with_details(field_errors(&errors))
}

/// One failed rule, as listed in a validation error's `details`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldError {
    /// Path to the field: `email`, `address.zip`, `items[0].name`. Empty for
    /// struct-level (schema) errors on the top-level value.
    pub field: String,
    /// The rule that failed: `email`, `length`, `range`, or a custom code.
    pub code: String,
    /// Human-readable message (the rule's `message`, or a default).
    pub message: String,
    /// The rule's parameters, e.g. `{"min": 8}`. Never includes the value.
    pub params: Map<String, Value>,
}

/// Flatten nested `ValidationErrors` into a list sorted by field path.
pub fn field_errors(errors: &ValidationErrors) -> Vec<FieldError> {
    let mut out = Vec::new();
    flatten(errors, "", &mut out);
    out.sort_by(|a, b| a.field.cmp(&b.field));
    out
}

fn join(prefix: &str, field: &str) -> String {
    match (prefix, field) {
        (p, "__all__") => p.to_owned(),
        ("", f) => f.to_owned(),
        (p, f) => format!("{p}.{f}"),
    }
}

fn flatten(errors: &ValidationErrors, prefix: &str, out: &mut Vec<FieldError>) {
    for (field, kind) in errors.errors() {
        let path = join(prefix, field);
        match kind {
            ValidationErrorsKind::Field(errs) => {
                out.extend(errs.iter().map(|e| field_error(&path, e)));
            }
            ValidationErrorsKind::Struct(inner) => flatten(inner, &path, out),
            ValidationErrorsKind::List(items) => {
                for (i, inner) in items {
                    flatten(inner, &format!("{path}[{i}]"), out);
                }
            }
        }
    }
}

fn field_error(field: &str, e: &ValidationError) -> FieldError {
    let mut params: Map<String, Value> = e
        .params
        .iter()
        .filter(|(k, _)| k.as_ref() != "value")
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    // `must_match` reports the *other field's value* as `other`.
    if e.code == "must_match" {
        params.remove("other");
    }
    let message = match &e.message {
        Some(m) => m.to_string(),
        None => default_message(&e.code, &params).into_owned(),
    };
    FieldError {
        field: field.to_owned(),
        code: e.code.to_string(),
        message,
        params,
    }
}

fn default_message(code: &str, params: &Map<String, Value>) -> Cow<'static, str> {
    let p = |k: &str| params.get(k).map(|v| v.to_string());
    match code {
        "email" => "must be a valid email address".into(),
        "url" => "must be a valid URL".into(),
        "required" => "is required".into(),
        "regex" => "has an invalid format".into(),
        "credit_card" => "must be a valid credit card number".into(),
        "non_control_character" => "must not contain control characters".into(),
        "ip" => "must be a valid IP address".into(),
        "must_match" => "must match the other field".into(),
        "contains" => match p("needle") {
            Some(n) => format!("must contain {n}").into(),
            None => "is missing a required value".into(),
        },
        "does_not_contain" => match p("needle") {
            Some(n) => format!("must not contain {n}").into(),
            None => "contains a forbidden value".into(),
        },
        "length" => match (p("equal"), p("min"), p("max")) {
            (Some(e), _, _) => format!("length must be exactly {e}").into(),
            (None, Some(min), Some(max)) => {
                format!("length must be between {min} and {max}").into()
            }
            (None, Some(min), None) => format!("length must be at least {min}").into(),
            (None, None, Some(max)) => format!("length must be at most {max}").into(),
            _ => "has an invalid length".into(),
        },
        "range" => {
            let lower = p("min")
                .map(|m| format!("at least {m}"))
                .or_else(|| p("exclusive_min").map(|m| format!("greater than {m}")));
            let upper = p("max")
                .map(|m| format!("at most {m}"))
                .or_else(|| p("exclusive_max").map(|m| format!("less than {m}")));
            match (lower, upper) {
                (Some(l), Some(u)) => format!("must be {l} and {u}").into(),
                (Some(b), None) | (None, Some(b)) => format!("must be {b}").into(),
                (None, None) => "is out of range".into(),
            }
        }
        other => format!("failed `{other}` validation").into(),
    }
}

#[cfg(feature = "openapi")]
impl<T: crate::openapi::ApiInput> crate::openapi::ApiInput for Valid<T> {
    fn describe_input(op: &mut crate::openapi::OperationDoc) {
        op.error_response(422, "Validation Error");
        T::describe_input(op);
    }
}
