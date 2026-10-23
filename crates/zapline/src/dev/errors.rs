//! The dev error page: in a debug build with dev errors on, a 500 or a
//! panic answers this machine with what went wrong and how to fix it,
//! instead of `{"error":"internal server error"}`.

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard, Once};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use http::header::{self, HeaderMap, HeaderValue};

use super::{escape, local, Hit, Recent, DEV_CONSOLE_PATH, RECENT};
use crate::codes;
use crate::error::{push_json_str, DevDiagnostic, ErrorReport};
use crate::middleware::Next;
use crate::response::{IntoResponse, Response};
use crate::{App, Error, Request, StatusCode};

/// How many recent errors the console keeps.
const ERRORS: usize = 20;

/// Header that names the diagnostic code of a rejected request (4xx) in dev.
pub const DIAGNOSTIC_HEADER: &str = "x-zapline-diagnostic";

/// Set by [`App::dev_errors`] (and [`App::dev_console`]).
#[derive(Clone, Copy)]
pub(super) struct DevErrorsOn;

/// One error as the console and `zapline mcp` show it.
#[derive(Clone)]
pub(super) struct ErrorEntry {
    pub(super) at_ms: u128,
    pub(super) method: String,
    pub(super) path: String,
    pub(super) status: u16,
    /// The diagnostic code (`ZL0002`).
    pub(super) code: Option<&'static str>,
    /// The client-facing code (`missing_dependency`).
    pub(super) error_code: Option<String>,
    pub(super) message: String,
    pub(super) fix: Option<String>,
    pub(super) location: Option<String>,
}

#[derive(Clone, Default)]
pub(super) struct ErrorLog(Arc<Mutex<VecDeque<ErrorEntry>>>);

impl ErrorLog {
    pub(super) fn lock(&self) -> MutexGuard<'_, VecDeque<ErrorEntry>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, entry: ErrorEntry) {
        let mut log = self.lock();
        if log.len() == ERRORS {
            log.pop_front();
        }
        log.push_back(entry);
    }
}

impl App {
    /// Answer 500s and panics with a page that says what broke and how to
    /// fix it: the real error with its causes, its [diagnostic
    /// code](crate::codes) and fix, the panic location, the request, and a
    /// **Copy fix prompt** block to paste into an AI assistant. JSON clients
    /// (no `text/html` in `Accept`) get `application/problem+json` with the
    /// same fields. A rejected request (a 4xx such as `invalid_json`) keeps
    /// its body and gains an `x-zapline-diagnostic: ZL0062` header.
    ///
    /// Debug builds only, and only for requests from this machine: in a
    /// release build this does nothing, and other clients get the usual
    /// generic error. It is also on whenever `ZAPLINE_DEV=1` (which
    /// `zapline dev` sets) and with [`App::dev_console`], which lists the
    /// recent errors.
    ///
    /// ```
    /// use zapline::prelude::*;
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    /// let app = App::new()
    ///     .dev_errors()
    ///     .get("/", |repo: Dep<String>| async move { repo.to_string() });
    /// let res = app.test().get("/").header("accept", "application/json").send().await;
    /// # #[cfg(debug_assertions)]
    /// res.assert_status(500)
    ///     .assert_header("content-type", "application/problem+json")
    ///     .assert_contains(r#""code":"ZL0002""#); // no provider for a dependency
    /// # });
    /// ```
    pub fn dev_errors(mut self) -> Self {
        if cfg!(debug_assertions) {
            self.extensions_mut().insert(DevErrorsOn);
        }
        self
    }
}

/// Install the dev error middleware, outermost, if dev errors are on. Runs
/// once, when the app is built.
pub(crate) fn install(app: &mut App) {
    if !cfg!(debug_assertions)
        || (app.extensions().get::<DevErrorsOn>().is_none() && !crate::error::dev_mode())
    {
        return;
    }
    crate::error::start_diagnosing();
    remember_panic_locations();
    let recent = app.extensions().get::<Recent>().cloned();
    let errors = app.extensions().get::<ErrorLog>().cloned();
    let state = Arc::new((recent, errors));
    let mw = move |req: Request, next: Next| {
        let state = state.clone();
        async move { handle(req, next, &state.0, &state.1).await }
    };
    app.middleware.insert(0, Arc::new(mw));
}

/// What the page shows about the request, captured before it is consumed.
struct Seen {
    method: String,
    uri: String,
    headers: HeaderMap,
    html: bool,
}

