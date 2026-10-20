//! Built-in middleware: request IDs, security headers, panics, load
//! shedding, path normalization, CORS, timeouts, default headers, and how
//! they compose.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use zapline::header;
use zapline::middleware::{
    CatchPanic, ConcurrencyLimit, Cors, DefaultHeaders, Logger, NormalizePath, RequestId,
    SecurityHeaders, SetRequestId, Timeout,
};
use zapline::prelude::*;

// ---------------------------------------------------------------------------
// Request IDs

fn is_uuid_v7(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && [8, 13, 18, 23].iter().all(|&i| b[i] == b'-')
        && b[14] == b'7'
        && matches!(b[19], b'8' | b'9' | b'a' | b'b')
        && s.chars()
            .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn id_app(mw: SetRequestId) -> zapline::test::TestClient {
    App::new()
        .wrap(mw)
        .get("/", |id: RequestId| async move { id.to_string() })
        .get("/hdr", |headers: HeaderMap| async move {
            headers
                .get("x-request-id")
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_default()
        })
        .test()
}

#[tokio::test]
async fn generates_request_ids() {
    let c = id_app(SetRequestId::new());
    let r = c.get("/").send().await;
    let id = r.header("x-request-id").unwrap().to_owned();
    assert!(is_uuid_v7(&id), "{id}");
    assert_eq!(r.text(), id, "extractor sees the same id");

    // Downstream code also sees it as a request header.
    let r = c.get("/hdr").send().await;
    assert_eq!(r.text(), r.header("x-request-id").unwrap());

    // 404s get one too (global middleware).
    let r = c.get("/missing").send().await;
    assert_eq!(r.status(), 404);
    assert!(r.header("x-request-id").is_some());
}

#[tokio::test]
async fn generated_ids_are_unique_and_time_ordered() {
    let ids: Vec<String> = (0..5000)
        .map(|_| RequestId::generate().to_string())
        .collect();
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
    // The first 48 bits are a millisecond timestamp.
    let first = &ids[0][..13];
    let last = &ids[ids.len() - 1][..13];
    assert!(first <= last);
    assert!(ids.iter().all(|id| is_uuid_v7(id)));
}

#[tokio::test]
async fn reuses_valid_incoming_ids() {
    let c = id_app(SetRequestId::new());
    let r = c
        .get("/")
        .header("x-request-id", "abc-123_x.y:z")
        .send()
        .await;
    assert_eq!(r.header("x-request-id"), Some("abc-123_x.y:z"));
    assert_eq!(r.text(), "abc-123_x.y:z");
}

#[tokio::test]
async fn replaces_invalid_incoming_ids() {
    let c = id_app(SetRequestId::new());
    let too_long = "a".repeat(129);
    for bad in [
        too_long.as_str(),
        "has space",
        "semi;colon",
        "quote\"",
        "ümlaut",
    ] {
        let r = c.get("/hdr").header("x-request-id", bad).send().await;
        let id = r.header("x-request-id").unwrap();
        assert!(is_uuid_v7(id), "{bad:?} -> {id}");
        assert_eq!(r.text(), id, "request header was replaced too");
    }
    let ok = "a".repeat(128);
    let r = c.get("/").header("x-request-id", ok.as_str()).send().await;
    assert_eq!(r.header("x-request-id"), Some(ok.as_str()));
}

#[tokio::test]
async fn untrusted_incoming_ids_are_ignored() {
    let c = id_app(SetRequestId::new().trust_incoming(false));
    let r = c
        .get("/")
        .header("x-request-id", "client-chosen")
        .send()
        .await;
    assert_ne!(r.header("x-request-id"), Some("client-chosen"));
    assert_eq!(r.text(), r.header("x-request-id").unwrap());
}

#[tokio::test]
async fn custom_header_and_generator() {
    let c = App::new()
        .wrap(
            SetRequestId::new()
                .header("X-Correlation-ID")
                .generator(|| "fixed-id".to_owned()),
        )
        .get("/", |id: RequestId| async move { id.to_string() })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.header("x-correlation-id"), Some("fixed-id"));
    assert_eq!(r.header("x-request-id"), None);
    assert_eq!(r.text(), "fixed-id");

    // A generator producing invalid IDs falls back to the default.
    let c = id_app(SetRequestId::new().generator(|| "not valid!".to_owned()));
    let r = c.get("/").send().await;
    assert!(is_uuid_v7(r.header("x-request-id").unwrap()));
}

