//! HTML form round trips: per-field messages, the values the user typed,
//! and the `422` page or fragment that shows them again.

use std::collections::BTreeMap;

use http::{HeaderValue, StatusCode};
use serde::ser::SerializeMap;
use serde::Serialize;

use super::{context, Value, Views};
use crate::secret::is_secret_name;
use crate::{Html, IntoResponse, Response};

/// What is wrong with a submitted form, field by field, for templates:
/// `{{ errors.title }}` is the first message for field `title` (undefined,
/// so falsy, when the field is fine). Problems with the form as a whole
/// (wrong password, a struct-level rule) go under
/// [`FORM`](Self::FORM): `{{ errors._form }}`.
///
/// Built by [`ValidForm`] from the `validator` rules and from fields that
/// don't parse; by hand for checks only the handler can make:
///
/// ```
/// use zapline::view::FormErrors;
///
/// let mut errors = FormErrors::new();
/// errors.add("email", "is already registered");
/// assert_eq!(errors.field("email"), Some("is already registered"));
/// assert!(errors.any());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormErrors {
    fields: BTreeMap<String, Vec<String>>,
}

impl FormErrors {
    /// The pseudo-field for errors about the whole form.
    ///
    /// ```
    /// use zapline::view::FormErrors;
    /// let errors = FormErrors::one(FormErrors::FORM, "wrong email or password");
    /// assert_eq!(errors.field("_form"), Some("wrong email or password"));
    /// ```
    pub const FORM: &'static str = "_form";

    /// No errors yet.
    ///
    /// ```
    /// assert!(!zapline::view::FormErrors::new().any());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// A single error.
    ///
    /// ```
    /// let errors = zapline::view::FormErrors::one("email", "is already registered");
    /// assert_eq!(errors.field("email"), Some("is already registered"));
    /// ```
    pub fn one(field: impl Into<String>, message: impl Into<String>) -> Self {
        let mut errors = Self::new();
        errors.add(field, message);
        errors
    }

    /// Add a message for `field` (a field can have several).
    ///
    /// ```
    /// let mut errors = zapline::view::FormErrors::new();
    /// errors.add("title", "is required").add("body", "is too long");
    /// assert_eq!(errors.len(), 2);
    /// ```
    pub fn add(&mut self, field: impl Into<String>, message: impl Into<String>) -> &mut Self {
        self.fields
            .entry(field.into())
            .or_default()
            .push(message.into());
        self
    }

    /// The first message for `field`.
    ///
    /// ```
    /// let errors = zapline::view::FormErrors::one("title", "is required");
    /// assert_eq!(errors.field("title"), Some("is required"));
    /// assert_eq!(errors.field("body"), None);
    /// ```
    pub fn field(&self, name: &str) -> Option<&str> {
        self.messages(name).first().map(String::as_str)
    }

    /// Every message for `field`, in the order they were added.
    ///
    /// ```
    /// let mut errors = zapline::view::FormErrors::new();
    /// errors.add("code", "is too short").add("code", "must be digits");
    /// assert_eq!(errors.messages("code"), ["is too short", "must be digits"]);
    /// ```
    pub fn messages(&self, name: &str) -> &[String] {
        self.fields.get(name).map_or(&[], Vec::as_slice)
    }

    /// Whether there is any error.
    ///
    /// ```
    /// assert!(zapline::view::FormErrors::one("title", "is required").any());
    /// ```
    pub fn any(&self) -> bool {
        !self.fields.is_empty()
    }

