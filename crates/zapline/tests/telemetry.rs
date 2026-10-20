//! Request tracing (`Trace`) and subscriber construction (`Telemetry`).
//!
//! Each test installs its own scoped subscriber writing JSON into a buffer,
//! then inspects the captured lines.

#![cfg(feature = "tracing")]

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use zapline::prelude::*;
use zapline::telemetry::{
    self, tracing, Format, Level, StatusClass, Telemetry, TelemetryError, Trace,
};

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn lines(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
            .collect()
    }
    /// Completion events only.
    fn completions(&self) -> Vec<Value> {
        self.lines()
            .into_iter()
            .filter(|l| l["message"] == "request completed")
            .collect()
    }
}

/// A JSON subscriber at `filter`, installed for the current thread.
fn capture(filter: &str) -> (Buf, tracing::dispatcher::DefaultGuard) {
    let buf = Buf::default();
    let b = buf.clone();
    let dispatch = Telemetry::new()
        .json()
        .env_filter(filter)
        .writer(move || b.clone())
        .into_dispatch()
        .unwrap();
    let guard = tracing::dispatcher::set_default(&dispatch);
    (buf, guard)
}

fn app() -> App {
    App::new()
        .get("/users/{id}", |Path(id): Path<u32>| async move {
            tracing::info!(user = id, "loading user");
            format!("user {id}")
        })
        .get("/fail", || async { Error::internal("db down") })
        .get("/bad", || async { Error::bad_request("nope") })
        .get("/with-id", || async {
            (
                [(
                    zapline::header::HeaderName::from_static("x-request-id"),
                    HeaderValue::from_static("resp-42"),
                )],
                "ok",
            )
        })
        .get("/health/live", || async { "up" })
}

// ---------------------------------------------------------------------------
// Trace

#[tokio::test]
async fn plugin_records_span_fields_and_completion_event() {
    let (buf, _g) = capture("info");
    let c = app().plugin(Trace::new()).test();
    let r = c
        .get("/users/7")
        .remote_addr("10.1.2.3:4000".parse().unwrap())
        .send()
        .await;
    assert_eq!(r.status(), 200);

    let lines = buf.lines();
    // The handler's own log line is nested in the request span, which
    // already carries the matched route.
    let inner = lines
        .iter()
        .find(|l| l["message"] == "loading user")
        .unwrap();
    assert_eq!(inner["user"], 7);
    assert_eq!(inner["span"]["name"], "request");
    assert_eq!(inner["span"]["http.route"], "/users/{id}");

    let done = buf.completions();
    assert_eq!(done.len(), 1, "{lines:?}");
    let e = &done[0];
    assert_eq!(e["level"], "INFO");
    assert_eq!(e["target"], telemetry::TARGET);
    assert_eq!(e["http.status_code"], 200);
    assert!(e["latency_ms"].as_f64().unwrap() >= 0.0);
    let span = &e["span"];
    assert_eq!(span["name"], "request");
    assert_eq!(span["http.method"], "GET");
    assert_eq!(span["http.route"], "/users/{id}");
    assert_eq!(span["url.path"], "/users/7");
    assert_eq!(span["client.address"], "10.1.2.3");
    // Outcome fields are recorded on the span after the event, so text logs
    // don't print them twice.
    assert!(span.get("http.status_code").is_none());
}

/// A minimal subscriber recording every span field, to check what the
/// request span ends up carrying.
/// Span name and every recorded `(field, value)`, by span id - 1.
type SpanLog = Vec<(String, Vec<(String, String)>)>;

#[derive(Default)]
struct SpanRecorder {
    spans: Mutex<SpanLog>,
}

struct Fields<'a>(&'a mut Vec<(String, String)>);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        self.0.push((f.name().to_owned(), format!("{v:?}")));
    }
    fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
        self.0.push((f.name().to_owned(), v.to_owned()));
    }
}