#[tokio::test]
async fn handler_set_id_header_wins() {
    let c = App::new()
        .wrap(SetRequestId::new())
        .get("/", || async {
            (
                [(
                    header::HeaderName::from_static("x-request-id"),
                    HeaderValue::from_static("mine"),
                )],
                "x",
            )
        })
        .test();
    assert_eq!(c.get("/").send().await.header("x-request-id"), Some("mine"));
}

#[tokio::test]
async fn extractor_without_middleware() {
    let c = App::new()
        .get("/", |a: RequestId, b: RequestId| async move {
            assert_eq!(a, b, "stable within a request");
            a.as_str().to_owned()
        })
        .test();
    assert_eq!(
        c.get("/")
            .header("x-request-id", "from-proxy")
            .send()
            .await
            .text(),
        "from-proxy"
    );
    let generated = c
        .get("/")
        .header("x-request-id", "bad value")
        .send()
        .await
        .text();
    assert!(is_uuid_v7(&generated), "{generated}");
    assert_eq!(c.get("/").send().await.status(), 200);
}

#[tokio::test]
async fn request_id_visible_to_route_middleware() {
    let c = App::new()
        .wrap(SetRequestId::new())
        .get("/", || async { "ok" })
        .wrap_route(|req: Request, next: Next| async move {
            let id = req
                .request_id()
                .map(|id| id.to_string())
                .unwrap_or_default();
            let mut res = next.run(req).await;
            res.headers_mut()
                .insert("x-seen", HeaderValue::from_str(&id).unwrap());
            res
        })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.header("x-seen"), r.header("x-request-id"));
    assert!(RequestId::parse("ok-id").is_some());
    assert!(RequestId::parse("").is_none());
}

// ---------------------------------------------------------------------------
// Security headers

#[tokio::test]
async fn security_header_defaults() {
    let c = App::new()
        .wrap(SecurityHeaders::default())
        .get("/", || async { "ok" })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    assert_eq!(
        r.header("referrer-policy"),
        Some("strict-origin-when-cross-origin")
    );
    assert_eq!(r.header("cross-origin-opener-policy"), Some("same-origin"));
    assert_eq!(r.header("x-dns-prefetch-control"), Some("off"));
    assert!(r
        .header("permissions-policy")
        .unwrap()
        .contains("camera=()"));
    assert_eq!(r.header("content-security-policy"), None);
    assert_eq!(r.header("strict-transport-security"), None);
    // Errors and 404s are covered too.
    assert_eq!(
        c.get("/nope").send().await.header("x-frame-options"),
        Some("DENY")
    );
}

#[tokio::test]
async fn strict_security_headers() {
    let c = App::new()
        .wrap(SecurityHeaders::strict())
        .get("/", || async { "ok" })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(
        r.header("strict-transport-security"),
        Some("max-age=63072000; includeSubDomains; preload")
    );
    let csp = r.header("content-security-policy").unwrap();
    assert!(csp.contains("default-src 'self'"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(
        r.header("cross-origin-resource-policy"),
        Some("same-origin")
    );
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
}

#[tokio::test]
async fn security_header_builder() {
    let c = App::new()
        .wrap(
            SecurityHeaders::default()
                .hsts(Duration::from_secs(600), false, false)
                .csp("default-src 'none'")
                .csp_report_only("script-src 'self'")
                .frame_options("SAMEORIGIN")
                .referrer_policy("no-referrer")
                .permissions_policy("geolocation=(self)")
                .header("x-custom", "1")
                .remove("X-DNS-Prefetch-Control"),
        )
        .get("/", || async { "ok" })
        .get("/framed", || async {
            (
                [(
                    header::X_FRAME_OPTIONS,
                    HeaderValue::from_static("ALLOW-FROM x"),
                )],
                "framed",
            )
        })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.header("strict-transport-security"), Some("max-age=600"));
    assert_eq!(
        r.header("content-security-policy"),
        Some("default-src 'none'")
    );
    assert_eq!(
        r.header("content-security-policy-report-only"),
        Some("script-src 'self'")
    );
    assert_eq!(r.header("x-frame-options"), Some("SAMEORIGIN"));
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(r.header("permissions-policy"), Some("geolocation=(self)"));
    assert_eq!(r.header("x-custom"), Some("1"));
    assert_eq!(r.header("x-dns-prefetch-control"), None);
    assert_eq!(r.headers().get_all("x-frame-options").iter().count(), 1);

    // Handler-set headers are never overwritten.
    let r = c.get("/framed").send().await;
    assert_eq!(r.header("x-frame-options"), Some("ALLOW-FROM x"));
}

#[test]
#[should_panic(expected = "invalid value for security header")]
fn invalid_security_header_panics_at_startup() {
    let _ = SecurityHeaders::default().csp("bad\nvalue");
}

// ---------------------------------------------------------------------------
// CatchPanic

#[tokio::test]
async fn catches_handler_panics() {
    let c = App::new()
        .wrap(CatchPanic::new())
        .get("/str", || async {
            if true {
                panic!("static message");
            }
            "unreachable"
        })
        .get("/string/{n}", |Path(n): Path<u32>| async move {
            if n > 0 {
                panic!("formatted {n}");
            }
            "unreachable"
        })
        .get("/sync", || -> std::future::Ready<&'static str> {
            panic!("before the future")
        })
        .get("/ok", || async { "fine" })
        .test();
    for path in ["/str", "/string/3", "/sync"] {
        let r = c.get(path).send().await;
        assert_eq!(r.status(), 500, "{path}");
        assert_eq!(
            r.text(),
            r#"{"error":"internal server error","code":"panic"}"#
        );
    }
    assert_eq!(c.get("/ok").send().await.text(), "fine");
}

