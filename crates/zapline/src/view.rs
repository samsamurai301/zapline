//! Server-rendered HTML with [MiniJinja] templates (Jinja2 syntax), built for
//! htmx-style apps: render a whole page, or just one `{% block %}` of it for
//! a partial update, from the same template.
//!
//! ```no_run
//! use zapline::prelude::*;
//! use zapline::view::{context, HxRequest, Views};
//!
//! #[get("/")]
//! async fn index(views: Views, hx: HxRequest) -> Result<Html<String>> {
//!     let todos = vec!["write docs", "ship it"];
//!     // Full page on a normal visit; only the `list` block for htmx requests.
//!     views.page_or_block(&hx, "index.html", "list", context! { todos })
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new().views("templates").mount(index).run("0.0.0.0:8080")
//! }
//! ```
//!
//! `templates/index.html`:
//!
//! ```jinja
//! {% extends "base.html" %}
//! {% block content %}
//!   <ul id="list">{% block list %}{% for t in todos %}<li>{{ t }}</li>{% endfor %}{% endblock %}</ul>
//! {% endblock %}
//! ```
//!
//! In debug builds templates are re-read on every render, so editing a
//! template needs no rebuild: save and refresh. Release builds load each
//! template once. Values are HTML-escaped automatically in `.html`
//! templates.
//!
//! [MiniJinja]: https://docs.rs/minijinja

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

pub use minijinja::{context, Environment, Value};
use serde::Serialize;

use crate::extract::FromRequestParts;
use crate::{App, Error, Html, Request};

type Configure = dyn Fn(&mut Environment<'static>) + Send + Sync;

/// The template engine, as a handler argument. Register it with
/// [`App::views`] or [`App::views_with`].
#[derive(Clone)]
pub struct Views(Arc<Inner>);

struct Inner {
    dir: PathBuf,
    configure: Option<Arc<Configure>>,
    /// `None` in reload mode: a fresh environment per render.
    cached: Option<Environment<'static>>,
}

impl fmt::Debug for Views {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Views")
            .field("dir", &self.0.dir)
            .field("reload", &self.0.cached.is_none())
            .finish()
    }
}

impl Views {
    /// Templates from `dir`. `reload` re-reads them on every render (the
    /// default in debug builds).
    pub fn new(dir: impl Into<PathBuf>, reload: bool) -> Self {
        Self::build(dir.into(), None, reload)
    }

    fn build(dir: PathBuf, configure: Option<Arc<Configure>>, reload: bool) -> Self {
        let mut inner = Inner {
            dir,
            configure,
            cached: None,
        };
        if !reload {
            inner.cached = Some(inner.environment());
        }
        Views(Arc::new(inner))
    }

    /// Render template `name` with `ctx` (anything `Serialize`, usually
    /// [`context!`]).
    pub fn render(&self, name: &str, ctx: impl Serialize) -> crate::Result<Html<String>> {
        self.with_env(|env| {
            let tmpl = env.get_template(name).map_err(template_error)?;
            tmpl.render(ctx).map(Html).map_err(template_error)
        })
    }

    /// Render only `{% block block %}` of template `name`: the fragment an
    /// htmx request swaps in. Blocks see the same context as the full page.
    pub fn render_block(
        &self,
        name: &str,
        block: &str,
        ctx: impl Serialize,
    ) -> crate::Result<Html<String>> {
        self.with_env(|env| {
            let tmpl = env.get_template(name).map_err(template_error)?;
            // Evaluate the template (output discarded) to set up its blocks
            // and top-level variables, then render the one block.
            let mut captured = tmpl
                .render_captured_to(ctx, std::io::sink())
                .map_err(template_error)?;
            captured
                .with_state_mut(|state| state.render_block(block))
                .map(Html)
                .map_err(template_error)
        })
    }

    /// The whole page for normal requests, only `block` for htmx requests
    /// (except history restores, which need the full page).
    pub fn page_or_block(
        &self,
        hx: &HxRequest,
        name: &str,
        block: &str,
        ctx: impl Serialize,
    ) -> crate::Result<Html<String>> {
        if hx.is_partial() {
            self.render_block(name, block, ctx)
        } else {
            self.render(name, ctx)
        }
    }

    /// The same templates with more set up on the environment (for
    /// features that add template functions, like `asset()`).
    #[allow(dead_code)]
    pub(crate) fn extended(
        &self,
        extra: impl Fn(&mut Environment<'static>) + Send + Sync + 'static,
    ) -> Views {
        let before = self.0.configure.clone();
        let configure: Arc<Configure> = Arc::new(move |env: &mut Environment<'static>| {
            if let Some(before) = &before {
                before(env);
            }
            extra(env);
        });
        Self::build(self.0.dir.clone(), Some(configure), self.0.cached.is_none())
    }

    fn with_env<R>(&self, f: impl FnOnce(&Environment<'static>) -> R) -> R {
        match &self.0.cached {
            Some(env) => f(env),
            None => f(&self.0.environment()),
        }
    }
}

impl Inner {
    fn environment(&self) -> Environment<'static> {
        let mut env = Environment::new();
        env.set_loader(minijinja::path_loader(self.dir.clone()));
        if let Some(configure) = &self.configure {
            configure(&mut env);
        }
        env
    }
}

