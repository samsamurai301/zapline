//! OpenAPI 3.1 generation and interactive docs.
//!
//! Like FastAPI, a zapline app documents itself: route macros read the
//! handler's doc comment and its argument/return types (`Path<T>`,
//! `Query<T>`, `Json<T>`, `Option<T>`, `Result<T>`...), and the [`OpenApi`]
//! plugin turns the route table into a spec served at `/openapi.json`, with
//! Swagger UI at `/docs`.
//!
//! ```no_run
//! use serde::{Deserialize, Serialize};
//! use zapline::openapi::{JsonSchema, OpenApi};
//! use zapline::prelude::*;
//!
//! #[derive(Serialize, Deserialize, JsonSchema)]
//! struct User {
//!     id: u64,
//!     name: String,
//! }
//!
//! /// Get a user.
//! ///
//! /// Returns 404 when the user does not exist.
//! #[get("/users/{id}", tag = "users")]
//! async fn get_user(Path(id): Path<u64>) -> Option<Json<User>> {
//!     (id == 1).then(|| Json(User { id, name: "ada".into() }))
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .mount(get_user)
//!         .get("/health", || async { "ok" })
//!         .summary("Liveness probe") // builder-style docs for plain routes
//!         .openapi(OpenApi::new("Users API", "1.0.0").bearer_auth("bearer"))
//!         .run("0.0.0.0:8080")
//! }
//! ```
//!
//! Authentication is documented too. Credential extractors (`BearerToken`,
//! `Jwt`, `BasicAuth`, `ApiKey`, `CurrentUser`) and route policies
//! (`.require_auth()`, `.require_roles(..)`, `.require_scopes(..)`) add a
//! security requirement and `401`/`403` responses, using the schemes declared
//! on [`OpenApi`] (the first bearer scheme for bearer tokens, and so on); a
//! default scheme such as `bearerAuth` is added when none fits. Routes marked
//! `.allow_anonymous()` get `security: []`. Infrastructure routes stay out of
//! the spec: the docs themselves, [`Health`](crate::health::Health) and
//! [`Metrics`](crate::metrics::Metrics) endpoints (unless they opt in with
//! `.document(true)`) and static files.
//!
//! Types appear in the spec when they implement [`JsonSchema`]. Derive it with
//! `#[derive(JsonSchema)]`; the derive expands to paths in the `schemars`
//! crate, so add `schemars = "1"` to your `Cargo.toml` (or point the derive at
//! the re-export with `#[schemars(crate = "zapline::openapi::schemars")]`).
//! Types without `JsonSchema` still work as handler arguments and return
//! values; they are simply not described.
//!
//! The spec is generated once, when the app is built, and served as static
//! bytes, so documentation costs nothing per request.

mod describe;
mod doc;
mod generate;
mod ui;

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::HeaderValue;
use serde_json::Value;

pub use describe::{ApiInput, ApiOutput};
pub use doc::{AuthKind, GroupDoc, OperationDoc, ParamIn};
pub use schemars::{self, JsonSchema};
pub use ui::DocsUi;

use crate::auth::AllowAnonymous;
use crate::{App, Plugin, RouteInfo, Routes};

/// Env var: when set to a file path, the [`OpenApi`] plugin writes the spec
/// there (pretty JSON) while the app is being built, then exits the process.
/// `zapline openapi` relies on this.
pub const ENV_EXPORT_OPENAPI: &str = "ZAPLINE_EXPORT_OPENAPI";

type SpecHook = Arc<dyn Fn(&mut Value) + Send + Sync>;

