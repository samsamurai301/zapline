//! Recording request/response pairs in the dev console, to save as
//! snapshot tests (see [`TestClient::replay_snapshots`](crate::test::TestClient::replay_snapshots)).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_TYPE, UPGRADE};

use super::{escape, local, DEV_CONSOLE_PATH};
use crate::body::{Body, ReqBody};
use crate::middleware::{Next, SameOrigin};
use crate::response::{IntoResponse, Redirect, Response};
use crate::router::Routes;
use crate::snapshot::{self, Exchange, RecordedRequest, RecordedResponse, SnapshotFile, BODY_CAP};
use crate::{Error, Request, StatusCode};

/// Set to `1` to record from startup, and keep the recording in
/// `target/zapline/recording.jsonl` so it survives restarts. `zapline dev
/// --record` sets it.
///
/// ```sh
/// ZAPLINE_RECORD=1 cargo run
/// ```
pub const ENV_RECORD: &str = "ZAPLINE_RECORD";

/// The most exchanges a recording keeps (the oldest go first).
const MAX_EXCHANGES: usize = 1000;

/// Under the console path.
const RECORDINGS: &str = "/_dev/recordings";

#[derive(Default)]
struct State {
    exchanges: VecDeque<Exchange>,
    /// Requests not recorded because their body was streamed or too large.
    skipped: usize,
}

/// The recording, shared by the console middleware and its routes.
#[derive(Clone)]
pub(crate) struct Recorder(Arc<Inner>);

struct Inner {
    on: AtomicBool,
    state: Mutex<State>,
    /// Where the recording is mirrored (with [`ENV_RECORD`]).
    file: Option<PathBuf>,
}