impl tracing::Subscriber for SpanRecorder {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut fields = Vec::new();
        attrs.record(&mut Fields(&mut fields));
        let mut spans = self.spans.lock().unwrap();
        spans.push((attrs.metadata().name().to_owned(), fields));
        tracing::span::Id::from_u64(spans.len() as u64)
    }
    fn record(&self, id: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut spans = self.spans.lock().unwrap();
        let fields = &mut spans[id.into_u64() as usize - 1].1;
        values.record(&mut Fields(fields));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn request_span_carries_all_fields() {
    let recorder = Arc::new(SpanRecorder::default());
    let dispatch = tracing::Dispatch::from(recorder.clone());
    let _g = tracing::dispatcher::set_default(&dispatch);
    let c = app().plugin(Trace::new()).test();
    c.get("/users/9").header("x-request-id", "abc").send().await;
    c.get("/missing").send().await;

    let spans = recorder.spans.lock().unwrap();
    let requests: Vec<&Vec<(String, String)>> = spans
        .iter()
        .filter(|(n, _)| n == "request")
        .map(|(_, f)| f)
        .collect();
    assert_eq!(requests.len(), 2);
    let get = |fields: &Vec<(String, String)>, k: &str| {
        fields
            .iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("span has no {k}: {fields:?}"))
    };
    let ok = requests[0];
    assert_eq!(get(ok, "http.method"), "GET");
    assert_eq!(get(ok, "http.route"), "/users/{id}");
    assert_eq!(get(ok, "url.path"), "/users/9");
    assert_eq!(get(ok, "client.address"), "127.0.0.1");
    assert_eq!(get(ok, "request_id"), "abc");
    assert_eq!(get(ok, "http.status_code"), "200");
    assert!(get(ok, "latency_ms").parse::<f64>().unwrap() >= 0.0);
    let missing = requests[1];
    assert_eq!(get(missing, "http.route"), "unmatched");
    assert_eq!(get(missing, "http.status_code"), "404");
    // Each field is recorded once.
    assert_eq!(ok.iter().filter(|(n, _)| n == "http.route").count(), 1);
}

#[tokio::test]
async fn levels_by_status_and_unmatched_route() {
    let (buf, _g) = capture("info");
    let c = app().plugin(Trace::new()).test();
    c.get("/bad").send().await;
    c.get("/fail").send().await;
    c.get("/missing").send().await;
    c.delete("/users/1").send().await;

    let done = buf.completions();
    let by_path = |p: &str| {
        done.iter()
            .find(|e| e["span"]["url.path"] == p)
            .unwrap_or_else(|| panic!("no event for {p}: {done:?}"))
            .clone()
    };
    assert_eq!(by_path("/bad")["level"], "WARN");
    assert_eq!(by_path("/bad")["span"]["http.route"], "/bad");
    assert_eq!(by_path("/fail")["level"], "ERROR");
    assert_eq!(by_path("/fail")["http.status_code"], 500);
    let missing = by_path("/missing");
    assert_eq!(missing["level"], "WARN");
    assert_eq!(missing["span"]["http.route"], "unmatched");
    assert_eq!(missing["http.status_code"], 404);
    let not_allowed = by_path("/users/1");
    assert_eq!(not_allowed["span"]["http.route"], "unmatched");
    assert_eq!(not_allowed["http.status_code"], 405);
}

#[tokio::test]
async fn level_overrides() {
    let (buf, _g) = capture("debug");
    let c = app()
        .plugin(
            Trace::new()
                .level(Level::DEBUG)
                .level_for(StatusClass::ClientError, Level::INFO)
                .level_for(StatusClass::ServerError, Level::WARN),
        )
        .test();
    c.get("/users/1").send().await;
    c.get("/bad").send().await;
    c.get("/fail").send().await;
    let levels: Vec<String> = buf
        .completions()
        .iter()
        .map(|e| e["level"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(levels, ["DEBUG", "INFO", "WARN"]);
}

#[tokio::test]
async fn skip_paths_exact_and_prefix() {
    let (buf, _g) = capture("info");
    let c = app()
        .get("/static/app.js", || async { "js" })
        .plugin(Trace::new().skip_paths(["/health/live", "/static/*", "/users/{id}"]))
        .test();
    c.get("/health/live").send().await;
    c.get("/static/app.js").send().await;
    c.get("/static/missing.css").send().await;
    c.get("/bad").send().await;
    let done = buf.completions();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0]["span"]["url.path"], "/bad");

    // Route templates can be skipped too (in route position the template is
    // known up front).
    let (buf, _g2) = capture("info");
    let c = app()
        .wrap_route(Trace::new().skip_paths(["/users/{id}"]))
        .test();
    c.get("/users/1").send().await;
    c.get("/bad").send().await;
    assert_eq!(buf.completions().len(), 1);
}