/// The OpenAPI plugin: generates the spec from the route table and serves it
/// together with interactive docs.
///
/// ```no_run
/// use zapline::openapi::{DocsUi, OpenApi};
/// use zapline::prelude::*;
///
/// let app = App::new().get("/", || async { "hi" }).openapi(
///     OpenApi::new("My API", "1.2.0")
///         .description("Everything about my service.")
///         .server("https://api.example.com")
///         .bearer_auth("bearer")
///         .docs(DocsUi::Swagger, "/docs")
///         .docs(DocsUi::Scalar, "/reference"),
/// );
/// ```
#[derive(Clone)]
pub struct OpenApi {
    pub(crate) title: String,
    pub(crate) version: String,
    pub(crate) description: Option<String>,
    pub(crate) contact: Option<(String, String)>,
    pub(crate) license: Option<String>,
    pub(crate) servers: Vec<(String, Option<String>)>,
    pub(crate) tags: Vec<(String, Option<String>)>,
    pub(crate) schemes: Vec<(String, Value)>,
    pub(crate) security: Vec<String>,
    pub(crate) hooks: Vec<SpecHook>,
    json_path: String,
    spec_url: Option<String>,
    cdn: String,
    uis: Option<Vec<(DocsUi, String)>>,
}

impl fmt::Debug for OpenApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenApi")
            .field("title", &self.title)
            .field("version", &self.version)
            .field("json_path", &self.json_path)
            .field("docs", &self.docs_pages())
            .finish_non_exhaustive()
    }
}

/// Paths always start with `/` and have no trailing slash.
fn normalize_path(path: &str) -> String {
    let path = path.trim();
    let mut p = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

impl OpenApi {
    /// A spec titled `title` at API version `version`, served at
    /// `/openapi.json` with Swagger UI at `/docs`.
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        OpenApi {
            title: title.into(),
            version: version.into(),
            description: None,
            contact: None,
            license: None,
            servers: Vec::new(),
            tags: Vec::new(),
            schemes: Vec::new(),
            security: Vec::new(),
            hooks: Vec::new(),
            json_path: "/openapi.json".to_owned(),
            spec_url: None,
            cdn: ui::DEFAULT_CDN.to_owned(),
            uis: None,
        }
    }

    /// API description (CommonMark).
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add a server URL (e.g. `https://api.example.com/v1`).
    pub fn server(mut self, url: impl Into<String>) -> Self {
        self.servers.push((url.into(), None));
        self
    }

    /// Add a server URL with a description (e.g. "Staging").
    pub fn server_with(mut self, url: impl Into<String>, description: impl Into<String>) -> Self {
        self.servers.push((url.into(), Some(description.into())));
        self
    }

    /// Contact information.
    pub fn contact(mut self, name: impl Into<String>, email: impl Into<String>) -> Self {
        self.contact = Some((name.into(), email.into()));
        self
    }

    /// License name, e.g. `"MIT"`.
    pub fn license(mut self, name: impl Into<String>) -> Self {
        self.license = Some(name.into());
        self
    }

    /// Describe a tag. Tags are listed in the order they are declared here.
    pub fn tag(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.tags.push((name.into(), Some(description.into())));
        self
    }

    /// Declare an HTTP bearer (JWT) security scheme named `name`. Routes
    /// opt in with `security = "name"` or `.security("name")`.
    pub fn bearer_auth(self, name: impl Into<String>) -> Self {
        self.security_scheme(
            name,
            serde_json::json!({ "type": "http", "scheme": "bearer", "bearerFormat": "JWT" }),
        )
    }

    /// Declare an HTTP basic security scheme named `name`.
    pub fn basic_auth(self, name: impl Into<String>) -> Self {
        self.security_scheme(
            name,
            serde_json::json!({ "type": "http", "scheme": "basic" }),
        )
    }

    /// Declare an API key security scheme named `name`, sent in `header`.
    pub fn api_key_auth(self, name: impl Into<String>, header: impl Into<String>) -> Self {
        let header = header.into();
        self.security_scheme(
            name,
            serde_json::json!({ "type": "apiKey", "in": "header", "name": header }),
        )
    }

    /// Declare any security scheme object (OAuth2, OpenID Connect, ...).
    pub fn security_scheme(mut self, name: impl Into<String>, scheme: Value) -> Self {
        let name = name.into();
        self.schemes.retain(|(n, _)| *n != name);
        self.schemes.push((name, scheme));
        self
    }

    /// Require `scheme` for every operation that doesn't set its own security.
    pub fn security(mut self, scheme: impl Into<String>) -> Self {
        self.security.push(scheme.into());
        self
    }

    /// Where the spec is served (default `/openapi.json`).
    pub fn json_path(mut self, path: impl AsRef<str>) -> Self {
        self.json_path = normalize_path(path.as_ref());
        self
    }

    /// The URL the docs pages load the spec from (default: the JSON path).
    /// Set it when the app is served behind a path-rewriting proxy.
    pub fn spec_url(mut self, url: impl Into<String>) -> Self {
        self.spec_url = Some(url.into());
        self
    }

    /// Load docs UI assets from a mirror with the npm package layout instead
    /// of `https://cdn.jsdelivr.net/npm` (e.g. an internal proxy in an
    /// air-gapped network). Files must be byte-identical: the pages pin exact
    /// versions and Subresource Integrity hashes.
    pub fn cdn(mut self, base_url: impl Into<String>) -> Self {
        self.cdn = base_url.into();
        self
    }

    /// Serve a docs UI at `path`. The first call replaces the default
    /// (Swagger UI at `/docs`); further calls add more UIs.
    pub fn docs(mut self, ui: DocsUi, path: impl AsRef<str>) -> Self {
        let path = normalize_path(path.as_ref());
        let uis = self.uis.get_or_insert_with(Vec::new);
        uis.retain(|(_, p)| *p != path);
        uis.push((ui, path));
        self
    }

    /// Serve only the JSON spec, no docs UI.
    pub fn no_docs(mut self) -> Self {
        self.uis = Some(Vec::new());
        self
    }

    /// Post-process the generated spec (add extensions, examples, webhooks...).
    pub fn modify(mut self, f: impl Fn(&mut Value) + Send + Sync + 'static) -> Self {
        self.hooks.push(Arc::new(f));
        self
    }

    /// Generate the spec for a route table (see [`App::on_build`]).
    pub fn generate(&self, routes: &[&'static RouteInfo]) -> Value {
        generate::generate(self, routes)
    }

    fn docs_pages(&self) -> Vec<(DocsUi, String)> {
        match &self.uis {
            Some(uis) => uis.clone(),
            None => vec![(DocsUi::Swagger, "/docs".to_owned())],
        }
    }

    /// The routes serving the spec and the docs pages.
    fn routes(self, table: &[&'static RouteInfo]) -> Routes {
        let spec = self.generate(table);
        if let Some(path) = std::env::var_os(ENV_EXPORT_OPENAPI) {
            let path = std::path::PathBuf::from(path);
            match export_spec(&spec, &path) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!(
                        "zapline: failed to write the OpenAPI spec to {}: {e}",
                        path.display()
                    );
                    std::process::exit(1);
                }
            }
        }

        // Not part of the API, and public even behind `Authenticate::required()`.
        let json = Bytes::from(generate::to_json(&spec, false));
        let mut routes = Routes::new().get(&self.json_path, static_page(json, "application/json"));
        let spec_url = self.spec_url.as_deref().unwrap_or(&self.json_path);
        for (ui, path) in self.docs_pages() {
            let html = Bytes::from(ui.page(&self.title, spec_url, &self.cdn));
            routes = routes.get(&path, static_page(html, "text/html; charset=utf-8"));
        }
        routes
            .meta_all(OperationDoc::hidden_route())
            .meta_all(AllowAnonymous)
            .meta_all(ServedSpec(Arc::new(spec)))
    }
}