    /// Number of fields with errors.
    ///
    /// ```
    /// assert_eq!(zapline::view::FormErrors::new().len(), 0);
    /// ```
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether no field has an error (the opposite of [`any`](Self::any)).
    ///
    /// ```
    /// assert!(zapline::view::FormErrors::new().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// `(field, first message)` pairs, sorted by field.
    ///
    /// ```
    /// let errors = zapline::view::FormErrors::one("title", "is required");
    /// assert_eq!(errors.iter().collect::<Vec<_>>(), [("title", "is required")]);
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields
            .iter()
            .filter_map(|(k, v)| Some((k.as_str(), v.first()?.as_str())))
    }

    /// The messages in an error: the per-field `details` of a validation
    /// error (`422 validation_error`, as `Valid<T>` returns), otherwise the
    /// error's message under [`FORM`](Self::FORM). A server error (5xx)
    /// becomes a generic message: its text is for the logs.
    ///
    /// ```
    /// use zapline::prelude::*;
    /// use zapline::view::FormErrors;
    ///
    /// let err = Error::conflict("this email is already registered");
    /// let errors = FormErrors::from_error(&err);
    /// assert_eq!(errors.field("_form"), Some("this email is already registered"));
    /// ```
    #[cfg(feature = "json")]
    pub fn from_error(err: &crate::Error) -> Self {
        let mut errors = Self::new();
        if let Some(serde_json::Value::Array(details)) = err.details() {
            for detail in details {
                let field = detail.get("field").and_then(|f| f.as_str());
                let message = detail.get("message").and_then(|m| m.as_str());
                if let (Some(field), Some(message)) = (field, message) {
                    errors.add(form_field(field), message);
                }
            }
        }
        if errors.is_empty() {
            let message = if err.status().is_server_error() {
                "something went wrong, please try again"
            } else {
                err.message()
            };
            errors.add(Self::FORM, message);
        }
        errors
    }
}

/// Validation reports struct-level errors with an empty field path.
#[cfg(feature = "json")]
fn form_field(field: &str) -> &str {
    if field.is_empty() {
        FormErrors::FORM
    } else {
        field
    }
}

/// Field -> first message.
impl Serialize for FormErrors {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.fields.len()))?;
        for (field, message) in self.iter() {
            map.serialize_entry(field, message)?;
        }
        map.end()
    }
}

#[cfg(feature = "validate")]
impl From<&validator::ValidationErrors> for FormErrors {
    /// Every failed rule, with the same messages as the JSON `422`.
    fn from(errors: &validator::ValidationErrors) -> Self {
        let mut out = FormErrors::new();
        for e in crate::validate::field_errors(errors) {
            out.add(form_field(&e.field), e.message);
        }
        out
    }
}

#[cfg(feature = "validate")]
impl From<validator::ValidationErrors> for FormErrors {
    fn from(errors: validator::ValidationErrors) -> Self {
        FormErrors::from(&errors)
    }
}

/// A form submission to show again: what is wrong, and what the user typed
/// (by field name). [`ValidForm`] hands it to the handler; pass it to
/// [`Views::render_invalid`].
///
/// Fields that look secret (`password`, `token`, `secret`, `*_hash`) are
/// never put back into the page, whatever `old` holds.
///
/// ```
/// use zapline::view::{FormErrors, Invalid};
///
/// let invalid = Invalid {
///     errors: FormErrors::one("email", "is already registered"),
///     old: [("email".to_string(), "ada@example.com".to_string())].into(),
/// };
/// assert_eq!(invalid.errors.field("email"), Some("is already registered"));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Invalid {
    /// What is wrong.
    pub errors: FormErrors,
    /// The submitted values, by field name.
    pub old: BTreeMap<String, String>,
}

impl Invalid {
    /// `errors` about a form that parsed and validated, but that the
    /// handler rejects (a taken email, a wrong password), with `old` taken
    /// from the parsed value.
    ///
    /// ```
    /// use serde::Serialize;
    /// use zapline::view::{FormErrors, Invalid};
    ///
    /// #[derive(Serialize)]
    /// struct SignUp { email: String, password: String }
    ///
    /// let form = SignUp { email: "ada@example.com".into(), password: "hunter22".into() };
    /// let invalid = Invalid::new(FormErrors::one("email", "is already registered"), &form);
    /// assert_eq!(invalid.old["email"], "ada@example.com");
    /// assert!(!invalid.old.contains_key("password"));
    /// ```
    #[cfg(feature = "form")]
    pub fn new(errors: FormErrors, form: &impl Serialize) -> Self {
        let old = serde_urlencoded::to_string(form)
            .ok()
            .and_then(|s| serde_urlencoded::from_str::<Vec<(String, String)>>(&s).ok())
            .unwrap_or_default();
        Invalid {
            errors,
            old: old_values(old),
        }
    }
}