#[tokio::test]
async fn request_id_from_request_or_response() {
    let (buf, _g) = capture("info");
    let c = app().plugin(Trace::new()).test();
    c.get("/users/1")
        .header("x-request-id", "req-1")
        .send()
        .await;
    c.get("/with-id").send().await;
    c.get("/users/2").send().await;
    let done = buf.completions();
    assert_eq!(done[0]["span"]["request_id"], "req-1");
    assert_eq!(done[1]["span"]["request_id"], "resp-42");
    assert!(done[2]["span"].get("request_id").is_none());

    // Custom header name; oversized ids are not logged.
    let (buf, _g2) = capture("info");
    let c = app()
        .plugin(Trace::new().request_id_header("x-correlation-id"))
        .test();
    c.get("/users/1")
        .header("x-correlation-id", "corr-9")
        .send()
        .await;
    c.get("/users/1")
        .header("x-correlation-id", "x".repeat(500))
        .send()
        .await;
    let done = buf.completions();
    assert_eq!(done[0]["span"]["request_id"], "corr-9");
    assert!(done[1]["span"].get("request_id").is_none());
}

#[tokio::test]
async fn client_ip_header() {
    let (buf, _g) = capture("info");
    let c = app()
        .plugin(Trace::new().client_ip_header("x-forwarded-for"))
        .test();
    c.get("/users/1")
        .header("x-forwarded-for", "203.0.113.9, 10.0.0.1")
        .send()
        .await;
    c.get("/users/1")
        .header("x-forwarded-for", "not-an-ip")
        .send()
        .await;
    let done = buf.completions();
    assert_eq!(done[0]["span"]["client.address"], "203.0.113.9");
    assert_eq!(done[1]["span"]["client.address"], "127.0.0.1");
}

#[tokio::test]
async fn wrap_route_sees_matched_routes_only() {
    let (buf, _g) = capture("info");
    let c = app().wrap_route(Trace::new()).test();
    c.get("/users/3").send().await;
    c.get("/missing").send().await;
    let done = buf.completions();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["span"]["http.route"], "/users/{id}");
}

#[tokio::test]
async fn plain_wrap_sees_404s_without_route() {
    let (buf, _g) = capture("info");
    let c = app().wrap(Trace::new()).test();
    c.get("/users/3").send().await;
    c.get("/missing").send().await;
    let done = buf.completions();
    assert_eq!(done.len(), 2);
    assert_eq!(done[0]["span"]["url.path"], "/users/3");
    assert!(done[0]["span"].get("http.route").is_none());
    assert_eq!(done[1]["http.status_code"], 404);
}

#[tokio::test]
async fn disabled_span_puts_request_fields_on_the_event() {
    // At WARN the INFO span is disabled: success is silent and failures
    // carry the request fields themselves.
    let (buf, _g) = capture("warn");
    let c = app().plugin(Trace::new()).test();
    c.get("/users/1").send().await;
    c.get("/bad").header("x-request-id", "r-7").send().await;
    c.get("/missing").send().await;
    let done = buf.completions();
    assert_eq!(done.len(), 2, "{done:?}");
    let bad = &done[0];
    assert_eq!(bad["level"], "WARN");
    assert!(bad.get("span").is_none());
    assert_eq!(bad["http.method"], "GET");
    assert_eq!(bad["url.path"], "/bad");
    assert_eq!(bad["client.address"], "127.0.0.1");
    assert_eq!(bad["request_id"], "r-7");
    assert_eq!(bad["http.status_code"], 400);
    assert!(bad["latency_ms"].as_f64().is_some());
    // Without a recording span, global tracing does not track the route...
    assert!(bad.get("http.route").is_none());
    assert_eq!(done[1]["url.path"], "/missing");
    assert_eq!(done[1]["http.status_code"], 404);

    // ...while route-level tracing knows it up front.
    let (buf, _g2) = capture("warn");
    let c = app().wrap_route(Trace::new()).test();
    c.get("/with-id").send().await;
    c.get("/bad").send().await;
    let done = buf.completions();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0]["http.route"], "/bad");

    // The response's request id is used when the request had none.
    let (buf, _g3) = capture("info");
    let c = App::new()
        .get("/", || async {
            (
                StatusCode::CONFLICT,
                [(
                    zapline::header::HeaderName::from_static("x-request-id"),
                    HeaderValue::from_static("gen-1"),
                )],
                "taken",
            )
        })
        .plugin(Trace::new().level(Level::DEBUG))
        .test();
    c.get("/").send().await;
    let done = buf.completions();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0]["request_id"], "gen-1");
}

