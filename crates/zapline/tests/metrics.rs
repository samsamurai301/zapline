//! Prometheus metrics: exposition format, HTTP request metrics, custom
//! metrics, process metrics and the plugin options.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::FutureExt;
use zapline::metrics::{
    self, Collector, Encoder, MetricKind, Metrics, MetricsHandle, ProcessCollector, Registry,
};
use zapline::prelude::*;
use zapline::test::TestClient;

// ---------------------------------------------------------------------------
// Helpers

/// Parse `name{a="b",c="d"} value` into (name, labels, value), handling
/// escaped label values.
fn parse_sample(line: &str) -> (String, Vec<(String, String)>, String) {
    let (head, value) = line.rsplit_once(' ').expect("sample has a value");
    let Some(brace) = head.find('{') else {
        return (head.to_owned(), vec![], value.to_owned());
    };
    let name = head[..brace].to_owned();
    let body = head[brace + 1..].strip_suffix('}').expect("closing brace");
    let mut labels = Vec::new();
    let mut chars = body.chars().peekable();
    while chars.peek().is_some() {
        let key: String = chars.by_ref().take_while(|&c| c != '=').collect();
        assert_eq!(
            chars.next(),
            Some('"'),
            "label value must be quoted: {line}"
        );
        let mut val = String::new();
        loop {
            match chars.next().expect("unterminated label value") {
                '\\' => match chars.next().unwrap() {
                    'n' => val.push('\n'),
                    c @ ('\\' | '"') => val.push(c),
                    c => panic!("bad escape \\{c} in {line}"),
                },
                '"' => break,
                c => val.push(c),
            }
        }
        labels.push((key, val));
        if chars.peek() == Some(&',') {
            chars.next();
        }
    }
    (name, labels, value.to_owned())
}

/// Check a scrape is valid text format 0.0.4: every sample belongs to a
/// family declared with HELP + TYPE, no family is declared twice, and
/// histograms are cumulative and end in `+Inf` == `_count`.
fn validate(text: &str) {
    let mut types: HashMap<String, String> = HashMap::new();
    let mut current: Option<String> = None;
    let mut last_help: Option<String> = None;
    let mut buckets: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    let mut counts: HashMap<String, f64> = HashMap::new();
    for line in text.lines() {
        assert!(!line.is_empty(), "blank line in exposition");
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let name = rest.split(' ').next().unwrap().to_owned();
            last_help = Some(name);
            continue;
        }
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, ty) = rest.split_once(' ').unwrap();
            assert_eq!(
                last_help.as_deref(),
                Some(name),
                "TYPE without HELP: {line}"
            );
            assert!(
                ["counter", "gauge", "histogram", "untyped", "summary"].contains(&ty),
                "{line}"
            );
            assert!(
                types.insert(name.to_owned(), ty.to_owned()).is_none(),
                "family {name} declared twice"
            );
            current = Some(name.to_owned());
            continue;
        }
        assert!(!line.starts_with('#'), "unexpected comment {line}");
        let (name, labels, value) = parse_sample(line);
        let family = current.as_deref().expect("sample before TYPE");
        let ty = &types[family];
        if ty == "histogram" {
            let suffix = name
                .strip_prefix(family)
                .unwrap_or_else(|| panic!("{line}"));
            let key: Vec<String> = labels
                .iter()
                .filter(|(k, _)| k != "le")
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            let key = format!("{name}|{}", key.join(","))
                .replace("_bucket|", "|")
                .replace("_count|", "|")
                .replace("_sum|", "|");
            let v: f64 = value.parse().unwrap();
            match suffix {
                "_bucket" => {
                    let le = &labels.iter().find(|(k, _)| k == "le").expect("le").1;
                    let le: f64 = if le == "+Inf" {
                        f64::INFINITY
                    } else {
                        le.parse().unwrap()
                    };
                    buckets.entry(key).or_default().push((le, v));
                }
                "_count" => {
                    counts.insert(key, v);
                }
                "_sum" => {}
                s => panic!("unexpected histogram suffix {s}"),
            }
        } else {
            assert_eq!(name, family, "sample {name} outside its family");
            let _: f64 = match value.as_str() {
                "+Inf" | "-Inf" | "NaN" => 0.0,
                v => v.parse().unwrap_or_else(|_| panic!("bad value in {line}")),
            };
        }
    }
    for (key, b) in &buckets {
        assert!(b.windows(2).all(|w| w[0].0 < w[1].0), "le order {key}");
        assert!(
            b.windows(2).all(|w| w[0].1 <= w[1].1),
            "not cumulative {key}"
        );
        let last = b.last().unwrap();
        assert_eq!(last.0, f64::INFINITY, "missing +Inf for {key}");
        assert_eq!(Some(&last.1), counts.get(key), "+Inf != _count for {key}");
    }
}