/// Submitted values to show again: the first value of each field, without
/// secret-looking fields.
#[cfg(feature = "form")]
pub(crate) fn old_values(pairs: Vec<(String, String)>) -> BTreeMap<String, String> {
    let mut old = BTreeMap::new();
    for (k, v) in pairs {
        if !is_secret_name(&k) {
            old.entry(k).or_insert(v);
        }
    }
    old
}

impl Views {
    /// Show a rejected form again: renders `name` with `ctx` plus `errors`
    /// ([`FormErrors`]) and `old` (the submitted values) as a
    /// `422 Unprocessable Entity`.
    ///
    /// For an htmx request only the template's `{% block form %}` is
    /// rendered (the whole template if it has none), meant to be the
    /// *inside* of the `<form>`. When the form has an `id` and posts
    /// somewhere else (`hx-target="#list"`), the response carries
    /// `HX-Retarget: #<form id>` and `HX-Reswap: innerHTML`, so the
    /// messages land in the form, not in the list. htmx doesn't swap error
    /// responses by default: allow `422` once in the layout (see the guide's
    /// "Forms" section). Secret-looking fields are never echoed back.
    ///
    /// ```no_run
    /// use serde::Deserialize;
    /// use validator::Validate;
    /// use zapline::prelude::*;
    /// use zapline::view::{context, ValidForm, Views};
    ///
    /// #[derive(Deserialize, Validate)]
    /// struct NewPost {
    ///     #[validate(length(min = 1, max = 200))]
    ///     title: String,
    /// }
    ///
    /// #[post("/posts")]
    /// async fn create(views: Views, ValidForm(form): ValidForm<NewPost>) -> Result<Response> {
    ///     let post = match form {
    ///         Ok(post) => post,
    ///         Err(invalid) => return views.render_invalid("posts/new.html", context! {}, invalid),
    ///     };
    ///     // ... save `post` ...
    ///     # let _ = post.title;
    ///     Ok(Redirect::to("/posts").into_response())
    /// }
    /// ```
    ///
    /// `templates/posts/new.html`:
    ///
    /// ```jinja
    /// <form id="post-form" hx-post="/posts">
    ///   {% block form %}
    ///   <input name="title" value="{{ old.title }}">
    ///   {% if errors.title %}<small class="error">{{ errors.title }}</small>{% endif %}
    ///   <button>Save</button>
    ///   {% endblock %}
    /// </form>
    /// ```
    pub fn render_invalid(
        &self,
        name: &str,
        ctx: impl Serialize,
        invalid: Invalid,
    ) -> crate::Result<Response> {
        let Invalid { errors, old } = invalid;
        // Also covers an `old` filled by hand.
        let old: BTreeMap<String, String> = old
            .into_iter()
            .filter(|(k, _)| !is_secret_name(k))
            .collect();
        let ctx = context! { errors, old, ..Value::from_serialize(&ctx) };
        let partial = self.hx().filter(|hx| hx.is_partial());
        let html = match partial {
            Some(_) => self.render_form_block(name, &ctx)?,
            None => self.render(name, &ctx)?.0,
        };
        let mut res = (StatusCode::UNPROCESSABLE_ENTITY, Html(html)).into_response();
        let form_id = partial
            .and_then(|hx| hx.trigger.as_deref())
            .filter(|id| is_plain_id(id));
        if let (Some(hx), Some(id)) = (partial, form_id) {
            let headers = res.headers_mut();
            if hx.target.as_deref() != Some(id) {
                if let Ok(v) = HeaderValue::try_from(format!("#{id}")) {
                    headers.insert("hx-retarget", v);
                }
            }
            headers.insert("hx-reswap", HeaderValue::from_static("innerHTML"));
        }
        Ok(res)
    }