#[tokio::test]
async fn nothing_is_logged_when_filtered_out() {
    let (buf, _g) = capture("error");
    let c = app().plugin(Trace::new()).test();
    c.get("/users/1").send().await;
    c.get("/bad").send().await;
    assert_eq!(buf.text(), "");
    c.get("/fail").send().await;
    let done = buf.completions();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["level"], "ERROR");

    // Target-level filtering silences request logs but not the app's.
    let (buf, _g2) = capture("info,zapline::request=off");
    let c = app().plugin(Trace::new()).test();
    c.get("/users/1").send().await;
    let lines = buf.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["message"], "loading user");
}

#[tokio::test]
async fn trace_composes_with_other_route_middleware() {
    let (buf, _g) = capture("info");
    let c = app()
        .plugin(Trace::new())
        .wrap_route(|req: Request, next: Next| async move {
            tracing::info!("in route middleware");
            next.run(req).await
        })
        .test();
    c.get("/users/5").send().await;
    let lines = buf.lines();
    let mw = lines
        .iter()
        .find(|l| l["message"] == "in route middleware")
        .unwrap();
    assert_eq!(mw["span"]["http.route"], "/users/{id}");
}

#[tokio::test]
async fn clones_are_configured_independently() {
    let (buf, _g) = capture("info");
    let base = Trace::new();
    let quiet = base.clone().level(Level::DEBUG);
    let c = app().wrap_route(quiet).test();
    c.get("/users/1").send().await;
    assert_eq!(buf.completions().len(), 0);
    let c = app().wrap_route(base).test();
    c.get("/users/1").send().await;
    assert_eq!(buf.completions().len(), 1);
}

// ---------------------------------------------------------------------------
// Telemetry builder

#[test]
fn invalid_filters_are_errors() {
    let err = Telemetry::new()
        .env_filter("info,foo=notalevel")
        .into_dispatch()
        .unwrap_err();
    assert!(matches!(err, TelemetryError::InvalidFilter(_)), "{err}");
    assert!(err.to_string().starts_with("invalid log filter"));
    let err = Telemetry::new()
        .level("loud")
        .env_var("ZAPLINE_TEST_UNSET_VAR")
        .into_dispatch();
    assert!(matches!(err, Err(TelemetryError::InvalidFilter(_))));
    assert!(Telemetry::new().level(Level::DEBUG).into_dispatch().is_ok());
}

#[test]
fn text_formats_render() {
    for format in [Format::Compact, Format::Pretty, Format::Full] {
        let buf = Buf::default();
        let b = buf.clone();
        let dispatch = Telemetry::new()
            .format(format)
            .env_filter("info")
            .with_target(false)
            .with_thread_names(true)
            .with_file_line(true)
            .ansi(false)
            .writer(move || b.clone())
            .into_dispatch()
            .unwrap();
        tracing::dispatcher::with_default(&dispatch, || {
            let span = tracing::info_span!("request", url.path = "/x");
            let _e = span.enter();
            tracing::info!(answer = 42, "hello");
            tracing::debug!("filtered out");
        });
        let out = buf.text();
        assert!(out.contains("hello"), "{format:?}: {out}");
        assert!(out.contains("42"), "{format:?}: {out}");
        assert!(out.contains("/x"), "{format:?}: {out}");
        assert!(!out.contains("filtered out"), "{format:?}: {out}");
        assert!(!out.contains('\u{1b}'), "{format:?}: ANSI escapes in {out}");
    }
}

#[test]
fn env_var_is_honored_unless_explicit_filter() {
    // A private variable name keeps this test independent of RUST_LOG.
    std::env::set_var("ZAPLINE_TEST_LOG", "warn");
    let run = |t: Telemetry| {
        let buf = Buf::default();
        let b = buf.clone();
        let d = t.json().writer(move || b.clone()).into_dispatch().unwrap();
        tracing::dispatcher::with_default(&d, || {
            tracing::info!("info line");
            tracing::warn!("warn line");
        });
        buf.text()
    };
    let out = run(Telemetry::new().env_var("ZAPLINE_TEST_LOG").level("debug"));
    assert!(
        !out.contains("info line") && out.contains("warn line"),
        "{out}"
    );
    let out = run(Telemetry::new()
        .env_var("ZAPLINE_TEST_LOG")
        .env_filter("info"));
    assert!(out.contains("info line"), "{out}");
    let out = run(Telemetry::new()
        .env_var("ZAPLINE_TEST_UNSET_VAR")
        .level("error"));
    assert!(!out.contains("warn line"), "{out}");
}