impl Recorder {
    /// Recording from the start with [`ENV_RECORD`]`=1` (resuming what an
    /// earlier run of the app recorded), otherwise off until the console
    /// turns it on.
    pub(crate) fn from_env() -> Self {
        let on = std::env::var_os(ENV_RECORD).is_some_and(|v| v == "1" || v == "true");
        let file = on.then(|| {
            snapshot::project_root()
                .join("target")
                .join("zapline")
                .join("recording.jsonl")
        });
        let mut state = State::default();
        if let Some(text) = file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()) {
            state.exchanges = text
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
        }
        Recorder(Arc::new(Inner {
            on: AtomicBool::new(on),
            state: Mutex::new(state),
            file,
        }))
    }

    #[inline]
    pub(crate) fn is_on(&self) -> bool {
        self.0.on.load(Ordering::Relaxed)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run the request, recording it and the response.
    pub(crate) async fn record(&self, mut req: Request, next: Next) -> Response {
        if req.headers().contains_key(UPGRADE) || req.path() == "/favicon.ico" {
            return next.run(req).await;
        }
        let body = match capture_body(&mut req).await {
            Ok(Some(body)) => body,
            Ok(None) => {
                self.lock().skipped += 1;
                return next.run(req).await;
            }
            Err(e) => return e.into_response(),
        };
        let request = RecordedRequest::new(req.method(), req.uri(), req.headers(), &body);
        let res = next.run(req).await;
        let (parts, body) = res.into_parts();
        if is_asset(&parts.headers) {
            return Response::from_parts(parts, body);
        }
        let bytes = match &body {
            Body::Empty | Body::Full(None) => Some(Bytes::new()),
            Body::Full(Some(b)) => Some(b.clone()),
            Body::Stream(_) => None,
        };
        let response =
            RecordedResponse::new(parts.status.as_u16(), &parts.headers, bytes.as_deref());
        self.push(Exchange { request, response });
        Response::from_parts(parts, body)
    }

    fn push(&self, exchange: Exchange) {
        let mut state = self.lock();
        if let Some(file) = &self.0.file {
            let line = serde_json::to_string(&exchange).unwrap_or_default();
            let appended = file
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(file)
                })
                .and_then(|mut f| writeln!(f, "{line}"));
            if let Err(e) = appended {
                #[cfg(feature = "tracing")]
                tracing::warn!("cannot write the recording to {}: {e}", file.display());
                #[cfg(not(feature = "tracing"))]
                let _ = e;
            }
        }
        if state.exchanges.len() == MAX_EXCHANGES {
            state.exchanges.pop_front();
        }
        state.exchanges.push_back(exchange);
    }

    fn clear(&self) {
        let mut state = self.lock();
        *state = State::default();
        if let Some(file) = &self.0.file {
            let _ = std::fs::remove_file(file);
        }
    }

    fn snapshot(&self) -> SnapshotFile {
        SnapshotFile::new(self.lock().exchanges.iter().cloned().collect())
    }

    /// The console section: state, controls and the latest exchanges.
    pub(crate) fn section(&self, req: &Request) -> String {
        let state = self.lock();
        let on = self.is_on();
        let mut html = String::from("<section><h2>Recording</h2>");
        if let Some(saved) = req
            .query_string()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("saved=")))
        {
            let saved = crate::request::decode(saved);
            let _ = write!(
                html,
                "<p><strong>Saved <code>{}</code>.</strong> Run it with <code>cargo test</code>.</p>",
                escape(&saved)
            );
        }
        let _ = write!(
            html,
            "<p>{} · {} request(s) recorded",
            if on {
                "<strong>Recording</strong>"
            } else {
                "Not recording"
            },
            state.exchanges.len()
        );
        if state.skipped > 0 {
            let _ = write!(
                html,
                " · {} skipped (request body streamed or over 64 KiB)",
                state.skipped
            );
        }
        html.push('.');
        if on && self.0.file.is_none() {
            html.push_str(
                " <small>Kept in memory: a restart clears it \
                 (<code>ZAPLINE_RECORD=1</code> keeps it across restarts).</small>",
            );
        }
        html.push_str("</p><p>");
        let toggle = if on {
            ("stop", "Stop recording")
        } else {
            ("start", "Start recording")
        };
        let _ = write!(
            html,
            "<form method=post action=\"{RECORDINGS}/{}\" style=display:inline><button>{}</button></form> ",
            toggle.0, toggle.1
        );
        if !state.exchanges.is_empty() {
            let _ = write!(
                html,
                "<form method=post action=\"{RECORDINGS}/save\" style=display:inline>\
                 <input name=name value=recorded pattern=\"[A-Za-z0-9_\\-]+\" required \
                 aria-label=\"Snapshot name\"> <button>Save as snapshot test</button></form> \
                 <form method=post action=\"{RECORDINGS}/clear\" style=display:inline>\
                 <button>Clear</button></form>"
            );
        }
        html.push_str("</p>");
        if state.exchanges.is_empty() {
            html.push_str(
                "<p class=dim>Record requests (method, path, body, response) to save them as \
                 tests that replay with <code>app.test().replay_snapshots(..)</code>. \
                 Authorization and cookie values are never recorded.</p>",
            );
        } else {
            html.push_str("<table><tr><th>Status<th>Method<th>Path</tr>");
            for ex in state.exchanges.iter().rev().take(20) {
                let path = match &ex.request.query {
                    Some(q) => format!("{}?{q}", ex.request.path),
                    None => ex.request.path.clone(),
                };
                let _ = write!(
                    html,
                    "<tr><td>{}<td>{}<td><code>{}</code></tr>",
                    ex.response.status,
                    escape(&ex.request.method),
                    escape(&path)
                );
            }
            html.push_str("</table>");
        }
        html.push_str("</section>");
        html
    }

    /// The recording routes under `/_dev/recordings`.
    pub(crate) fn routes(&self) -> Routes {
        let list = {
            let rec = self.clone();
            move |req: Request| {
                let rec = rec.clone();
                async move {
                    if !local(&req) {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    crate::Json(rec.snapshot()).into_response()
                }
            }
        };
        let control = {
            let rec = self.clone();
            move |req: Request| {
                let rec = rec.clone();
                async move { control(req, rec).await }
            }
        };
        Routes::new()
            .get(RECORDINGS, list)
            .post(&format!("{RECORDINGS}/{{action}}"), control)
    }
}