    /// `{% block form %}` of `name`, or all of it when it has no such block.
    fn render_form_block(&self, name: &str, ctx: &Value) -> crate::Result<String> {
        self.with_env(|env| {
            let tmpl = env.get_template(name).map_err(super::template_error)?;
            let mut captured = tmpl
                .render_captured_to(ctx, std::io::sink())
                .map_err(super::template_error)?;
            match captured.with_state_mut(|state| state.render_block("form")) {
                Ok(html) => Ok(html),
                Err(e) if e.kind() == minijinja::ErrorKind::UnknownBlock => {
                    tmpl.render(ctx).map_err(super::template_error)
                }
                Err(e) => Err(super::template_error(e)),
            }
        })
    }
}

/// An element id usable as a `#id` selector as is.
fn is_plain_id(id: &str) -> bool {
    id.starts_with(|c: char| c.is_ascii_alphabetic())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---------------------------------------------------------------------------
// ValidForm

#[cfg(all(feature = "form", feature = "validate"))]
pub use valid_form::ValidForm;

#[cfg(all(feature = "form", feature = "validate"))]
mod valid_form {
    use serde::de::DeserializeOwned;
    use validator::Validate;

    use super::{old_values, FormErrors, Invalid};
    use crate::extract::{Form, FromRequest};
    use crate::{Error, Request};

    /// An HTML form post, parsed and validated, that never rejects bad
    /// input: the handler gets `Ok(value)`, or `Err(`[`Invalid`]`)` with a
    /// message per field and the submitted values, to show the form again
    /// with [`Views::render_invalid`](super::Views::render_invalid).
    ///
    /// Fields that don't parse (`age=abc` for a number) get a message too
    /// ("must be a number"), as do missing ones ("is required"); then the
    /// `validator` rules run. Only a request that isn't a form at all
    /// (wrong `Content-Type`, body too large) is rejected, like [`Form`].
    ///
    /// `Valid<Form<T>>` stays the choice for APIs: it rejects with the JSON
    /// `422` before the handler runs. `ValidForm` is for pages, where the
    /// handler must answer with HTML either way, so the outcome is a
    /// `Result` it can't forget to handle.
    ///
    /// ```
    /// use serde::Deserialize;
    /// use validator::Validate;
    /// use zapline::prelude::*;
    /// use zapline::view::{context, ValidForm, Views};
    ///
    /// #[derive(Deserialize, Validate)]
    /// struct Contact {
    ///     #[validate(email)]
    ///     email: String,
    /// }
    ///
    /// async fn contact(views: Views, ValidForm(form): ValidForm<Contact>) -> Result<Response> {
    ///     match form {
    ///         Ok(contact) => Ok(Redirect::to("/thanks").into_response()),
    ///         Err(invalid) => views.render_invalid("contact.html", context! {}, invalid),
    ///     }
    /// }
    /// # let _ = App::new().views("templates").post("/contact", contact);
    /// ```
    #[derive(Debug)]
    pub struct ValidForm<T>(pub Result<T, Invalid>);

    impl<T> ValidForm<T> {
        /// The outcome.
        ///
        /// ```
        /// # use zapline::view::{Invalid, ValidForm};
        /// let form: ValidForm<u8> = ValidForm(Ok(1));
        /// assert_eq!(form.into_inner().ok(), Some(1));
        /// ```
        pub fn into_inner(self) -> Result<T, Invalid> {
            self.0
        }
    }

    impl<T: DeserializeOwned + Validate + Send> FromRequest for ValidForm<T> {
        async fn from_request(req: Request) -> Result<Self, Error> {
            let Form(pairs) = Form::<Vec<(String, String)>>::from_request(req).await?;
            let result = match deserialize::<T>(&pairs) {
                Ok(value) => match value.validate() {
                    Ok(()) => Ok(value),
                    Err(errors) => Err(FormErrors::from(&errors)),
                },
                Err(errors) => Err(errors),
            };
            Ok(ValidForm(result.map_err(|errors| Invalid {
                errors,
                old: old_values(pairs),
            })))
        }
    }

    /// Parse like `Form<T>`, but name the field at fault. A field that
    /// doesn't parse is set aside and parsing retried, so every such field
    /// gets its message, not just the first.
    fn deserialize<T: DeserializeOwned>(pairs: &[(String, String)]) -> Result<T, FormErrors> {
        let mut remaining = pairs.to_vec();
        let mut errors = FormErrors::new();
        for _ in 0..=pairs.len() {
            let body = form_urlencoded::Serializer::new(String::new())
                .extend_pairs(&remaining)
                .finish();
            let de = serde_urlencoded::Deserializer::new(form_urlencoded::parse(body.as_bytes()));
            let err = match serde_path_to_error::deserialize::<_, T>(de) {
                Ok(value) if errors.is_empty() => return Ok(value),
                Ok(_) => return Err(errors),
                Err(err) => err,
            };
            let path = err.path().to_string();
            let message = err.inner().to_string();
            if let Some(field) = missing_field(&message) {
                if errors.messages(field).is_empty() {
                    errors.add(field, "is required");
                }
                return Err(errors);
            }
            let Some(value) = remaining
                .iter()
                .find(|(k, _)| *k == path)
                .map(|(_, v)| v.clone())
            else {
                // Not one top-level field (a nested path, or the whole form).
                if errors.is_empty() {
                    errors.add(FormErrors::FORM, message);
                }
                return Err(errors);
            };
            errors.add(path.clone(), parse_message(&message, &value));
            remaining.retain(|(k, _)| *k != path);
        }
        Err(errors)
    }

    /// The field in serde's "missing field `x`".
    fn missing_field(message: &str) -> Option<&str> {
        let rest = message.strip_prefix("missing field `")?;
        rest.split('`').next()
    }

    /// A message for a value that didn't parse, in the words of the
    /// validation messages.
    fn parse_message(error: &str, value: &str) -> &'static str {
        if value.trim().is_empty() {
            "is required"
        } else if ["digit", "integer", "float", "number"]
            .iter()
            .any(|w| error.contains(w))
        {
            "must be a number"
        } else if error.contains("`true` or `false`") {
            "must be true or false"
        } else if error.contains("unknown variant") {
            "is not one of the choices"
        } else {
            "is invalid"
        }
    }

    #[cfg(feature = "openapi")]
    impl<T: schemars::JsonSchema> crate::openapi::ApiInput for ValidForm<T> {
        fn describe_input(op: &mut crate::openapi::OperationDoc) {
            op.request_body_as::<T>("application/x-www-form-urlencoded")
                .error_response(422, "Invalid form (re-rendered with errors)");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Item {
            name: String,
            qty: u32,
            price: f64,
        }

        fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
            list.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        }

        #[test]
        fn every_unparsable_field_is_named() {
            let errors =
                deserialize::<Item>(&pairs(&[("name", "x"), ("qty", "lots"), ("price", "")]))
                    .unwrap_err();
            assert_eq!(errors.field("qty"), Some("must be a number"));
            assert_eq!(errors.field("price"), Some("is required"));
            assert_eq!(errors.field("name"), None);
        }

        #[test]
        fn missing_fields_are_required() {
            let errors = deserialize::<Item>(&pairs(&[("qty", "1"), ("price", "2")])).unwrap_err();
            assert_eq!(errors.field("name"), Some("is required"));
            assert!(
                deserialize::<Item>(&pairs(&[("name", "a"), ("qty", "1"), ("price", "2")])).is_ok()
            );
        }
    }
}
