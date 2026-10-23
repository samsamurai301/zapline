//! The development console: one page at [`DEV_CONSOLE_PATH`] that shows what
//! the running app is made of and doing.
//!
//! ```no_run
//! use zapline::prelude::*;
//!
//! #[get("/")]
//! async fn index() -> &'static str { "hi" }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new().mount(index).dev_console().run("127.0.0.1:8080")
//! }
//! ```
//!
//! It lists every route (with its summary and tags when the `openapi`
//! feature is on) and the last requests with their status and latency, and
//! the features that know about it add their own panels: [jobs](crate::jobs)
//! show their queue and can retry failed jobs, an
//! [outbox mailer](crate::mail) links its inbox.
//!
//! The console exists only in debug builds and only answers requests from
//! the same machine: in a release build [`App::dev_console`] does nothing,
//! so the call can stay in the code.
//!
//! It also turns on the [dev error page](App::dev_errors), lists the recent
//! errors with their [diagnostic codes](crate::codes), and serves all of it
//! as JSON at [`DEV_API_PATH`] for tools such as `zapline mcp`:
//!
//! ```text
//! GET /_dev/api
//! {"requests":[{"method":"GET","path":"/orders","status":500,"micros":812,"code":"ZL0002"}],
//!  "errors":[{"at_ms":1767225600000,"method":"GET","path":"/orders","status":500,
//!             "code":"ZL0002","error_code":"missing_dependency",
//!             "message":"no provider registered for `app::Repo`","fix":".provide(value) ..."}],
//!  "routes":[{"method":"GET","path":"/orders"}]}
//! ```

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::handler::BoxFuture;
use crate::response::{Html, IntoResponse, Redirect, Response};
use crate::router::{RouteInfo, Routes};
use crate::{App, Request, StatusCode};

mod errors;

pub use errors::DIAGNOSTIC_HEADER;
pub(crate) use errors::{install, panic_response, take_panic_location};
use errors::{DevErrorsOn, ErrorLog};

/// Where [`App::dev_console`] serves the console.
pub const DEV_CONSOLE_PATH: &str = "/_dev";

/// Where [`App::dev_console`] serves the console's data as JSON.
pub const DEV_API_PATH: &str = "/_dev/api";

/// Env var naming a file where a debug build writes the URL it listens on
/// (`http://127.0.0.1:8080`), so tools find the running app. `zapline dev`
/// sets it to `target/zapline-dev.addr`.
pub const ENV_DEV_ADDR_FILE: &str = "ZAPLINE_DEV_ADDR_FILE";

/// Record where the server listens, for tools (see [`ENV_DEV_ADDR_FILE`]).
pub(crate) fn announce(addr: std::net::SocketAddr, tls: bool) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(file) = std::env::var_os(ENV_DEV_ADDR_FILE) else {
        return;
    };
    let scheme = if tls { "https" } else { "http" };
    let _ = std::fs::write(file, format!("{scheme}://{addr}\n"));
}

/// How many recent requests the console keeps.
const RECENT: usize = 50;

type Render = Arc<dyn Fn(&Request) -> BoxFuture<String> + Send + Sync>;
type Action = Arc<dyn Fn(&Request) -> BoxFuture<crate::Result<()>> + Send + Sync>;

/// A section of the console contributed by a feature.
#[derive(Clone)]
pub(crate) struct DevPanel {
    /// Unique, URL-safe: actions are posted to `/_dev/{id}`.
    pub(crate) id: String,
    pub(crate) title: String,
    /// The panel's HTML (already escaped).
    pub(crate) render: Render,
    /// A button label and what it does when pressed.
    pub(crate) action: Option<(String, Action)>,
}

// Used by the features that add panels (jobs, mail).
#[allow(dead_code)]
impl DevPanel {
    pub(crate) fn new<F>(id: impl Into<String>, title: impl Into<String>, render: F) -> Self
    where
        F: Fn(&Request) -> BoxFuture<String> + Send + Sync + 'static,
    {
        DevPanel {
            id: id.into(),
            title: title.into(),
            render: Arc::new(render),
            action: None,
        }
    }