#[tokio::test]
async fn catches_panics_in_route_middleware_and_extractors() {
    struct Exploding;
    impl zapline::extract::FromRequestParts for Exploding {
        async fn from_request_parts(_: &mut Request) -> Result<Self> {
            panic!("extractor exploded")
        }
    }
    let c = App::new()
        .wrap(CatchPanic::new())
        .get("/extract", |_: Exploding| async { "never" })
        .get("/mw", || async { "never" })
        .wrap_route(|req: Request, next: Next| async move {
            if req.path() == "/mw" {
                panic!("middleware exploded");
            }
            next.run(req).await
        })
        .test();
    assert_eq!(c.get("/extract").send().await.status(), 500);
    assert_eq!(c.get("/mw").send().await.status(), 500);
}

#[tokio::test]
async fn custom_panic_handler() {
    let c = App::new()
        .wrap(
            CatchPanic::new()
                .handler(|msg: String| (StatusCode::SERVICE_UNAVAILABLE, format!("caught: {msg}"))),
        )
        .get("/{n}", |Path(n): Path<u32>| async move {
            if n > 0 {
                panic!("number {n}");
            }
            "zero"
        })
        .test();
    let r = c.get("/7").send().await;
    assert_eq!(r.status(), 503);
    assert_eq!(r.text(), "caught: number 7");
    assert_eq!(c.get("/0").send().await.text(), "zero");

    // A panicking handler falls back to the default response.
    let c = App::new()
        .wrap(CatchPanic::new().handler(|_: String| -> &'static str { panic!("handler too") }))
        .get("/", || async {
            if true {
                panic!("first");
            }
            ""
        })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.status(), 500);
    assert!(r.text().contains(r#""code":"panic""#));
}

#[tokio::test]
async fn panic_over_a_real_socket_keeps_the_server_alive() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        App::new()
            .wrap(CatchPanic::new())
            .get("/boom", || async {
                if true {
                    panic!("boom");
                }
                ""
            })
            .get("/", || async { "alive" })
            .serve(listener),
    );
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /boom HTTP/1.1\r\nHost: x\r\n\r\nGET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 500"), "{out}");
    assert!(
        out.contains("alive"),
        "same connection keeps working: {out}"
    );
}

// ---------------------------------------------------------------------------
// ConcurrencyLimit

struct Gate {
    entered: Notify,
    release: Notify,
}

fn gated_app(limit: ConcurrencyLimit) -> (zapline::test::TestClient, Arc<Gate>) {
    let gate = Arc::new(Gate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let g = gate.clone();
    let c = App::new()
        .wrap(limit)
        .get("/slow", move || {
            let g = g.clone();
            async move {
                g.entered.notify_one();
                g.release.notified().await;
                "slow"
            }
        })
        .get("/fast", || async { "fast" })
        .test();
    (c, gate)
}

#[tokio::test]
async fn sheds_load_immediately_by_default() {
    let limit = ConcurrencyLimit::new(1);
    assert_eq!(limit.max(), 1);
    let (c, gate) = gated_app(limit.clone());
    let busy = tokio::spawn(c.get("/slow").send());
    gate.entered.notified().await;
    assert_eq!(limit.available(), 0);

    let r = c.get("/fast").send().await;
    assert_eq!(r.status(), 503);
    assert_eq!(r.header("retry-after"), Some("1"));
    assert!(r.text().contains(r#""code":"overloaded""#));

    gate.release.notify_one();
    assert_eq!(busy.await.unwrap().text(), "slow");
    assert_eq!(limit.available(), 1);
    assert_eq!(c.get("/fast").send().await.status(), 200);
}

#[tokio::test]
async fn queues_up_to_the_timeout() {
    let limit = ConcurrencyLimit::new(1).queue_timeout(Duration::from_secs(5));
    let (c, gate) = gated_app(limit);
    let busy = tokio::spawn(c.get("/slow").send());
    gate.entered.notified().await;
    let queued = tokio::spawn(c.get("/fast").send());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!queued.is_finished(), "waits for a slot");
    gate.release.notify_one();
    assert_eq!(busy.await.unwrap().status(), 200);
    assert_eq!(queued.await.unwrap().text(), "fast");
}

#[tokio::test]
async fn queue_timeout_expires() {
    let limit = ConcurrencyLimit::new(1)
        .queue_timeout(Duration::from_millis(30))
        .retry_after(7);
    let (c, gate) = gated_app(limit);
    let busy = tokio::spawn(c.get("/slow").send());
    gate.entered.notified().await;
    let r = c.get("/fast").send().await;
    assert_eq!(r.status(), 503);
    assert_eq!(r.header("retry-after"), Some("7"));
    gate.release.notify_one();
    busy.await.unwrap();
}

#[tokio::test]
async fn concurrency_limit_frees_slots_on_errors_and_panics() {
    let c = App::new()
        .wrap(CatchPanic::new())
        .wrap(ConcurrencyLimit::new(1))
        .get("/err", || async {
            Err::<&str, _>(Error::bad_request("no"))
        })
        .get("/panic", || async {
            if true {
                panic!("x");
            }
            ""
        })
        .get("/", || async { "ok" })
        .test();
    for _ in 0..3 {
        assert_eq!(c.get("/err").send().await.status(), 400);
        assert_eq!(c.get("/panic").send().await.status(), 500);
        assert_eq!(c.get("/").send().await.status(), 200);
    }
}

#[test]
#[should_panic(expected = "ConcurrencyLimit")]
fn zero_concurrency_panics_at_startup() {
    let _ = ConcurrencyLimit::new(0);
}

// ---------------------------------------------------------------------------
// NormalizePath

fn path_app(mw: NormalizePath) -> zapline::test::TestClient {
    App::new()
        .wrap(mw)
        .get("/", || async { "root" })
        .get("/users", |uri: Uri| async move { format!("users {uri}") })
        .get("/users/{id}", |Path(id): Path<String>| async move {
            format!("user {id}")
        })
        .fallback(|uri: Uri| async move { (StatusCode::NOT_FOUND, format!("fallback {uri}")) })
        .test()
}

#[tokio::test]
async fn trims_trailing_slashes() {
    let c = path_app(NormalizePath::trim_trailing_slash());
    assert_eq!(c.get("/users").send().await.text(), "users /users");
    assert_eq!(c.get("/users/").send().await.text(), "users /users");
    assert_eq!(c.get("/users///").send().await.text(), "users /users");
    assert_eq!(
        c.get("/users/?a=1&b=2").send().await.text(),
        "users /users?a=1&b=2"
    );
    assert_eq!(c.get("/users/7/").send().await.text(), "user 7");
    assert_eq!(c.get("/").send().await.text(), "root");
    assert_eq!(c.get("/users/a%2F/").send().await.text(), "user a/");
    // Repeated inner slashes are kept unless merging is on.
    assert_eq!(c.get("//users").send().await.status(), 404);

    // Without the middleware, the trailing slash doesn't match.
    let plain = App::new().get("/users", || async { "users" }).test();
    assert_eq!(plain.get("/users/").send().await.status(), 404);
}

#[tokio::test]
async fn merges_slashes() {
    let c = path_app(NormalizePath::trim_trailing_slash().merge_slashes(true));
    assert_eq!(c.get("//users").send().await.text(), "users /users");
    assert_eq!(c.get("/users//7//").send().await.text(), "user 7");
    assert_eq!(c.get("///").send().await.text(), "root");
    assert_eq!(
        c.get("//users//?q=a//b").send().await.text(),
        "users /users?q=a//b"
    );
}

#[tokio::test]
async fn appends_trailing_slashes() {
    let c = path_app(NormalizePath::append_trailing_slash());
    // Route patterns never end in `/`, so appended paths reach the fallback.
    assert_eq!(c.get("/docs").send().await.text(), "fallback /docs/");
    assert_eq!(
        c.get("/docs?x=1").send().await.text(),
        "fallback /docs/?x=1"
    );
    assert_eq!(c.get("/docs/").send().await.text(), "fallback /docs/");
    // File-like last segments are left alone.
    assert_eq!(c.get("/app.js").send().await.text(), "fallback /app.js");
    assert_eq!(c.get("/").send().await.text(), "root");
}

#[tokio::test]
async fn redirects_instead_of_rewriting() {
    let c = path_app(NormalizePath::trim_trailing_slash().redirect(StatusCode::PERMANENT_REDIRECT));
    let r = c.get("/users/?page=2").send().await;
    assert_eq!(r.status(), 308);
    assert_eq!(r.header("location"), Some("/users?page=2"));
    assert_eq!(c.get("/users").send().await.text(), "users /users");

    let c =
        path_app(NormalizePath::append_trailing_slash().redirect(StatusCode::MOVED_PERMANENTLY));
    let r = c.post("/docs").send().await;
    assert_eq!(r.status(), 301);
    assert_eq!(r.header("location"), Some("/docs/"));
}

#[tokio::test]
async fn redirects_never_point_off_site() {
    let c = path_app(NormalizePath::trim_trailing_slash().redirect(StatusCode::FOUND));
    let r = c.get("//evil.example/").send().await;
    assert_eq!(r.status(), 302);
    assert_eq!(r.header("location"), Some("/evil.example"));
    let r = c.get("///evil.example//").send().await;
    assert_eq!(r.header("location"), Some("/evil.example"));
}

#[test]
#[should_panic(expected = "3xx")]
fn non_redirect_status_panics_at_startup() {
    let _ = NormalizePath::trim_trailing_slash().redirect(StatusCode::OK);
}

// ---------------------------------------------------------------------------
// CORS, Timeout, DefaultHeaders, Logger

fn cors_app(cors: Cors) -> zapline::test::TestClient {
    App::new().wrap(cors).get("/x", || async { "x" }).test()
}

#[tokio::test]
async fn cors_origin_list_and_predicate() {
    let c = cors_app(
        Cors::origins(["https://a.com"])
            .allow_origin_fn(|o: &HeaderValue| o.as_bytes().ends_with(b".preview.dev")),
    );
    let get = |origin: &'static str| c.get("/x").header("origin", origin).send();
    let r = get("https://a.com").await;
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("https://a.com")
    );
    assert_eq!(r.header("vary"), Some("origin"));
    let r = get("https://pr-1.preview.dev").await;
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("https://pr-1.preview.dev")
    );
    let r = get("https://evil.com").await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("access-control-allow-origin"), None);
    assert_eq!(r.header("vary"), Some("origin"), "caches must still vary");
    // No Origin at all: still varies.
    assert_eq!(c.get("/x").send().await.header("vary"), Some("origin"));
}

