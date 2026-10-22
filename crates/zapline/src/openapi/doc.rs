//! Per-route documentation metadata.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};

pub(crate) type Text = Cow<'static, str>;

/// Produces a schema with a shared generator (so components are reused).
pub(crate) type SchemaFn = fn(&mut SchemaGenerator) -> Schema;

/// `$ref` to `T`'s component (or `T`'s schema when it is inlined).
pub(crate) fn reference<T: JsonSchema>(g: &mut SchemaGenerator) -> Schema {
    g.subschema_for::<T>()
}

/// `T`'s own schema, so parameters can look at its properties.
pub(crate) fn inline<T: JsonSchema>(g: &mut SchemaGenerator) -> Schema {
    T::json_schema(g)
}

/// Where a parameter lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParamIn {
    /// A route parameter, e.g. `{id}`.
    Path,
    /// A query-string parameter.
    Query,
    /// A request header.
    Header,
    /// A cookie.
    Cookie,
}

impl ParamIn {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ParamIn::Path => "path",
            ParamIn::Query => "query",
            ParamIn::Header => "header",
            ParamIn::Cookie => "cookie",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct QueryDoc {
    pub(crate) schema: SchemaFn,
    pub(crate) optional: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ParamDoc {
    pub(crate) location: ParamIn,
    pub(crate) name: Text,
    pub(crate) required: bool,
    pub(crate) schema: SchemaFn,
}

#[derive(Clone, Debug)]
pub(crate) struct BodyDoc {
    pub(crate) required: bool,
    pub(crate) content: Vec<(Text, Option<SchemaFn>)>,
}

/// A response status: the route's success status, a fixed code, or `default`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Success,
    Code(u16),
    Default,
}

#[derive(Clone, Debug)]
pub(crate) struct ResponseDoc {
    pub(crate) status: Status,
    pub(crate) description: Option<Text>,
    /// Content type and schema; `None` schema means "any bytes".
    pub(crate) content: Option<(Text, Option<SchemaFn>)>,
    /// Response headers: `(name, description)`.
    pub(crate) headers: Vec<(Text, Text)>,
}

impl ResponseDoc {
    /// A `status` response whose body is the framework's `Error`.
    pub(crate) fn error(status: u16, description: &'static str) -> Self {
        ResponseDoc {
            status: Status::Code(status),
            description: Some(description.into()),
            content: error_content(),
            headers: Vec::new(),
        }
    }
}

/// A kind of credentials, for documenting extractors that don't know the
/// name of the app's security scheme (e.g. `BearerToken`).
///
/// When the spec is generated, it resolves to the first scheme of that kind
/// declared on [`OpenApi`](super::OpenApi) (e.g. with `.bearer_auth("jwt")`);
/// if there is none, a default scheme is added: `bearerAuth`, `basicAuth`
/// or `apiKeyAuth` (header `x-api-key`).
///
/// ```
/// use zapline::openapi::{ApiInput, AuthKind, OperationDoc};
///
/// struct AdminToken(String);
///
/// impl ApiInput for AdminToken {
///     fn describe_input(op: &mut OperationDoc) {
///         op.security_kind(AuthKind::Bearer)
///             .error_response(401, "Unauthorized");
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuthKind {
    /// HTTP bearer tokens (`Authorization: Bearer ..`).
    Bearer,
    /// HTTP basic credentials.
    Basic,
    /// An API key (`apiKey` scheme).
    ApiKey,
}

/// A security scheme required by an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scheme {
    /// Declared on `OpenApi` under this name.
    Named(Text),
    /// The app's scheme for this kind of credentials.
    Kind(AuthKind),
    /// However the app authenticates users (`CurrentUser`, route policies):
    /// the group's or the spec's security, else bearer.
    User,
}

#[derive(Clone, Debug)]
pub(crate) struct SecurityDoc {
    pub(crate) scheme: Scheme,
    /// Added by an optional extractor (`Option<BearerToken>`).
    pub(crate) optional: bool,
}

/// How much input-side documentation an operation had, so docs added by an
/// optional extractor can be found afterwards.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InputMark {
    query: usize,
    params: usize,
    body: bool,
    security: usize,
    responses: usize,
}