/// The spec an [`OpenApi`] plugin serves, kept on its routes so the test
/// client's fuzzer checks the app against the same document.
#[derive(Clone)]
pub(crate) struct ServedSpec(pub(crate) Arc<Value>);

/// A handler serving pre-rendered bytes: one refcount bump per request.
fn static_page(
    body: Bytes,
    content_type: &'static str,
) -> impl Fn() -> std::future::Ready<crate::Response> + Clone + Send + Sync + 'static {
    move || {
        let mut res = crate::Response::new(body.clone().into());
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        std::future::ready(res)
    }
}

impl Plugin for OpenApi {
    fn install(self, app: App) -> App {
        app.on_build(move |table| self.routes(table))
    }
}

/// Write `spec` to `path` as pretty-printed JSON (what
/// [`ENV_EXPORT_OPENAPI`] does before exiting).
pub fn export_spec(spec: &Value, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let mut bytes = generate::to_json(spec, true);
    bytes.push(b'\n');
    std::fs::write(path, bytes)
}

/// The spec `api` would serve for `app`, without starting anything. Handy
/// in tests and tooling.
pub fn spec_for(app: App, api: OpenApi) -> Value {
    api.generate(&app.route_table())
}

impl App {
    /// Serve an OpenAPI spec for this app plus interactive docs (Swagger UI
    /// at `/docs` by default). See [`OpenApi`].
    pub fn openapi(self, api: OpenApi) -> Self {
        self.plugin(api)
    }
}