async fn handle(
    req: Request,
    next: Next,
    recent: &Option<Recent>,
    errors: &Option<ErrorLog>,
) -> Response {
    let console = req.path().starts_with(DEV_CONSOLE_PATH);
    let seen = local(&req).then(|| Seen {
        method: req.method().to_string(),
        uri: req.uri().to_string(),
        headers: req.headers().clone(),
        html: accepts_html(req.headers()),
    });
    let (method, path) = (req.method().to_string(), req.uri().to_string());
    let start = Instant::now();
    let res = match catch_unwind(AssertUnwindSafe(|| next.run(req))) {
        Ok(fut) => match AssertUnwindSafe(fut).catch_unwind().await {
            Ok(res) => res,
            Err(payload) => panicked(payload.as_ref(), &method, &path),
        },
        Err(payload) => panicked(payload.as_ref(), &method, &path),
    };
    let micros = start.elapsed().as_micros();
    let diag = res.extensions().get::<DevDiagnostic>().cloned();
    let status = res.status();
    // Only developer mistakes: server errors, and rejections the framework
    // recognized. A handler's own `Error::not_found()` is not one.
    let diag = diag.filter(|d| status.is_server_error() || d.code.is_some());

    if let (Some(recent), false) = (recent, console) {
        let mut log = recent.lock();
        if log.len() == RECENT {
            log.pop_front();
        }
        log.push_back(Hit {
            method: method.clone(),
            path: path.clone(),
            status: status.as_u16(),
            micros,
            code: diag.as_ref().and_then(|d| d.code),
        });
    }
    let Some(diag) = diag else {
        return res;
    };
    if let (Some(errors), false) = (errors, console) {
        errors.push(ErrorEntry {
            at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
            method,
            path,
            status: status.as_u16(),
            code: diag.code,
            error_code: res
                .extensions()
                .get::<ErrorReport>()
                .and_then(|r| r.code.clone()),
            message: diag.message.clone(),
            fix: diag.fix.clone(),
            location: diag.location.clone(),
        });
    }
    let Some(seen) = seen else {
        return res;
    };
    if !status.is_server_error() {
        let mut res = res;
        if let Some(code) = diag.code {
            res.headers_mut()
                .insert(DIAGNOSTIC_HEADER, HeaderValue::from_static(code));
        }
        return res;
    }
    let error_code = res
        .extensions()
        .get::<ErrorReport>()
        .and_then(|r| r.code.clone());
    let mut page = if seen.html {
        let mut page = crate::response::Html(html_page(status, &diag, &seen)).into_response();
        *page.status_mut() = status;
        page
    } else {
        problem_json(status, &diag, error_code.as_deref(), &seen)
    };
    // Keep the original response's other headers (request IDs, Retry-After).
    for (name, value) in res.headers() {
        if name != header::CONTENT_TYPE
            && name != header::CONTENT_LENGTH
            && !page.headers().contains_key(name)
        {
            page.headers_mut().append(name.clone(), value.clone());
        }
    }
    if let Some(code) = diag.code {
        page.headers_mut()
            .insert(DIAGNOSTIC_HEADER, HeaderValue::from_static(code));
    }
    page.extensions_mut().insert(diag);
    page
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.contains("text/html"))
}

/// The 500 for a panic that nothing else caught, with where it happened.
fn panicked(payload: &(dyn Any + Send), method: &str, path: &str) -> Response {
    let message = crate::middleware::catch_panic::panic_message(payload);
    let location = take_panic_location();
    crate::middleware::catch_panic::log_panic_str(method, path, &message);
    panic_response(message, location)
}

/// `500 {"error":"internal server error","code":"panic"}`, carrying the
/// panic for the dev page.
pub(crate) fn panic_response(message: String, location: Option<String>) -> Response {
    let mut res = Error::internal("internal server error")
        .with_code("panic")
        .into_response();
    if crate::error::diagnosing() {
        let code = "ZL0080";
        res.extensions_mut().insert(DevDiagnostic {
            message: format!("panicked: {message}"),
            code: Some(code),
            fix: codes::lookup(code).map(|c| c.fix_for(None)),
            location,
        });
    }
    res
}

thread_local! {
    /// Where the last panic on this thread happened, for the dev page.
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Chain a panic hook that remembers each panic's location (the previous
/// hook still prints it).
fn remember_panic_locations() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(loc) = info.location() {
                let at = format!("{}:{}:{}", loc.file(), loc.line(), loc.column());
                let _ = LAST_PANIC.try_with(|p| *p.borrow_mut() = Some(at));
            }
            previous(info);
        }));
    });
}

/// The location of the panic just caught on this thread.
pub(crate) fn take_panic_location() -> Option<String> {
    LAST_PANIC
        .try_with(|p| p.borrow_mut().take())
        .ok()
        .flatten()
}

/// Headers whose values never appear on the page.
fn secret(name: &str) -> bool {
    matches!(
        name,
        "authorization" | "cookie" | "proxy-authorization" | "x-api-key" | "x-csrf-token"
    )
}

/// The text to paste into an AI assistant.
fn prompt(status: StatusCode, diag: &DevDiagnostic, seen: &Seen) -> String {
    let mut out = format!(
        "My zapline app answered {} {} with {status}.\n",
        seen.method, seen.uri
    );
    let entry = diag.code.and_then(codes::lookup);
    match entry {
        Some(c) => {
            let _ = writeln!(out, "Error {} ({}): {}", c.code, c.title, diag.message);
        }
        None => {
            let _ = writeln!(out, "Error: {}", diag.message);
        }
    }
    if let Some(at) = &diag.location {
        let _ = writeln!(out, "Panicked at: {at}");
    }
    if let Some(c) = entry {
        let _ = writeln!(out, "Cause: {}", c.explanation);
    }
    if let Some(fix) = &diag.fix {
        let _ = writeln!(out, "Suggested fix: {fix}");
    }
    out.push_str(
        "Fix the code so this request succeeds, and add a test with `app.test()` that sends it.",
    );
    out
}