/// OpenAPI documentation for one route (an "operation").
///
/// The route macros fill this in from doc comments, macro arguments and the
/// handler signature. Builder methods on `Routes`/`App` (`.summary(..)`,
/// `.tag(..)`, `.response::<T>(..)`, ...) edit the doc of the most recently
/// registered route, and custom extractors or response types document
/// themselves by implementing [`ApiInput`](super::ApiInput) /
/// [`ApiOutput`](super::ApiOutput).
///
/// ```
/// use zapline::openapi::OperationDoc;
///
/// let mut doc = OperationDoc::new();
/// doc.summary("List users").tag("users").response::<Vec<String>>(200, "User names");
/// ```
#[derive(Clone, Debug, Default)]
pub struct OperationDoc {
    pub(crate) handler: Option<&'static str>,
    /// `module_path!()` of a macro route's handler, to tell apart
    /// same-named handlers in different modules.
    pub(crate) module: Option<&'static str>,
    pub(crate) summary: Option<Text>,
    pub(crate) description: Option<Text>,
    pub(crate) tags: Vec<Text>,
    pub(crate) operation_id: Option<Text>,
    pub(crate) deprecated: bool,
    pub(crate) hidden: bool,
    pub(crate) security: Vec<SecurityDoc>,
    pub(crate) success_status: Option<u16>,
    pub(crate) path: Option<SchemaFn>,
    pub(crate) query: Vec<QueryDoc>,
    pub(crate) params: Vec<ParamDoc>,
    pub(crate) body: Option<BodyDoc>,
    pub(crate) responses: Vec<ResponseDoc>,
}

impl OperationDoc {
    /// An empty doc.
    pub fn new() -> Self {
        Self::default()
    }

    /// A doc that hides the route from the generated spec.
    pub fn hidden_route() -> Self {
        OperationDoc {
            hidden: true,
            ..Self::default()
        }
    }

    /// One-line summary shown next to the route.
    pub fn summary(&mut self, summary: impl Into<Text>) -> &mut Self {
        self.summary = Some(summary.into());
        self
    }

    /// Longer description (CommonMark).
    pub fn description(&mut self, description: impl Into<Text>) -> &mut Self {
        self.description = Some(description.into());
        self
    }

    /// Add a tag (used by docs UIs to group operations).
    pub fn tag(&mut self, tag: impl Into<Text>) -> &mut Self {
        let tag = tag.into();
        if !self.tags.contains(&tag) {
            self.tags.push(tag);
        }
        self
    }

    /// Explicit `operationId` (default: the handler name for macro routes,
    /// otherwise `<method>_<path>`). Duplicates get a suffix.
    pub fn operation_id(&mut self, id: impl Into<Text>) -> &mut Self {
        self.operation_id = Some(id.into());
        self
    }

    /// Mark the operation deprecated.
    pub fn deprecated(&mut self) -> &mut Self {
        self.deprecated = true;
        self
    }

    /// Leave the route out of the generated spec.
    pub fn hidden(&mut self) -> &mut Self {
        self.hidden = true;
        self
    }

    /// Require a security scheme declared on [`OpenApi`](super::OpenApi)
    /// (e.g. with `bearer_auth("bearer")`). Several calls list alternatives:
    /// any one of them is enough.
    pub fn security(&mut self, scheme: impl Into<Text>) -> &mut Self {
        self.push_security(Scheme::Named(scheme.into()))
    }

    /// Require the app's security scheme for `kind` of credentials, whatever
    /// its name (see [`AuthKind`]). Combines with [`security`](Self::security)
    /// as an alternative.
    pub fn security_kind(&mut self, kind: AuthKind) -> &mut Self {
        self.push_security(Scheme::Kind(kind))
    }

    /// Require an authenticated user, however the app authenticates them.
    pub(crate) fn security_user(&mut self) -> &mut Self {
        self.push_security(Scheme::User)
    }

    fn push_security(&mut self, scheme: Scheme) -> &mut Self {
        match self.security.iter_mut().find(|s| s.scheme == scheme) {
            // Required once means required.
            Some(existing) => existing.optional = false,
            None => self.security.push(SecurityDoc {
                scheme,
                optional: false,
            }),
        }
        self
    }

    /// The status of a successful response (default 200).
    pub fn success_status(&mut self, status: u16) -> &mut Self {
        self.success_status = Some(status);
        self
    }

    /// Describe the route parameters with `T` (a struct, a tuple or a scalar),
    /// like `Path<T>` does.
    pub fn path_params<T: JsonSchema>(&mut self) -> &mut Self {
        self.path = Some(inline::<T>);
        self
    }

    /// Describe query parameters with the properties of `T`, like `Query<T>`.
    pub fn query_params<T: JsonSchema>(&mut self) -> &mut Self {
        self.query.push(QueryDoc {
            schema: inline::<T>,
            optional: false,
        });
        self
    }

    /// Add a single parameter (e.g. a header) whose value matches `T`.
    pub fn param<T: JsonSchema>(
        &mut self,
        location: ParamIn,
        name: impl Into<Text>,
        required: bool,
    ) -> &mut Self {
        self.params.push(ParamDoc {
            location,
            name: name.into(),
            required: required || location == ParamIn::Path,
            schema: reference::<T>,
        });
        self
    }

    /// A required JSON request body of type `T`.
    pub fn request_body<T: JsonSchema>(&mut self) -> &mut Self {
        self.request_body_as::<T>("application/json")
    }

    /// A required request body of type `T` with the given content type
    /// (e.g. `application/x-www-form-urlencoded`).
    pub fn request_body_as<T: JsonSchema>(&mut self, content_type: impl Into<Text>) -> &mut Self {
        self.add_body(content_type.into(), Some(reference::<T>))
    }