    pub(crate) fn action<F>(mut self, label: impl Into<String>, action: F) -> Self
    where
        F: Fn(&Request) -> BoxFuture<crate::Result<()>> + Send + Sync + 'static,
    {
        self.action = Some((label.into(), Arc::new(action)));
        self
    }
}

/// Panels registered so far, shared between [`App::dev_panel`] and the
/// console whatever order they are called in.
#[derive(Clone, Default)]
struct Panels(Arc<Mutex<Vec<DevPanel>>>);

impl Panels {
    fn lock(&self) -> MutexGuard<'_, Vec<DevPanel>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct Hit {
    method: String,
    path: String,
    status: u16,
    micros: u128,
    /// The diagnostic code of an error response.
    code: Option<&'static str>,
}

#[derive(Clone, Default)]
struct Recent(Arc<Mutex<VecDeque<Hit>>>);

impl Recent {
    fn lock(&self) -> MutexGuard<'_, VecDeque<Hit>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A route as the console shows it.
struct RouteRow {
    method: String,
    path: String,
    summary: Option<String>,
    tags: Vec<String>,
}

impl RouteRow {
    fn new(route: &RouteInfo) -> Self {
        #[allow(unused_mut)]
        let mut row = RouteRow {
            method: route.method().map_or("ANY", |m| m.as_str()).to_owned(),
            path: route.path().to_owned(),
            summary: None,
            tags: Vec::new(),
        };
        #[cfg(feature = "openapi")]
        if let Some(doc) = route.meta::<crate::openapi::OperationDoc>() {
            row.summary = doc.summary.as_deref().map(str::to_owned);
            row.tags = doc.tags.iter().map(|t| t.to_string()).collect();
        }
        row
    }
}

impl App {
    /// Serve the [development console](crate::dev) at [`DEV_CONSOLE_PATH`]:
    /// routes, recent requests and errors, and panels from jobs and the dev
    /// mailer, plus the same data as JSON at [`DEV_API_PATH`]. Also turns on
    /// the [dev error page](App::dev_errors). Debug builds only, and only to
    /// requests from this machine; in a release build this does nothing.
    ///
    /// ```
    /// use zapline::App;
    ///
    /// let app = App::new().dev_console();
    /// ```
    pub fn dev_console(mut self) -> Self {
        if !cfg!(debug_assertions) {
            return self;
        }
        let panels = self.panels();
        // The dev error middleware (installed when the app is built) records
        // requests and errors here.
        let recent = Recent::default();
        let errors = ErrorLog::default();
        self.extensions_mut().insert(recent.clone());
        self.extensions_mut().insert(errors.clone());
        self.extensions_mut().insert(DevErrorsOn);
        self.on_build(move |table| {
            let routes: Arc<[RouteRow]> = table.iter().map(|r| RouteRow::new(r)).collect();
            let page = {
                let (routes, recent, errors, panels) = (
                    routes.clone(),
                    recent.clone(),
                    errors.clone(),
                    panels.clone(),
                );
                move |req: Request| {
                    let (routes, recent, errors, panels) = (
                        routes.clone(),
                        recent.clone(),
                        errors.clone(),
                        panels.clone(),
                    );
                    async move { console(&req, &routes, &recent, &errors, &panels).await }
                }
            };
            let api = move |req: Request| {
                let (routes, recent, errors) = (routes.clone(), recent.clone(), errors.clone());
                async move { api(&req, &routes, &recent, &errors) }
            };
            let act = move |req: Request| {
                let panels = panels.clone();
                async move { act(req, panels).await }
            };
            internal(
                Routes::new()
                    .get(DEV_CONSOLE_PATH, page)
                    .get(DEV_API_PATH, api)
                    .post(&format!("{DEV_CONSOLE_PATH}/{{panel}}"), act),
            )
        })
    }

    /// Add a panel to the console (whether or not it is enabled).
    #[allow(dead_code)]
    pub(crate) fn dev_panel(mut self, panel: DevPanel) -> Self {
        if cfg!(debug_assertions) {
            self.panels().lock().push(panel);
        }
        self
    }