fn last_doc(routes: &mut Routes) -> &mut OperationDoc {
    routes
        .last_meta_mut()
        .get_or_insert_default::<OperationDoc>()
}

/// Builder methods that edit the [`OperationDoc`] of the most recently
/// registered route, on both `Routes` and `App`.
macro_rules! doc_methods {
    ($(
        $(#[$attr:meta])*
        fn $name:ident $(<$g:ident: $bound:path>)? ($($arg:ident: $ty:ty),*) => |$doc:ident| $body:expr;
    )*) => {
        /// OpenAPI documentation for the most recently registered route.
        ///
        /// # Panics
        /// If no route has been registered yet.
        impl Routes {$(
            $(#[$attr])*
            pub fn $name $(<$g: $bound>)? (mut self, $($arg: $ty),*) -> Self {
                let $doc = last_doc(&mut self);
                $body;
                self
            }
        )*}

        /// OpenAPI documentation for the most recently registered route.
        ///
        /// # Panics
        /// If no route has been registered yet.
        impl App {$(
            $(#[$attr])*
            pub fn $name $(<$g: $bound>)? (mut self, $($arg: $ty),*) -> Self {
                let $doc = last_doc(&mut self.routes);
                $body;
                self
            }
        )*}
    };
}

doc_methods! {
    /// OpenAPI summary of the last registered route.
    fn summary(summary: impl Into<Cow<'static, str>>) => |doc| doc.summary(summary);
    /// OpenAPI description (CommonMark) of the last registered route.
    fn description(description: impl Into<Cow<'static, str>>) => |doc| doc.description(description);
    /// Add an OpenAPI tag to the last registered route.
    fn tag(tag: impl Into<Cow<'static, str>>) => |doc| doc.tag(tag);
    /// Set the `operationId` of the last registered route.
    fn operation_id(id: impl Into<Cow<'static, str>>) => |doc| doc.operation_id(id);
    /// Mark the last registered route deprecated in the spec.
    fn deprecated() => |doc| doc.deprecated();
    /// Leave the last registered route out of the spec.
    fn hidden() => |doc| doc.hidden();
    /// Require a security scheme (declared on [`OpenApi`]) for the last
    /// registered route.
    fn security(scheme: impl Into<Cow<'static, str>>) => |doc| doc.security(scheme);
    /// Document a JSON response of type `T` for the last registered route.
    fn response<T: JsonSchema>(status: u16, description: impl Into<Cow<'static, str>>)
        => |doc| doc.response::<T>(status, description);
    /// Document a JSON request body of type `T` for the last registered route.
    fn request_body<T: JsonSchema>() => |doc| doc.request_body::<T>();
    /// Edit the full [`OperationDoc`] of the last registered route.
    fn api_doc(f: impl FnOnce(&mut OperationDoc)) => |doc| f(doc);
}

impl Routes {
    /// Tag every route in this group (including routes added later). Tags
    /// of nested groups accumulate, inner groups' first.
    pub fn tag_all(self, tag: impl Into<Cow<'static, str>>) -> Self {
        let tag = tag.into();
        self.map_meta_all(move |meta| {
            meta.get_or_insert_default::<GroupDoc>().tag(tag.clone());
        })
    }

    /// Require a security scheme on every route in this group that doesn't
    /// set its own. Schemes of nested groups accumulate as alternatives.
    pub fn security_all(self, scheme: impl Into<Cow<'static, str>>) -> Self {
        let scheme = scheme.into();
        self.map_meta_all(move |meta| {
            meta.get_or_insert_default::<GroupDoc>()
                .security(scheme.clone());
        })
    }
}