    /// A required request body of any bytes with the given content type.
    pub fn request_body_raw(&mut self, content_type: impl Into<Text>) -> &mut Self {
        self.add_body(content_type.into(), None)
    }

    pub(crate) fn add_body(&mut self, content_type: Text, schema: Option<SchemaFn>) -> &mut Self {
        let body = self.body.get_or_insert_with(|| BodyDoc {
            required: true,
            content: Vec::new(),
        });
        body.content.retain(|(ct, _)| *ct != content_type);
        body.content.push((content_type, schema));
        self
    }

    /// A JSON response of type `T` for `status`.
    pub fn response<T: JsonSchema>(
        &mut self,
        status: u16,
        description: impl Into<Text>,
    ) -> &mut Self {
        self.push_response(
            Status::Code(status),
            Some(description.into()),
            Some(("application/json".into(), Some(reference::<T>))),
        )
    }

    /// A response without a body for `status`.
    pub fn response_empty(&mut self, status: u16, description: impl Into<Text>) -> &mut Self {
        self.push_response(Status::Code(status), Some(description.into()), None)
    }

    /// An error response for `status` whose body is the framework's `Error`.
    pub fn error_response(&mut self, status: u16, description: impl Into<Text>) -> &mut Self {
        self.push_response(
            Status::Code(status),
            Some(description.into()),
            error_content(),
        )
    }

    /// The catch-all (`default`) error response.
    pub fn default_error(&mut self) -> &mut Self {
        self.push_response(Status::Default, Some("Error".into()), error_content())
    }

    /// The successful response has content type `content_type` and type `T`.
    pub fn success<T: JsonSchema>(&mut self, content_type: impl Into<Text>) -> &mut Self {
        self.push_response(
            Status::Success,
            None,
            Some((content_type.into(), Some(reference::<T>))),
        )
    }

    /// The successful response has content type `content_type` (any bytes).
    pub fn success_raw(&mut self, content_type: impl Into<Text>) -> &mut Self {
        self.push_response(Status::Success, None, Some((content_type.into(), None)))
    }

    /// The successful response has no body.
    pub fn success_empty(&mut self) -> &mut Self {
        self.push_response(Status::Success, None, None)
    }

    /// A `303 See Other` redirect with a `Location` header.
    pub fn redirect(&mut self) -> &mut Self {
        self.push_response(Status::Code(303), None, None);
        if let Some(r) = self.responses.last_mut() {
            r.headers
                .push(("Location".into(), "Where to go next.".into()));
        }
        self
    }

    fn push_response(
        &mut self,
        status: Status,
        description: Option<Text>,
        content: Option<(Text, Option<SchemaFn>)>,
    ) -> &mut Self {
        self.responses.push(ResponseDoc {
            status,
            description,
            content,
            headers: Vec::new(),
        });
        self
    }

    /// Snapshot of the input-side docs, for [`Self::make_optional_since`].
    pub(crate) fn input_mark(&self) -> InputMark {
        InputMark {
            query: self.query.len(),
            params: self.params.len(),
            body: self.body.is_some(),
            security: self.security.len(),
            responses: self.responses.len(),
        }
    }

    /// Docs added after `mark` came from an optional extractor
    /// (`Option<BearerToken>`), which never fails: its inputs become
    /// optional, its security can be skipped and its error responses go.
    pub(crate) fn make_optional_since(&mut self, mark: InputMark) {
        for q in &mut self.query[mark.query..] {
            q.optional = true;
        }
        for p in &mut self.params[mark.params..] {
            if p.location != ParamIn::Path {
                p.required = false;
            }
        }
        if !mark.body {
            if let Some(b) = &mut self.body {
                b.required = false;
            }
        }
        for s in &mut self.security[mark.security..] {
            s.optional = true;
        }
        self.responses.truncate(mark.responses);
    }
}

fn error_content() -> Option<(Text, Option<SchemaFn>)> {
    Some((
        "application/json".into(),
        Some(reference::<crate::Error> as SchemaFn),
    ))
}

/// Documentation defaults for a group of routes, set with
/// `Routes::tag_all` / `Routes::security_all`.
///
/// Route-level tags are added after the group's; route-level security
/// replaces the group's. Nested groups accumulate: an inner group's tags and
/// schemes come first, then the outer group's.
#[derive(Clone, Debug, Default)]
pub struct GroupDoc {
    pub(crate) tags: Vec<Text>,
    pub(crate) security: Vec<Text>,
}

impl GroupDoc {
    /// Empty defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Tag every route in the group.
    pub fn tag(&mut self, tag: impl Into<Text>) -> &mut Self {
        let tag = tag.into();
        if !self.tags.contains(&tag) {
            self.tags.push(tag);
        }
        self
    }

    /// Require a security scheme on every route in the group.
    pub fn security(&mut self, scheme: impl Into<Text>) -> &mut Self {
        let scheme = scheme.into();
        if !self.security.contains(&scheme) {
            self.security.push(scheme);
        }
        self
    }
}