#[tokio::test]
async fn cors_permissive_has_no_overhead_without_origin() {
    let c = cors_app(Cors::permissive());
    let r = c.get("/x").send().await;
    assert_eq!(r.header("access-control-allow-origin"), None);
    assert_eq!(r.header("vary"), None);
}

#[tokio::test]
async fn cors_preflight_details() {
    let c = cors_app(
        Cors::origins(["https://a.com"])
            .allow_methods("GET, POST")
            .allow_headers("content-type, x-token")
            .max_age(Duration::from_secs(600)),
    );
    let r = c
        .request(Method::OPTIONS, "/x")
        .header("origin", "https://a.com")
        .header("access-control-request-method", "POST")
        .send()
        .await;
    assert_eq!(r.status(), 204);
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("https://a.com")
    );
    assert_eq!(r.header("access-control-allow-methods"), Some("GET, POST"));
    assert_eq!(
        r.header("access-control-allow-headers"),
        Some("content-type, x-token")
    );
    assert_eq!(r.header("access-control-max-age"), Some("600"));
    assert!(r
        .header("vary")
        .unwrap()
        .contains("access-control-request-method"));

    let r = c
        .request(Method::OPTIONS, "/x")
        .header("origin", "https://evil.com")
        .header("access-control-request-method", "POST")
        .send()
        .await;
    assert_eq!(r.status(), 204);
    assert_eq!(r.header("access-control-allow-origin"), None);
    assert_eq!(r.header("access-control-allow-methods"), None);
}