/// `start`, `stop`, `clear` and `save` (form field `name`).
async fn control(mut req: Request, rec: Recorder) -> Response {
    // Same-origin too: a web page could otherwise post a form here.
    if !local(&req) || !SameOrigin::new().allows(&req) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match req.param("action").as_deref() {
        Some("start") => rec.0.on.store(true, Ordering::Relaxed),
        Some("stop") => rec.0.on.store(false, Ordering::Relaxed),
        Some("clear") => rec.clear(),
        Some("save") => {
            return save(&mut req, &rec)
                .await
                .unwrap_or_else(|e| e.into_response())
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    }
    Redirect::to(DEV_CONSOLE_PATH).into_response()
}

async fn save(req: &mut Request, rec: &Recorder) -> crate::Result<Response> {
    let wants_json = req
        .headers()
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    let body = req.body_bytes().await?;
    let name = form_value(&body, "name")
        .or_else(|| {
            req.query_string()
                .and_then(|q| form_value(q.as_bytes(), "name"))
        })
        .unwrap_or_default();
    if !snapshot::valid_name(&name) {
        return Err(Error::bad_request(
            "the snapshot name must be 1 to 64 letters, digits, `-` or `_`",
        )
        .with_code("invalid_name"));
    }
    let snapshot = rec.snapshot();
    if snapshot.exchanges.is_empty() {
        return Err(Error::conflict(
            "nothing recorded yet: start recording in the dev console (/_dev), \
             or run the app with ZAPLINE_RECORD=1 (`zapline dev --record`), and send some requests",
        )
        .with_code("nothing_recorded"));
    }
    let saved = tokio::task::spawn_blocking(move || {
        snapshot::save(&snapshot::project_root(), &name, &snapshot)
    })
    .await
    .map_err(|e| Error::internal(e.to_string()))?
    .map_err(|e| Error::internal(format!("cannot write the snapshot: {e}")))?;
    rec.clear();
    if wants_json {
        return Ok(crate::Json(saved).into_response());
    }
    let to = format!(
        "{DEV_CONSOLE_PATH}?saved={}",
        percent_encoding::utf8_percent_encode(&saved.file, percent_encoding::NON_ALPHANUMERIC)
    );
    Ok(Redirect::to(&to).into_response())
}

/// The first value of `name` in a urlencoded form.
fn form_value(body: &[u8], name: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    text.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(&s.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        };
        (decode(k) == name).then(|| decode(v).trim().to_owned())
    })
}

/// Buffer the request body so it can be both recorded and handled. `None`
/// (body left alone) when it is streamed without a length or over the cap.
async fn capture_body(req: &mut Request) -> crate::Result<Option<Bytes>> {
    match req.body.take() {
        None => Ok(Some(Bytes::new())),
        Some(ReqBody::Full(b)) => {
            let fits = b.len() <= BODY_CAP;
            let copy = b.clone();
            req.body = Some(ReqBody::Full(b));
            Ok(fits.then_some(copy))
        }
        // A streamed body (HTTP/1.1, HTTP/2 or HTTP/3).
        Some(body) => {
            let fits = body
                .size_hint()
                .upper()
                .is_some_and(|n| n <= BODY_CAP as u64);
            if !fits {
                req.body = Some(body);
                return Ok(None);
            }
            let b = body.collect_limited(BODY_CAP).await?;
            req.body = Some(ReqBody::Full(b.clone()));
            Ok(Some(b))
        }
    }
}

/// Static files (styles, scripts, images, fonts) are not app behavior worth
/// a test.
fn is_asset(headers: &http::HeaderMap) -> bool {
    let Some(ct) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let ct = ct.to_ascii_lowercase();
    [
        "image/",
        "font/",
        "audio/",
        "video/",
        "text/css",
        "text/javascript",
        "application/javascript",
        "application/wasm",
    ]
    .iter()
    .any(|p| ct.starts_with(p))
}