    fn panels(&mut self) -> Panels {
        self.extensions_mut()
            .get_or_insert_default::<Panels>()
            .clone()
    }
}

/// Keep framework routes (the console, the dev inbox, signed file links)
/// out of the OpenAPI spec and the typed clients generated from it.
#[allow(dead_code)]
pub(crate) fn internal(routes: Routes) -> Routes {
    #[cfg(feature = "openapi")]
    let routes = routes.meta_all(crate::openapi::OperationDoc::hidden_route());
    routes
}

/// Only this machine may see the console (requests without a peer address
/// come from [`TestClient`](crate::test::TestClient)).
/// A request from this machine, not relayed by a proxy on it: the peer is a
/// loopback address and no forwarding header names another client. (A
/// reverse proxy on the same host connects from loopback too.) The host it
/// was sent to must be a loopback name too, so a web page can't reach it by
/// pointing its own domain at 127.0.0.1 (DNS rebinding).
pub(crate) fn local(req: &Request) -> bool {
    const FORWARDED: [&str; 3] = ["forwarded", "x-forwarded-for", "x-real-ip"];
    req.remote_addr().is_some_and(|a| a.ip().is_loopback())
        && !FORWARDED.iter().any(|h| req.headers().contains_key(*h))
        && loopback_host(req)
}

/// Whether `Host` (or the HTTP/2 authority) names this machine. Browsers
/// always send one; a request without any is not from a web page.
fn loopback_host(req: &Request) -> bool {
    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()));
    let Some(host) = host else {
        return !req.headers().contains_key(http::header::HOST);
    };
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn act(req: Request, panels: Panels) -> Response {
    if !local(&req) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let id = req.param("panel").unwrap_or_default().into_owned();
    let action = panels
        .lock()
        .iter()
        .find(|p| p.id == id)
        .and_then(|p| p.action.clone());
    let Some((_, action)) = action else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match action(&req).await {
        Ok(_) => Redirect::to(DEV_CONSOLE_PATH).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn console(
    req: &Request,
    routes: &[RouteRow],
    recent: &Recent,
    errors: &ErrorLog,
    panels: &Panels,
) -> Response {
    if !local(req) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut page = String::from(
        "<!doctype html><meta charset=utf-8><title>Dev console</title>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <style>body{font:14px/1.5 system-ui,sans-serif;max-width:64rem;margin:2rem auto;padding:0 1rem}\
         section{border:1px solid #8884;border-radius:8px;padding:0 1rem 1rem;margin:1rem 0;overflow-x:auto}\
         table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:.25rem .5rem;border-bottom:1px solid #8882}\
         code{font:13px ui-monospace,monospace}small,.dim{color:#888}.s4{color:#b60}.s5{color:#c22}\
         button{font:inherit;padding:.25rem .75rem}</style>\
         <h1>Dev console</h1><p><small>Debug builds only. Reload for fresh data.</small></p>",
    );

    let custom: Vec<DevPanel> = panels.lock().clone();
    for panel in custom {
        let body = (panel.render)(req).await;
        let _ = write!(page, "<section><h2>{}</h2>{body}", escape(&panel.title));
        if let Some((label, _)) = &panel.action {
            let _ = write!(
                page,
                "<form method=post action=\"{DEV_CONSOLE_PATH}/{}\"><button>{}</button></form>",
                escape(&panel.id),
                escape(label)
            );
        }
        page.push_str("</section>");
    }

    let errs = errors.lock();
    if !errs.is_empty() {
        page.push_str(
            "<section><h2>Recent errors</h2><table><tr><th>Code<th>Request<th>Error</tr>",
        );
        for e in errs.iter().rev() {
            let code = e.code.unwrap_or("");
            let fix = e.fix.as_deref().map_or_else(String::new, |f| {
                format!("<br><small>fix: <code>{}</code></small>", escape(f))
            });
            let _ = write!(
                page,
                "<tr><td><code>{code}</code><td class=s{}>{} <code>{}</code> {}<td>{}{fix}</tr>",
                e.status / 100,
                escape(&e.method),
                escape(&e.path),
                e.status,
                escape(&e.message)
            );
        }
        page.push_str("</table></section>");
    }
    drop(errs);

    page.push_str("<section><h2>Recent requests</h2>");
    let hits = recent.lock();
    if hits.is_empty() {
        page.push_str("<p class=dim>None yet.</p>");
    } else {
        page.push_str("<table><tr><th>Status<th>Method<th>Path<th>Time</tr>");
        for hit in hits.iter().rev() {
            let class = match hit.status {
                500.. => " class=s5",
                400.. => " class=s4",
                _ => "",
            };
            let _ = write!(
                page,
                "<tr><td{class}>{}<td>{}<td><code>{}</code><td>{}</tr>",
                hit.status,
                escape(&hit.method),
                escape(&hit.path),
                duration(hit.micros)
            );
        }
        page.push_str("</table>");
    }
    drop(hits);
    page.push_str("</section>");

    let _ = write!(page, "<section><h2>Routes ({})</h2><table>", routes.len());
    page.push_str("<tr><th>Method<th>Path<th>Summary</tr>");
    for route in routes {
        let path = escape(&route.path);
        let link = route.method == "GET" && !route.path.contains('{');
        let path = if link {
            format!("<a href=\"{path}\"><code>{path}</code></a>")
        } else {
            format!("<code>{path}</code>")
        };
        let mut summary = route.summary.as_deref().map(escape).unwrap_or_default();
        if !route.tags.is_empty() {
            let _ = write!(
                summary,
                " <small>{}</small>",
                escape(&route.tags.join(", "))
            );
        }
        let _ = write!(page, "<tr><td>{}<td>{path}<td>{summary}</tr>", route.method);
    }
    page.push_str("</table></section>");
    Html(page).into_response()
}

/// The console's data as JSON, newest first (no serde needed).
fn api(req: &Request, routes: &[RouteRow], recent: &Recent, errors: &ErrorLog) -> Response {
    use crate::error::push_json_str as string;
    if !local(req) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let opt = |out: &mut String, key: &str, value: Option<&str>| {
        if let Some(v) = value {
            let _ = write!(out, ",\"{key}\":");
            string(out, v);
        }
    };
    let mut out = String::from("{\"requests\":[");
    for (i, hit) in recent.lock().iter().rev().enumerate() {
        out.push_str(if i == 0 {
            "{\"method\":"
        } else {
            ",{\"method\":"
        });
        string(&mut out, &hit.method);
        out.push_str(",\"path\":");
        string(&mut out, &hit.path);
        let _ = write!(out, ",\"status\":{},\"micros\":{}", hit.status, hit.micros);
        opt(&mut out, "code", hit.code);
        out.push('}');
    }
    out.push_str("],\"errors\":[");
    for (i, e) in errors.lock().iter().rev().enumerate() {
        let _ = write!(
            out,
            "{}{{\"at_ms\":{},\"method\":",
            if i == 0 { "" } else { "," },
            e.at_ms
        );
        string(&mut out, &e.method);
        out.push_str(",\"path\":");
        string(&mut out, &e.path);
        let _ = write!(out, ",\"status\":{}", e.status);
        opt(&mut out, "code", e.code);
        opt(&mut out, "error_code", e.error_code.as_deref());
        out.push_str(",\"message\":");
        string(&mut out, &e.message);
        opt(&mut out, "fix", e.fix.as_deref());
        opt(&mut out, "location", e.location.as_deref());
        out.push('}');
    }
    out.push_str("],\"routes\":[");
    for (i, r) in routes.iter().enumerate() {
        out.push_str(if i == 0 {
            "{\"method\":"
        } else {
            ",{\"method\":"
        });
        string(&mut out, &r.method);
        out.push_str(",\"path\":");
        string(&mut out, &r.path);
        out.push('}');
    }
    out.push_str("]}");
    let mut res = Response::new(out.into());
    res.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    res
}

fn duration(micros: u128) -> String {
    if micros < 1000 {
        format!("{micros} µs")
    } else {
        format!("{:.1} ms", micros as f64 / 1000.0)
    }
}

/// Escape text for HTML content and attribute values.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