#[tokio::test]
async fn cors_credentials_echo_instead_of_wildcards() {
    let c = cors_app(
        Cors::permissive()
            .allow_credentials()
            .expose_headers("x-request-id"),
    );
    let r = c
        .request(Method::OPTIONS, "/x")
        .header("origin", "https://a.com")
        .header("access-control-request-method", "PUT")
        .header("access-control-request-headers", "x-a, x-b")
        .send()
        .await;
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("https://a.com")
    );
    assert_eq!(r.header("access-control-allow-credentials"), Some("true"));
    assert_eq!(r.header("access-control-allow-headers"), Some("x-a, x-b"));

    let r = c.get("/x").header("origin", "https://b.com").send().await;
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("https://b.com")
    );
    assert_eq!(r.header("access-control-allow-credentials"), Some("true"));
    assert_eq!(
        r.header("access-control-expose-headers"),
        Some("x-request-id")
    );
    assert_eq!(r.header("vary"), Some("origin"));
}

#[tokio::test]
async fn timeout_returns_503_with_code() {
    let c = App::new()
        .wrap(Timeout::new(Duration::from_millis(20)))
        .get("/slow", || async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            "late"
        })
        .get("/fast", || async { "fast" })
        .test();
    let r = c.get("/slow").send().await;
    assert_eq!(r.status(), 503);
    assert_eq!(
        r.text(),
        r#"{"error":"request timed out","code":"timeout"}"#
    );
    assert_eq!(c.get("/fast").send().await.text(), "fast");
}