fn html_page(status: StatusCode, diag: &DevDiagnostic, seen: &Seen) -> String {
    let entry = diag.code.and_then(codes::lookup);
    let title = entry.map_or_else(
        || status.canonical_reason().unwrap_or("Error").to_owned(),
        |c| format!("{} · {}", c.code, c.title),
    );
    let mut page = format!(
        "<!doctype html><meta charset=utf-8><title>{}</title>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <style>body{{font:14px/1.5 system-ui,sans-serif;max-width:64rem;margin:2rem auto;padding:0 1rem}}\
         h1{{font-size:1.4rem;margin:.5rem 0}}.status{{color:#c22;font-weight:600}}\
         pre{{background:#8881;border:1px solid #8884;border-radius:8px;padding:.75rem;overflow-x:auto;white-space:pre-wrap}}\
         code,pre{{font:13px ui-monospace,monospace}}section{{margin:1.5rem 0}}\
         table{{border-collapse:collapse;width:100%}}td,th{{text-align:left;padding:.2rem .5rem;border-bottom:1px solid #8882;vertical-align:top}}\
         .dim{{color:#888}}button{{font:inherit;padding:.25rem .75rem}}</style>\
         <p class=status>{} · dev error page</p><h1>{}</h1>",
        escape(&title),
        status,
        escape(&title)
    );
    let _ = write!(page, "<pre id=message>{}</pre>", escape(&diag.message));
    if let Some(at) = &diag.location {
        let _ = write!(
            page,
            "<p>Panicked at <code id=location>{}</code></p>",
            escape(at)
        );
    }
    if let Some(c) = entry {
        let _ = write!(
            page,
            "<section><h2>Why</h2><p>{}</p>",
            escape(c.explanation)
        );
        if let Some(fix) = &diag.fix {
            let _ = write!(
                page,
                "<h2>Fix</h2><pre id=fix><code>{}</code></pre>",
                escape(fix)
            );
        }
        let _ = write!(
            page,
            "<p class=dim>More: <code>zapline explain {}</code> · <a href=\"{}\">docs</a></p></section>",
            c.code,
            escape(&codes::docs_url(c.code))
        );
    }
    let _ = write!(
        page,
        "<section><h2>Copy fix prompt</h2><pre id=prompt>{}</pre>\
         <button onclick=\"navigator.clipboard.writeText(document.getElementById('prompt').innerText)\
         .then(()=>this.textContent='Copied')\">Copy fix prompt</button></section>",
        escape(&prompt(status, diag, seen))
    );
    let _ = write!(
        page,
        "<section><h2>Request</h2><p><code>{} {}</code></p><table>",
        escape(&seen.method),
        escape(&seen.uri)
    );
    for (name, value) in &seen.headers {
        let shown = if secret(name.as_str()) {
            "[hidden]".to_owned()
        } else {
            String::from_utf8_lossy(value.as_bytes()).into_owned()
        };
        let _ = write!(
            page,
            "<tr><th>{}<td><code>{}</code></tr>",
            escape(name.as_str()),
            escape(&shown)
        );
    }
    let _ = write!(
        page,
        "</table></section><p class=dim>Shown only in debug builds, to this machine. \
         <a href=\"{DEV_CONSOLE_PATH}\">Dev console</a></p>"
    );
    page
}

/// RFC 9457 `application/problem+json` with the diagnostic fields.
fn problem_json(
    status: StatusCode,
    diag: &DevDiagnostic,
    error_code: Option<&str>,
    seen: &Seen,
) -> Response {
    let entry = diag.code.and_then(codes::lookup);
    let mut body = String::from("{\"type\":");
    match entry {
        Some(c) => push_json_str(&mut body, &codes::docs_url(c.code)),
        None => body.push_str("\"about:blank\""),
    }
    body.push_str(",\"title\":");
    push_json_str(
        &mut body,
        entry.map_or(status.canonical_reason().unwrap_or("Error"), |c| c.title),
    );
    let _ = write!(body, ",\"status\":{}", status.as_u16());
    body.push_str(",\"detail\":");
    push_json_str(&mut body, &diag.message);
    // `error` too, so clients that read the usual `{"error": ..}` still do.
    body.push_str(",\"error\":");
    push_json_str(&mut body, &diag.message);
    let mut field = |key: &str, value: Option<&str>| {
        if let Some(v) = value {
            let _ = write!(body, ",\"{key}\":");
            push_json_str(&mut body, v);
        }
    };
    field("code", diag.code);
    field("error_code", error_code);
    field("fix", diag.fix.as_deref());
    field("location", diag.location.as_deref());
    field("request", Some(&format!("{} {}", seen.method, seen.uri)));
    body.push('}');
    let mut res = Response::new(body.into());
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    res
}