/// The value of the sample with exactly these labels (any order).
fn value(text: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(parse_sample)
        .find(|(n, ls, _)| {
            n == name
                && ls.len() == labels.len()
                && labels
                    .iter()
                    .all(|(k, v)| ls.iter().any(|(lk, lv)| lk == k && lv == v))
        })
        .map(|(_, _, v)| v.parse().unwrap())
}

async fn scrape(c: &TestClient) -> String {
    let r = c.get("/metrics").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.header("content-type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let text = r.text();
    validate(&text);
    text
}

fn requests(text: &str, method: &str, route: &str, status: &str) -> f64 {
    value(
        text,
        "http_requests_total",
        &[("method", method), ("route", route), ("status", status)],
    )
    .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Exposition format & registry

#[test]
fn exposition_format_is_exact() {
    let reg = Registry::new();
    let c = reg.counter(
        "jobs_total",
        "Jobs run.\nPer \\ queue.",
        &["queue", "outcome"],
    );
    c.with_labels(&["mail", "ok"]).add(3);
    c.with_labels(&["a\"b\\c\nd", "ok"]).inc();
    let g = reg.gauge("temperature_celsius", "Current temperature.", &[]);
    g.set(-1.5);
    let h = reg.histogram_with_buckets("job_seconds", "Job duration.", &["queue"], &[0.1, 1.0]);
    let mail = h.with_labels(&["mail"]);
    mail.observe(0.05);
    mail.observe(0.5);
    mail.observe(7.0);

    let expected = "\
# HELP jobs_total Jobs run.\\nPer \\\\ queue.
# TYPE jobs_total counter
jobs_total{queue=\"mail\",outcome=\"ok\"} 3
jobs_total{queue=\"a\\\"b\\\\c\\nd\",outcome=\"ok\"} 1
# HELP temperature_celsius Current temperature.
# TYPE temperature_celsius gauge
temperature_celsius -1.5
# HELP job_seconds Job duration.
# TYPE job_seconds histogram
job_seconds_bucket{queue=\"mail\",le=\"0.1\"} 1
job_seconds_bucket{queue=\"mail\",le=\"1\"} 2
job_seconds_bucket{queue=\"mail\",le=\"+Inf\"} 3
job_seconds_sum{queue=\"mail\"} 7.55
job_seconds_count{queue=\"mail\"} 3
";
    let text = reg.render();
    assert_eq!(text, expected);
    validate(&text);
    // The parser round-trips the escaped label value.
    assert_eq!(
        value(
            &text,
            "jobs_total",
            &[("queue", "a\"b\\c\nd"), ("outcome", "ok")]
        ),
        Some(1.0)
    );
}

#[test]
fn special_float_values() {
    let reg = Registry::new();
    reg.gauge("g_inf", "inf", &[]).set(f64::INFINITY);
    reg.gauge("g_ninf", "-inf", &[]).set(f64::NEG_INFINITY);
    reg.gauge("g_nan", "nan", &[]).set(f64::NAN);
    let text = reg.render();
    assert!(text.contains("\ng_inf +Inf\n"), "{text}");
    assert!(text.contains("\ng_ninf -Inf\n"), "{text}");
    assert!(text.contains("\ng_nan NaN\n"), "{text}");
    validate(&text);
}

#[test]
fn handles_and_values() {
    let reg = Registry::new();
    let c = reg.counter("hits_total", "Hits.", &[]);
    c.inc();
    c.add(4);
    assert_eq!(c.get(), 5);

    let g = reg.gauge("level", "Level.", &["tank"]);
    let a = g.with_labels(&["a"]);
    a.set(10.0);
    a.sub(2.5);
    a.inc();
    a.dec();
    a.add(0.5);
    assert_eq!(a.get(), 8.0);
    // Clones and fresh lookups share the series.
    assert_eq!(g.with_labels(&["a"]).get(), 8.0);

    let h = reg.histogram("lat_seconds", "Latency.", &[]);
    h.observe(0.2);
    h.observe(f64::NAN); // ignored
    let t = h.start_timer();
    assert!(t.stop() >= 0.0);
    h.start_timer().discard();
    {
        let _t = h.start_timer();
    }
    assert_eq!(h.count(), 3);
    assert!(h.sum() >= 0.2);
    let text = reg.render();
    assert_eq!(value(&text, "lat_seconds_count", &[]), Some(3.0));
    assert_eq!(
        value(&text, "lat_seconds_bucket", &[("le", "+Inf")]),
        Some(3.0)
    );
    validate(&text);
}

#[test]
fn label_values_are_padded_or_truncated() {
    let reg = Registry::new();
    let c = reg.counter("req_total", "Requests.", &["a", "b"]);
    c.with_labels(&["x"]).inc();
    c.with_labels(&["x", "y", "ignored"]).inc();
    c.inc(); // unbound handle on a labeled family: all-empty values
    let text = reg.render();
    assert_eq!(
        value(&text, "req_total", &[("a", "x"), ("b", "")]),
        Some(1.0)
    );
    assert_eq!(
        value(&text, "req_total", &[("a", "x"), ("b", "y")]),
        Some(1.0)
    );
    assert_eq!(
        value(&text, "req_total", &[("a", ""), ("b", "")]),
        Some(1.0)
    );
    // A labeled family with no series yet renders just its header.
    let reg = Registry::new();
    reg.counter("empty_total", "Nothing yet.", &["x"]);
    assert_eq!(
        reg.render(),
        "# HELP empty_total Nothing yet.\n# TYPE empty_total counter\n"
    );
}

#[test]
fn re_registration_returns_the_same_metric() {
    let reg = Registry::new();
    reg.counter("dup_total", "Dup.", &["k"])
        .with_labels(&["v"])
        .inc();
    reg.counter("dup_total", "Dup.", &["k"])
        .with_labels(&["v"])
        .inc();
    let text = reg.render();
    assert_eq!(text.matches("# TYPE dup_total").count(), 1);
    assert_eq!(value(&text, "dup_total", &[("k", "v")]), Some(2.0));
}

#[test]
#[should_panic(expected = "already registered as a counter")]
fn conflicting_registration_panics() {
    let reg = Registry::new();
    reg.counter("conflict", "A.", &[]);
    reg.gauge("conflict", "B.", &[]);
}

#[test]
#[should_panic(expected = "already registered")]
fn conflicting_labels_panic() {
    let reg = Registry::new();
    reg.counter("conflict_total", "A.", &["a"]);
    reg.counter("conflict_total", "A.", &["b"]);
}

#[test]
fn name_validation() {
    for ok in ["a", "_x", "a:b_c9", "http_requests_total", ":colon"] {
        assert!(metrics::is_valid_metric_name(ok), "{ok}");
    }
    for bad in ["", "9a", "a-b", "a b", "é", "a.b"] {
        assert!(!metrics::is_valid_metric_name(bad), "{bad}");
    }
    for ok in ["a", "_x", "route", "a9"] {
        assert!(metrics::is_valid_label_name(ok), "{ok}");
    }
    for bad in ["", "__reserved", "a:b", "9a", "a-b"] {
        assert!(!metrics::is_valid_label_name(bad), "{bad}");
    }
    let bad_metric = std::panic::catch_unwind(|| Registry::new().counter("bad-name", "x", &[]));
    assert!(bad_metric.is_err());
    let bad_label = std::panic::catch_unwind(|| Registry::new().counter("ok_total", "x", &["__x"]));
    assert!(bad_label.is_err());
    let dup_label =
        std::panic::catch_unwind(|| Registry::new().counter("ok_total", "x", &["a", "a"]));
    assert!(dup_label.is_err());
    let le_label = std::panic::catch_unwind(|| Registry::new().histogram("h", "x", &["le"]));
    assert!(le_label.is_err());
}

#[test]
fn bucket_validation() {
    let reg = Registry::new();
    // A trailing +Inf is implied and accepted.
    let h = reg.histogram_with_buckets("ok_seconds", "x", &[], &[0.5, 1.0, f64::INFINITY]);
    h.observe(2.0);
    let text = reg.render();
    assert_eq!(text.matches("le=\"+Inf\"").count(), 1, "{text}");
    for bad in [vec![1.0, 1.0], vec![2.0, 1.0], vec![f64::NAN]] {
        let r = std::panic::catch_unwind(|| {
            Registry::new().histogram_with_buckets("bad_seconds", "x", &[], &bad)
        });
        assert!(r.is_err(), "{bad:?}");
    }
}

#[test]
fn series_per_family_are_bounded() {
    let reg = Registry::new();
    let c = reg.counter("users_total", "Per user (bad idea).", &["user"]);
    for i in 0..metrics::MAX_SERIES_PER_FAMILY + 50 {
        c.with_labels(&[&i.to_string()]).inc();
    }
    let text = reg.render();
    let series = text
        .lines()
        .filter(|l| l.starts_with("users_total{"))
        .count();
    assert_eq!(series, metrics::MAX_SERIES_PER_FAMILY + 1);
    assert_eq!(
        value(&text, "users_total", &[("user", "_overflow")]),
        Some(50.0)
    );
}

#[test]
fn collectors_and_global_registry() {
    struct Queue;
    impl Collector for Queue {
        fn collect(&self, out: &mut Encoder<'_>) {
            out.header("queue_depth", "Queued jobs.", MetricKind::Gauge);
            out.sample("queue_depth", &[("queue", "mail")], 4.0);
            out.sample_u64("queue_depth", &[("queue", "sms")], 2);
        }
    }
    let reg = Registry::new();
    reg.register(Queue);
    reg.register(|out: &mut Encoder<'_>| {
        out.header("build_info", "Build.", MetricKind::Gauge);
        out.sample("build_info", &[("version", "1.2.3")], 1.0);
    });
    let text = reg.render();
    validate(&text);
    assert_eq!(value(&text, "queue_depth", &[("queue", "mail")]), Some(4.0));
    assert_eq!(value(&text, "queue_depth", &[("queue", "sms")]), Some(2.0));
    assert_eq!(
        value(&text, "build_info", &[("version", "1.2.3")]),
        Some(1.0)
    );

    metrics::registry()
        .counter("global_test_total", "Global.", &[])
        .inc();
    assert!(metrics::registry().render().contains("global_test_total "));
}

// ---------------------------------------------------------------------------
// HTTP metrics

#[derive(serde::Deserialize)]
struct Id {
    id: u64,
}

fn app() -> App {
    App::new()
        .get("/users/{id}", |Path(p): Path<Id>| async move {
            if p.id == 0 {
                Err(Error::not_found())
            } else {
                Ok(format!("user {}", p.id))
            }
        })
        .post("/users", || async { StatusCode::CREATED })
        .get("/boom", || async { Error::internal("boom") })
}

#[tokio::test]
async fn counts_matched_routes_by_template() {
    let c = app().plugin(Metrics::new()).test();
    for id in 1..=3 {
        assert_eq!(c.get(&format!("/users/{id}")).send().await.status(), 200);
    }
    assert_eq!(c.get("/users/0").send().await.status(), 404);
    assert_eq!(c.post("/users").send().await.status(), 201);
    assert_eq!(c.get("/boom").send().await.status(), 500);

    let text = scrape(&c).await;
    assert_eq!(requests(&text, "GET", "/users/{id}", "200"), 3.0);
    assert_eq!(requests(&text, "GET", "/users/{id}", "404"), 1.0);
    assert_eq!(requests(&text, "POST", "/users", "201"), 1.0);
    assert_eq!(requests(&text, "GET", "/boom", "500"), 1.0);
    // A handler's own 404 is not "unmatched".
    assert_eq!(requests(&text, "GET", "unmatched", "404"), 0.0);
    // Raw paths never become labels.
    assert!(!text.contains("/users/1"), "{text}");

    let h = |suffix: &str, extra: &[(&str, &str)]| {
        let mut labels = vec![("method", "GET"), ("route", "/users/{id}")];
        labels.extend_from_slice(extra);
        value(
            &text,
            &format!("http_request_duration_seconds{suffix}"),
            &labels,
        )
    };
    assert_eq!(h("_count", &[]), Some(4.0));
    assert_eq!(h("_bucket", &[("le", "+Inf")]), Some(4.0));
    assert_eq!(h("_bucket", &[("le", "10")]), Some(4.0));
    assert!(h("_sum", &[]).unwrap() > 0.0);
    // Default buckets.
    for le in [
        "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10",
    ] {
        assert!(h("_bucket", &[("le", le)]).is_some(), "missing le={le}");
    }
    // The in-flight gauge counts the scrape itself.
    assert_eq!(value(&text, "http_requests_in_flight", &[]), Some(1.0));

    // The scrape endpoint is a regular route and was counted.
    let text = scrape(&c).await;
    assert_eq!(requests(&text, "GET", "/metrics", "200"), 1.0);
}

#[tokio::test]
async fn counts_unmatched_requests() {
    let c = app().plugin(Metrics::new()).test();
    assert_eq!(c.get("/nope").send().await.status(), 404);
    assert_eq!(c.get("/nope/again").send().await.status(), 404);
    assert_eq!(c.delete("/users").send().await.status(), 405);
    let r = c
        .request(Method::from_bytes(b"BREW").unwrap(), "/teapot")
        .send()
        .await;
    assert_eq!(r.status(), 404);

    let text = scrape(&c).await;
    assert_eq!(requests(&text, "GET", "unmatched", "404"), 2.0);
    assert_eq!(requests(&text, "DELETE", "unmatched", "405"), 1.0);
    assert_eq!(requests(&text, "_OTHER", "unmatched", "404"), 1.0);
    assert!(!text.contains("/nope"), "{text}");
    assert!(!text.contains("BREW"), "{text}");
}

#[tokio::test]
async fn unmatched_tracking_can_be_disabled() {
    let c = app().plugin(Metrics::new().track_unmatched(false)).test();
    assert_eq!(c.get("/nope").send().await.status(), 404);
    c.get("/users/1").send().await;
    let text = scrape(&c).await;
    assert_eq!(requests(&text, "GET", "unmatched", "404"), 0.0);
    assert_eq!(requests(&text, "GET", "/users/{id}", "200"), 1.0);
    // In-flight is then tracked for matched routes (the scrape itself).
    assert_eq!(value(&text, "http_requests_in_flight", &[]), Some(1.0));
}

#[tokio::test]
async fn fallback_and_error_handler_installed_first_are_seen() {
    let c = App::new()
        .get("/", || async { "home" })
        .error_handler(|r: &ErrorReport| (r.status, "custom").into_response())
        .plugin(Metrics::new())
        .test();
    let r = c.get("/missing").send().await;
    assert_eq!(r.text(), "custom");
    let text = scrape(&c).await;
    assert_eq!(requests(&text, "GET", "unmatched", "404"), 1.0);

    let c = App::new()
        .fallback(|| async { (StatusCode::NOT_FOUND, "nothing here") })
        .plugin(Metrics::new())
        .test();
    c.get("/x").send().await;
    let text = scrape(&c).await;
    assert_eq!(requests(&text, "GET", "unmatched", "404"), 1.0);
}

use zapline::ErrorReport;

#[tokio::test]
async fn methods_are_bounded() {
    let purge = Method::from_bytes(b"PURGE").unwrap();
    let c = App::new()
        .get("/page", || async { "page" })
        .route(purge.clone(), "/cache", || async { "purged" })
        .any("/any", || async { "any" })
        .plugin(Metrics::new())
        .test();
    // HEAD is served by the GET handler but labeled HEAD.
    assert_eq!(c.request(Method::HEAD, "/page").send().await.status(), 200);
    assert_eq!(c.request(purge, "/cache").send().await.status(), 200);
    c.request(Method::from_bytes(b"WHATEVER").unwrap(), "/any")
        .send()
        .await;
    c.put("/any").send().await;

    let text = scrape(&c).await;
    assert_eq!(requests(&text, "HEAD", "/page", "200"), 1.0);
    assert_eq!(requests(&text, "PURGE", "/cache", "200"), 1.0);
    assert_eq!(requests(&text, "_OTHER", "/any", "200"), 1.0);
    assert_eq!(requests(&text, "PUT", "/any", "200"), 1.0);
    assert!(!text.contains("WHATEVER"));
}

#[tokio::test]
async fn options_prefix_path_buckets_exclusions() {
    let c = App::new()
        .get("/work", || async { "done" })
        .get("/internal/debug", || async { "dbg" })
        .get("/health/live", || async { "up" })
        .plugin(
            Metrics::new()
                .path("/_metrics")
                .prefix("api")
                .buckets([0.5, 1.0])
                .exclude_paths(["/health/live", "/internal/*", "/favicon.ico"]),
        )
        .test();
    c.get("/work").send().await;
    c.get("/internal/debug").send().await;
    c.get("/health/live").send().await;
    c.get("/favicon.ico").send().await;
    c.get("/other-missing").send().await;
    assert_eq!(c.get("/metrics").send().await.status(), 404);

    let r = c.get("/_metrics").send().await;
    let text = r.text();
    validate(&text);
    assert!(!text.contains("http_requests_total"));
    let total = |route: &str, status: &str| {
        value(
            &text,
            "api_requests_total",
            &[("method", "GET"), ("route", route), ("status", status)],
        )
    };
    assert_eq!(total("/work", "200"), Some(1.0));
    assert_eq!(total("/internal/debug", "200"), None);
    assert_eq!(total("/health/live", "200"), None);
    assert_eq!(total("unmatched", "404"), Some(2.0)); // /metrics + /other-missing
    let le: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.starts_with("api_request_duration_seconds_bucket{method=\"GET\",route=\"/work\"")
        })
        .collect();
    assert_eq!(le.len(), 3, "{le:?}");
    assert!(
        le[0].contains("le=\"0.5\"") && le[1].contains("le=\"1\"") && le[2].contains("le=\"+Inf\"")
    );
    assert!(text.contains("# TYPE api_requests_in_flight gauge"));
}

#[tokio::test]
async fn empty_prefix() {
    let c = App::new().plugin(Metrics::new().prefix("")).test();
    let text = scrape(&c).await;
    assert!(text.contains("# TYPE requests_total counter"), "{text}");
    assert!(text.contains("# TYPE request_duration_seconds histogram"));
}

#[test]
#[should_panic(expected = "invalid metrics prefix")]
fn invalid_prefix_panics_at_install() {
    let _ = App::new().plugin(Metrics::new().prefix("bad-prefix"));
}

#[test]
#[should_panic(expected = "strictly increasing")]
fn invalid_buckets_panic_at_install() {
    let _ = App::new().plugin(Metrics::new().buckets([1.0, 0.5]));
}

#[tokio::test]
async fn custom_registry_handle_and_disabled_endpoint() {
    let reg = Registry::new();
    let orders = reg.counter("orders_total", "Orders placed.", &["kind"]);
    let o = orders.clone();
    metrics::registry()
        .counter("only_in_global_total", "Global only.", &[])
        .inc();

    async fn admin(State(m): State<MetricsHandle>) -> Response {
        m.into_response()
    }

    let c = App::new()
        .post("/orders", move || {
            let o = o.clone();
            async move {
                o.with_labels(&["web"]).inc();
                StatusCode::CREATED
            }
        })
        .plugin(Metrics::new().endpoint(false).registry(reg))
        .get("/admin/metrics", admin)
        .test();
    c.post("/orders").send().await;
    c.post("/orders").send().await;
    assert_eq!(c.get("/metrics").send().await.status(), 404);

    let r = c.get("/admin/metrics").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-type"), Some(metrics::CONTENT_TYPE_TEXT));
    let text = r.text();
    validate(&text);
    assert_eq!(value(&text, "orders_total", &[("kind", "web")]), Some(2.0));
    assert_eq!(requests(&text, "POST", "/orders", "201"), 2.0);
    assert!(!text.contains("only_in_global_total"));

    // The handle is also reachable from resources.
    let handle = c.resources().await.require::<MetricsHandle>().unwrap();
    assert!(handle.render().contains("orders_total"));
    assert_eq!(
        handle
            .registry()
            .counter("orders_total", "Orders placed.", &["kind"])
            .with_labels(&["web"])
            .get(),
        2
    );
}

#[tokio::test]
async fn global_registry_is_the_default() {
    metrics::registry()
        .gauge("plugin_default_gauge", "From the global registry.", &[])
        .set(7.0);
    let c = App::new().plugin(Metrics::new()).test();
    let text = scrape(&c).await;
    assert_eq!(value(&text, "plugin_default_gauge", &[]), Some(7.0));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn process_metrics_on_linux() {
    let c = App::new().plugin(Metrics::new()).test();
    let text = scrape(&c).await;
    let get = |n: &str| value(&text, n, &[]).unwrap_or_else(|| panic!("missing {n}\n{text}"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let start = get("process_start_time_seconds");
    assert!(
        start > now - 86_400.0 * 365.0 && start <= now + 5.0,
        "{start}"
    );
    assert!(get("process_resident_memory_bytes") > 1_000_000.0);
    assert!(get("process_virtual_memory_bytes") > get("process_resident_memory_bytes"));
    assert!(get("process_open_fds") >= 3.0);
    assert!(get("process_max_fds") >= get("process_open_fds"));
    assert!(get("process_cpu_seconds_total") >= 0.0);
    assert!(text.contains("# TYPE process_cpu_seconds_total counter"));

    let c = App::new()
        .plugin(Metrics::new().process_metrics(false))
        .test();
    let text = scrape(&c).await;
    assert!(!text.contains("process_"), "{text}");
}

#[test]
fn process_collector_standalone() {
    let reg = Registry::new();
    reg.register(ProcessCollector);
    let text = reg.render();
    validate(&text);
    assert!(text.contains("# TYPE process_start_time_seconds gauge"));
}

#[tokio::test]
async fn in_flight_and_cancellation() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let (g, e) = (gate.clone(), entered.clone());
    let c = App::new()
        .get("/slow", move || {
            let (g, e) = (g.clone(), e.clone());
            async move {
                e.notify_one();
                g.notified().await;
                "done"
            }
        })
        .plugin(Metrics::new())
        .test();

    let c2 = c.clone();
    let slow = tokio::spawn(async move { c2.get("/slow").send().await.status() });
    entered.notified().await;
    let text = scrape(&c).await;
    assert_eq!(value(&text, "http_requests_in_flight", &[]), Some(2.0));
    gate.notify_one();
    assert_eq!(slow.await.unwrap(), 200);
    let text = scrape(&c).await;
    assert_eq!(value(&text, "http_requests_in_flight", &[]), Some(1.0));
    assert_eq!(requests(&text, "GET", "/slow", "200"), 1.0);

    // A cancelled request releases its in-flight slot and is not counted.
    let c2 = c.clone();
    let slow = tokio::spawn(async move { c2.get("/slow").send().await });
    entered.notified().await;
    slow.abort();
    let _ = slow.await;
    let text = scrape(&c).await;
    assert_eq!(value(&text, "http_requests_in_flight", &[]), Some(1.0));
    assert_eq!(requests(&text, "GET", "/slow", "200"), 1.0);
}

#[tokio::test]
async fn panicking_handler_counts_as_500() {
    let c = App::new()
        .get("/panic", || async {
            if true {
                panic!("handler bug");
            }
            "unreachable"
        })
        .plugin(Metrics::new())
        .test();
    let r = std::panic::AssertUnwindSafe(c.get("/panic").send())
        .catch_unwind()
        .await;
    assert!(r.is_err());
    let text = scrape(&c).await;
    assert_eq!(requests(&text, "GET", "/panic", "500"), 1.0);
    assert_eq!(value(&text, "http_requests_in_flight", &[]), Some(1.0));
}

#[tokio::test]
async fn many_status_codes_per_route() {
    let c = App::new()
        .get("/status/{code}", |Path(code): Path<u16>| async move {
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST)
        })
        .plugin(Metrics::new())
        .test();
    let codes = [
        200u16, 201, 202, 204, 301, 302, 400, 401, 403, 409, 418, 429, 500, 503,
    ];
    for (i, code) in codes.iter().enumerate() {
        for _ in 0..=i {
            c.get(&format!("/status/{code}")).send().await;
        }
    }
    let text = scrape(&c).await;
    for (i, code) in codes.iter().enumerate() {
        assert_eq!(
            requests(&text, "GET", "/status/{code}", &code.to_string()),
            (i + 1) as f64,
            "status {code}"
        );
    }
    // Sorted by status code.
    let order: Vec<u16> = text
        .lines()
        .filter(|l| l.starts_with("http_requests_total{method=\"GET\",route=\"/status/{code}\""))
        .map(|l| parse_sample(l).1[2].1.parse().unwrap())
        .collect();
    assert_eq!(order, codes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_are_counted_exactly() {
    let c = App::new()
        .get("/a/{id}", || async { "a" })
        .post("/b", || async { "b" })
        .plugin(Metrics::new())
        .test();
    c.start().await;
    let tasks: Vec<_> = (0..16)
        .map(|t| {
            let c = c.clone();
            tokio::spawn(async move {
                for i in 0..250 {
                    if (t + i) % 2 == 0 {
                        c.get(&format!("/a/{i}")).send().await;
                    } else {
                        c.post("/b").send().await;
                    }
                }
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let text = scrape(&c).await;
    assert_eq!(requests(&text, "GET", "/a/{id}", "200"), 2000.0);
    assert_eq!(requests(&text, "POST", "/b", "200"), 2000.0);
    let count = value(
        &text,
        "http_request_duration_seconds_count",
        &[("method", "GET"), ("route", "/a/{id}")],
    );
    assert_eq!(count, Some(2000.0));
    assert_eq!(value(&text, "http_requests_in_flight", &[]), Some(1.0));
}

#[tokio::test]
async fn routes_in_groups_and_nested() {
    let api = Routes::new()
        .get("/items/{id}", || async { "item" })
        .wrap(|req: Request, next: Next| async move { next.run(req).await });
    let c = App::new()
        .nest("/api/v1", api)
        .plugin(Metrics::new())
        .test();
    c.get("/api/v1/items/9").send().await;
    let text = scrape(&c).await;
    assert_eq!(requests(&text, "GET", "/api/v1/items/{id}", "200"), 1.0);
}

#[tokio::test]
async fn real_server_scrape() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        App::new()
            .get("/hello/{name}", || async { "hi" })
            .plugin(Metrics::new())
            .serve(listener),
    );
    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }
    get(addr, "/hello/ada").await;
    get(addr, "/hello/bob").await;
    get(addr, "/missing").await;
    let res = get(addr, "/metrics").await;
    let (head, body) = res.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head
        .to_ascii_lowercase()
        .contains("content-type: text/plain; version=0.0.4"));
    validate(body);
    assert_eq!(requests(body, "GET", "/hello/{name}", "200"), 2.0);
    assert_eq!(requests(body, "GET", "unmatched", "404"), 1.0);
}