fn template_error(err: minijinja::Error) -> Error {
    // Template bugs are server errors. Show the details (file, line, what
    // went wrong) while developing; keep them in the logs in release.
    let detail = format!("{err:#}");
    let msg = if cfg!(debug_assertions) {
        format!("template error: {detail}")
    } else {
        "internal server error".to_string()
    };
    Error::internal(msg).with_source(err)
}

/// Re-read templates on every render in debug builds, and under
/// `zapline dev` (`ZAPLINE_DEV=1`) even in release builds, so its browser
/// live reload shows edits without a restart. Decided once, at build time.
fn reload_by_default() -> bool {
    cfg!(debug_assertions) || crate::dev::live_reload()
}

impl App {
    /// Render HTML templates from `dir` (relative to the working directory,
    /// i.e. the project root under `cargo run`). Handlers take [`Views`].
    pub fn views(self, dir: impl Into<PathBuf>) -> Self {
        self.state(Views::build(dir.into(), None, reload_by_default()))
    }

    /// Like [`views`](Self::views), with a hook to add filters, functions
    /// and globals to the MiniJinja environment.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// let app = App::new().views_with("templates", |env| {
    ///     env.add_global("site_name", "Acme");
    ///     env.add_filter("shout", |s: String| s.to_uppercase());
    /// });
    /// ```
    pub fn views_with(
        self,
        dir: impl Into<PathBuf>,
        configure: impl Fn(&mut Environment<'static>) + Send + Sync + 'static,
    ) -> Self {
        let views = Views::build(dir.into(), Some(Arc::new(configure)), reload_by_default());
        self.state(views)
    }
}

impl FromRequestParts for Views {
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        req.state::<Views>().cloned().ok_or_else(|| {
            Error::internal("no templates registered: add `.views(\"templates\")` to the app")
        })
    }
}

// ---------------------------------------------------------------------------
// htmx

/// What htmx says about the request (its `HX-*` headers). Never fails:
/// for a plain browser request every field is empty.
#[derive(Clone, Debug, Default)]
pub struct HxRequest {
    /// `HX-Request: true`: the request was made by htmx.
    pub htmx: bool,
    /// `HX-Boosted`: a boosted link or form (expects a full page).
    pub boosted: bool,
    /// `HX-History-Restore-Request`: htmx restoring history (full page).
    pub history_restore: bool,
    /// `HX-Target`: id of the element being swapped.
    pub target: Option<String>,
    /// `HX-Trigger`: id of the element that triggered the request.
    pub trigger: Option<String>,
}

impl HxRequest {
    /// Whether to answer with a fragment rather than a full page.
    pub fn is_partial(&self) -> bool {
        self.htmx && !self.boosted && !self.history_restore
    }
}

impl FromRequestParts for HxRequest {
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        let headers = &req.head.headers;
        let flag = |name: &str| headers.get(name).is_some_and(|v| v.as_bytes() == b"true");
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        Ok(HxRequest {
            htmx: flag("hx-request"),
            boosted: flag("hx-boosted"),
            history_restore: flag("hx-history-restore-request"),
            target: text("hx-target"),
            trigger: text("hx-trigger"),
        })
    }
}

/// Response headers that steer htmx. Wrap any response:
/// `Hx::redirect("/todos")`, or `Hx::trigger("todo-added").with(html)`.
#[derive(Clone, Debug, Default)]
#[must_use]
pub struct Hx {
    headers: Vec<(&'static str, String)>,
}

impl Hx {
    /// Client-side redirect (`HX-Redirect`): htmx navigates to `uri`.
    pub fn redirect(uri: impl Into<String>) -> Self {
        Hx::default().header("hx-redirect", uri)
    }

    /// Reload the whole page (`HX-Refresh`).
    pub fn refresh() -> Self {
        Hx::default().header("hx-refresh", "true")
    }

    /// Fire a client-side event (`HX-Trigger`) other elements can react to.
    pub fn trigger(event: impl Into<String>) -> Self {
        Hx::default().header("hx-trigger", event)
    }

    /// Swap into a different element than the one that asked (`HX-Retarget`).
    pub fn retarget(selector: impl Into<String>) -> Self {
        Hx::default().header("hx-retarget", selector)
    }

    /// Push `uri` into the browser history (`HX-Push-Url`).
    pub fn push_url(uri: impl Into<String>) -> Self {
        Hx::default().header("hx-push-url", uri)
    }

    /// Add another `HX-*` header.
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// These headers on top of `response`.
    pub fn with(self, response: impl crate::IntoResponse) -> crate::Response {
        let mut res = response.into_response();
        for (name, value) in self.headers {
            if let Ok(value) = http::HeaderValue::try_from(value) {
                res.headers_mut().append(name, value);
            }
        }
        res
    }
}

impl crate::IntoResponse for Hx {
    fn into_response(self) -> crate::Response {
        self.with(())
    }
}