#[tokio::test]
async fn default_headers_do_not_override() {
    let c = App::new()
        .wrap(
            DefaultHeaders::new()
                .add(header::SERVER, HeaderValue::from_static("zapline"))
                .add(header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        )
        .get("/", || async { "x" })
        .get("/cached", || async {
            (
                [(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("max-age=60"),
                )],
                "x",
            )
        })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.header("server"), Some("zapline"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let r = c.get("/cached").send().await;
    assert_eq!(r.header("cache-control"), Some("max-age=60"));
}

#[tokio::test]
async fn logger_passes_responses_through() {
    let c = App::new()
        .wrap(Logger)
        .wrap(SetRequestId::new())
        .get("/users/{id}", || async { "user" })
        .wrap_route(Logger)
        .test();
    let r = c.get("/users/1").send().await;
    assert_eq!(r.text(), "user");
    assert_eq!(c.get("/missing").send().await.status(), 404);
}

// ---------------------------------------------------------------------------
// Composition

#[tokio::test]
async fn full_stack_ordering() {
    use zapline::middleware::RateLimit;

    let c = App::new()
        // Outermost first: everything below gets an ID and security headers,
        // including panics, 404s, rejections and normalized paths.
        .wrap(SetRequestId::new())
        .wrap(SecurityHeaders::default())
        .wrap(CatchPanic::new())
        .wrap(NormalizePath::trim_trailing_slash())
        .wrap(ConcurrencyLimit::new(64))
        .wrap(RateLimit::per_ip(5, Duration::from_secs(60)))
        .get("/users", || async { "users" })
        .get("/boom", || async {
            if true {
                panic!("boom");
            }
            ""
        })
        .test();

    let r = c.get("/users/").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.text(), "users");
    assert!(r.header("x-request-id").is_some());
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    assert_eq!(r.header("ratelimit-remaining"), Some("4"));

    let r = c.get("/boom").send().await;
    assert_eq!(r.status(), 500);
    assert!(r.header("x-request-id").is_some());
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));

    let r = c.get("/missing/").send().await;
    assert_eq!(r.status(), 404);
    assert!(r.header("x-request-id").is_some());

    for _ in 0..2 {
        c.get("/users").send().await;
    }
    let r = c.get("/users").send().await;
    assert_eq!(r.status(), 429);
    assert!(r.header("x-request-id").is_some(), "rejections are tagged");
    assert_eq!(
        r.header("referrer-policy"),
        Some("strict-origin-when-cross-origin")
    );
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn compression_composes_with_the_stack() {
    use zapline::middleware::Compression;

    let c = App::new()
        .wrap(SetRequestId::new())
        .wrap(SecurityHeaders::default())
        .wrap(Compression::new())
        .wrap(NormalizePath::trim_trailing_slash())
        .get("/big", || async { "zapline ".repeat(500) })
        .test();
    let r = c
        .get("/big/")
        .header("accept-encoding", "gzip")
        .send()
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert!(r.header("x-request-id").is_some());
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    assert!(r.bytes().len() < 200);
}
