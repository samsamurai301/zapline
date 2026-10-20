//! Rate limiting: GCRA quotas, keys, trusted proxies, per-route policies
//! and the IETF `RateLimit-*` headers.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use zapline::header::HeaderName;
use zapline::middleware::rate_limit::{ClientIpHeader, TrustedProxies};
use zapline::middleware::{RateLimit, RateLimitPolicy};
use zapline::prelude::*;
use zapline::test::{TestClient, TestResponse};

const MIN: Duration = Duration::from_secs(60);

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn app(mw: RateLimit) -> TestClient {
    App::new().wrap(mw).get("/", || async { "ok" }).test()
}

async fn get_from(c: &TestClient, peer: &str) -> TestResponse {
    c.get("/").remote_addr(addr(peer)).send().await
}

#[tokio::test]
async fn exhausts_then_rejects_with_429() {
    let c = app(RateLimit::per_ip(2, MIN));

    let r = c.get("/").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("ratelimit-limit"), Some("2"));
    assert_eq!(r.header("ratelimit-remaining"), Some("1"));
    assert_eq!(r.header("ratelimit-reset"), Some("30"));

    let r = c.get("/").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("ratelimit-remaining"), Some("0"));
    assert_eq!(r.header("ratelimit-reset"), Some("60"));

    let r = c.get("/").send().await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(
        r.text(),
        r#"{"error":"too many requests","code":"rate_limited"}"#
    );
    // The next slot frees up after one emission interval (30 s).
    let retry: u64 = r.header("retry-after").unwrap().parse().unwrap();
    assert!((29..=30).contains(&retry), "retry-after = {retry}");
    assert_eq!(r.header("ratelimit-limit"), Some("2"));
    assert_eq!(r.header("ratelimit-remaining"), Some("0"));
}

#[tokio::test]
async fn recovers_after_the_interval() {
    let c = app(RateLimit::per_ip(1, Duration::from_millis(400)));
    assert_eq!(c.get("/").send().await.status(), 200);
    let r = c.get("/").send().await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.header("retry-after"), Some("1"), "rounded up, at least 1");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(c.get("/").send().await.status(), 200);
}

#[tokio::test]
async fn burst_limits_back_to_back_requests() {
    let c = app(RateLimit::per_ip(100, MIN).burst(3));
    for _ in 0..3 {
        assert_eq!(c.get("/").send().await.status(), 200);
    }
    let r = c.get("/").send().await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.header("retry-after"), Some("1"));
}

#[tokio::test]
async fn per_ip_isolation() {
    let c = app(RateLimit::per_ip(1, MIN));
    assert_eq!(get_from(&c, "10.0.0.1:1000").await.status(), 200);
    assert_eq!(
        get_from(&c, "10.0.0.1:2000").await.status(),
        429,
        "port ignored"
    );
    assert_eq!(get_from(&c, "10.0.0.2:1000").await.status(), 200);
    // IPv4-mapped IPv6 is the same client as plain IPv4.
    assert_eq!(get_from(&c, "[::ffff:10.0.0.2]:1").await.status(), 429);
}

#[tokio::test]
async fn ipv6_clients_are_grouped_by_prefix() {
    let c = app(RateLimit::per_ip(1, MIN));
    assert_eq!(get_from(&c, "[2001:db8::1]:1").await.status(), 200);
    assert_eq!(
        get_from(&c, "[2001:db8::ffff]:1").await.status(),
        429,
        "same /64"
    );
    assert_eq!(get_from(&c, "[2001:db8:0:1::1]:1").await.status(), 200);

    let c = app(RateLimit::per_ip(1, MIN).ipv6_prefix(128));
    assert_eq!(get_from(&c, "[2001:db8::1]:1").await.status(), 200);
    assert_eq!(get_from(&c, "[2001:db8::2]:1").await.status(), 200);
}

#[tokio::test]
async fn global_limit_is_shared_by_everyone() {
    let c = app(RateLimit::global(2, MIN));
    assert_eq!(get_from(&c, "1.1.1.1:1").await.status(), 200);
    assert_eq!(get_from(&c, "2.2.2.2:1").await.status(), 200);
    assert_eq!(get_from(&c, "3.3.3.3:1").await.status(), 429);
}

#[tokio::test]
async fn global_limit_is_exact_under_concurrency() {
    let c = app(RateLimit::global(25, MIN));
    let tasks: Vec<_> = (0..100).map(|_| tokio::spawn(c.get("/").send())).collect();
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap().status() == 200 {
            ok += 1;
        }
    }
    assert_eq!(ok, 25);
}

#[tokio::test]
async fn keyed_by_header_falls_back_to_ip() {
    let c = app(RateLimit::by_header("x-api-key", 1, MIN));
    let with_key = |k: &'static str| c.get("/").header("x-api-key", k).send();
    assert_eq!(with_key("a").await.status(), 200);
    assert_eq!(with_key("a").await.status(), 429);
    assert_eq!(with_key("b").await.status(), 200);
    // No key: limited per IP, in a namespace separate from key values.
    assert_eq!(get_from(&c, "10.1.1.1:1").await.status(), 200);
    assert_eq!(get_from(&c, "10.1.1.1:1").await.status(), 429);
    assert_eq!(get_from(&c, "10.1.1.2:1").await.status(), 200);
}

#[tokio::test]
async fn custom_key_and_none_skips() {
    let c = app(RateLimit::by_key(1, MIN, |req: &Request| {
        req.headers()
            .get("x-user")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }));
    let user = |u: &'static str| c.get("/").header("x-user", u).send();
    assert_eq!(user("ada").await.status(), 200);
    assert_eq!(user("ada").await.status(), 429);
    assert_eq!(user("bob").await.status(), 200);
    // `None` = not limited at all.
    for _ in 0..5 {
        let r = c.get("/").send().await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.header("ratelimit-limit"), None);
    }
}

#[tokio::test]
async fn exempt_requests_are_not_counted() {
    let c = App::new()
        .wrap(RateLimit::per_ip(1, MIN).exempt(|req: &Request| req.path() == "/healthz"))
        .get("/", || async { "ok" })
        .get("/healthz", || async { "up" })
        .test();
    for _ in 0..3 {
        let r = c.get("/healthz").send().await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.header("ratelimit-remaining"), None);
    }
    assert_eq!(c.get("/").send().await.status(), 200);
    assert_eq!(c.get("/").send().await.status(), 429);
}

#[tokio::test]
async fn headers_can_be_disabled() {
    let c = app(RateLimit::per_ip(1, MIN).headers(false));
    let r = c.get("/").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("ratelimit-limit"), None);
    let r = c.get("/").send().await;
    assert_eq!(r.status(), 429);
    assert!(r.header("retry-after").is_some());
    assert_eq!(r.header("ratelimit-remaining"), None);
}

#[tokio::test]
async fn global_middleware_limits_404s_too() {
    let c = app(RateLimit::per_ip(1, MIN));
    assert_eq!(c.get("/nope").send().await.status(), 404);
    assert_eq!(c.get("/nope").send().await.status(), 429);
}

#[tokio::test]
async fn untrusted_peer_cannot_spoof_forwarded_for() {
    let c = app(RateLimit::per_ip(1, MIN));
    let spoof = |ip: &'static str| {
        c.get("/")
            .remote_addr(addr("9.9.9.9:1"))
            .header("x-forwarded-for", ip)
            .send()
    };
    assert_eq!(spoof("1.1.1.1").await.status(), 200);
    assert_eq!(spoof("2.2.2.2").await.status(), 429, "header ignored");

    // Trusting some proxies doesn't make other peers trusted.
    let c = app(RateLimit::per_ip(1, MIN).trust_forwarded_for(["10.0.0.0/8"]));
    let r = |xff: &'static str| {
        c.get("/")
            .remote_addr(addr("9.9.9.9:1"))
            .header("x-forwarded-for", xff)
            .send()
    };
    assert_eq!(r("1.1.1.1").await.status(), 200);
    assert_eq!(r("2.2.2.2").await.status(), 429);
}

#[tokio::test]
async fn trusted_proxy_forwarded_for_uses_rightmost_untrusted() {
    let c = app(RateLimit::per_ip(1, MIN).trust_forwarded_for(["10.0.0.0/8", "192.168.1.1"]));
    let via_proxy = |xff: &'static str| {
        c.get("/")
            .remote_addr(addr("10.0.0.1:1"))
            .header("x-forwarded-for", xff)
            .send()
    };
    assert_eq!(via_proxy("1.1.1.1").await.status(), 200);
    assert_eq!(via_proxy("2.2.2.2").await.status(), 200, "distinct clients");
    // A client-supplied prefix doesn't change the identity...
    assert_eq!(via_proxy("6.6.6.6, 1.1.1.1").await.status(), 429);
    assert_eq!(via_proxy("7.7.7.7,1.1.1.1").await.status(), 429);
    // ...and trusted hops are skipped.
    assert_eq!(
        via_proxy("2.2.2.2, 192.168.1.1, 10.9.9.9").await.status(),
        429
    );
    assert_eq!(via_proxy("3.3.3.3, 192.168.1.1").await.status(), 200);
}

#[tokio::test]
async fn client_ip_resolution() {
    let proxies = TrustedProxies::new(["10.0.0.0/8"]);
    let forwarded = TrustedProxies::new(["10.0.0.0/8"]).header(ClientIpHeader::Forwarded);
    let real_ip = TrustedProxies::new(["10.0.0.0/8"])
        .header(ClientIpHeader::Single(HeaderName::from_static("x-real-ip")));
    let all = Arc::new([proxies, forwarded, real_ip]);
    let c = App::new()
        .get("/{i}", move |Path(i): Path<usize>, req: Request| {
            let all = all.clone();
            async move {
                all[i]
                    .client_ip(&req)
                    .map_or("none".to_owned(), |ip| ip.to_string())
            }
        })
        .test();
    let ip = |path: &'static str, peer: &'static str, hdr: &'static str, v: &'static str| {
        let mut r = c.get(path).remote_addr(addr(peer));
        if !hdr.is_empty() {
            r = r.header(hdr, v);
        }
        async move { r.send().await.text() }
    };
    let xff = "x-forwarded-for";
    assert_eq!(ip("/0", "10.0.0.1:1", "", "").await, "10.0.0.1");
    assert_eq!(ip("/0", "10.0.0.1:1", xff, "1.2.3.4").await, "1.2.3.4");
    assert_eq!(
        ip("/0", "10.0.0.1:1", xff, "[2001:db8::1]:443").await,
        "2001:db8::1"
    );
    assert_eq!(ip("/0", "10.0.0.1:1", xff, "1.2.3.4:5678").await, "1.2.3.4");
    // Every hop trusted: the left-most one is the best we know.
    assert_eq!(
        ip("/0", "10.0.0.1:1", xff, "10.0.0.7, 10.0.0.8").await,
        "10.0.0.7"
    );
    // Garbage in a trusted chain: stop at the last good hop.
    assert_eq!(
        ip("/0", "10.0.0.1:1", xff, "1.1.1.1, junk, 10.0.0.9").await,
        "10.0.0.9"
    );
    assert_eq!(ip("/0", "8.8.8.8:1", xff, "1.2.3.4").await, "8.8.8.8");

    let fwd = "forwarded";
    assert_eq!(
        ip(
            "/1",
            "10.0.0.1:1",
            fwd,
            "for=1.1.1.1;proto=https, for=\"[2001:db8::7]:80\""
        )
        .await,
        "2001:db8::7"
    );
    assert_eq!(
        ip("/1", "10.0.0.1:1", fwd, "for=1.1.1.1, For=10.0.0.3").await,
        "1.1.1.1"
    );
    assert_eq!(
        ip("/1", "10.0.0.1:1", xff, "1.1.1.1").await,
        "10.0.0.1",
        "XFF ignored"
    );
    assert_eq!(ip("/1", "10.0.0.1:1", fwd, "for=unknown").await, "10.0.0.1");

    assert_eq!(
        ip("/2", "10.0.0.1:1", "x-real-ip", "4.4.4.4").await,
        "4.4.4.4"
    );
    assert_eq!(
        ip("/2", "5.5.5.5:1", "x-real-ip", "4.4.4.4").await,
        "5.5.5.5"
    );
    assert_eq!(
        ip("/2", "10.0.0.1:1", "x-real-ip", "nope").await,
        "10.0.0.1"
    );
}

#[test]
#[should_panic(expected = "invalid trusted proxy")]
fn invalid_proxy_panics_at_startup() {
    let _ = RateLimit::per_ip(1, MIN).trust_forwarded_for(["10.0.0.0/99"]);
}

#[test]
#[should_panic(expected = "at least 1")]
fn zero_limit_panics_at_startup() {
    let _ = RateLimit::per_ip(0, MIN);
}

#[tokio::test]
async fn per_route_policies_have_their_own_buckets() {
    let c = App::new()
        .post("/login", || async { "welcome" })
        .rate_limit(1, MIN)
        .get("/a", || async { "a" })
        .get("/b", || async { "b" })
        .get("/health", || async { "up" })
        .meta(RateLimitPolicy::exempt())
        .wrap_route(RateLimit::per_ip(2, MIN))
        .test();

    let r = c.post("/login").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("ratelimit-limit"), Some("1"));
    assert_eq!(c.post("/login").send().await.status(), 429);

    // Routes without a policy share the middleware's default bucket,
    // separate from the /login policy.
    let r = c.get("/a").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("ratelimit-limit"), Some("2"));
    assert_eq!(c.get("/b").send().await.status(), 200);
    assert_eq!(c.get("/a").send().await.status(), 429);

    for _ in 0..5 {
        let r = c.get("/health").send().await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.header("ratelimit-limit"), None);
    }
    // Unmatched requests never reach route middleware.
    assert_eq!(c.get("/missing").send().await.status(), 404);
}

#[tokio::test]
async fn policies_without_scope_are_per_route_and_scopes_share() {
    let policy = RateLimitPolicy::new(1, MIN);
    let auth = RateLimitPolicy::new(1, MIN).scope("auth");
    let api = Routes::new()
        .get("/x", || async { "x" })
        .get("/y", || async { "y" })
        .meta_all(policy)
        .post("/login", || async { "login" })
        .meta(auth.clone())
        .post("/register", || async { "register" })
        .meta(auth)
        .wrap(RateLimit::per_ip(100, MIN));
    let c = App::new().nest("/api", api).test();

    assert_eq!(c.get("/api/x").send().await.status(), 200);
    assert_eq!(c.get("/api/y").send().await.status(), 200, "own bucket");
    assert_eq!(c.get("/api/x").send().await.status(), 429);
    assert_eq!(c.post("/api/login").send().await.status(), 200);
    assert_eq!(
        c.post("/api/register").send().await.status(),
        429,
        "shared scope"
    );
}

#[tokio::test]
async fn burst_on_route_policy() {
    let c = Routes::new()
        .get("/", || async { "ok" })
        .meta(RateLimitPolicy::new(60, MIN).burst(2))
        .wrap(RateLimit::per_ip(1000, MIN));
    let c = App::new().mount(c).test();
    assert_eq!(c.get("/").send().await.status(), 200);
    assert_eq!(c.get("/").send().await.status(), 200);
    assert_eq!(c.get("/").send().await.status(), 429);
    assert_eq!(RateLimitPolicy::new(60, MIN).limit(), Some(60));
    assert_eq!(RateLimitPolicy::exempt().limit(), None);
}

#[tokio::test]
async fn global_without_policy_ignores_route_meta() {
    // Route policies are only visible to route middleware.
    let c = App::new()
        .wrap(RateLimit::per_ip(3, MIN))
        .get("/", || async { "ok" })
        .rate_limit(1, MIN)
        .test();
    assert_eq!(c.get("/").send().await.status(), 200);
    assert_eq!(c.get("/").send().await.status(), 200);
}

#[tokio::test]
async fn nested_limiters_report_the_tightest() {
    let c = App::new()
        .wrap(RateLimit::global(100, MIN))
        .get("/", || async { "ok" })
        .wrap_route(RateLimit::per_ip(5, MIN))
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.header("ratelimit-limit"), Some("5"));
    assert_eq!(r.header("ratelimit-remaining"), Some("4"));

    let c = App::new()
        .wrap(RateLimit::global(2, MIN))
        .get("/", || async { "ok" })
        .wrap_route(RateLimit::per_ip(5, MIN))
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.header("ratelimit-limit"), Some("2"));
    assert_eq!(r.header("ratelimit-remaining"), Some("1"));
}

#[tokio::test]
async fn clones_share_state_and_max_keys_bounds_memory() {
    let limiter = RateLimit::per_ip(1, MIN).max_keys(64);
    let c = App::new()
        .wrap(limiter.clone())
        .get("/", || async { "ok" })
        .test();
    let c2 = App::new().wrap(limiter).get("/", || async { "ok" }).test();
    assert_eq!(get_from(&c, "10.0.0.1:1").await.status(), 200);
    assert_eq!(
        get_from(&c2, "10.0.0.1:1").await.status(),
        429,
        "shared buckets"
    );

    // Far more clients than slots: still serving, oldest buckets evicted.
    for i in 0..2000u32 {
        let peer = format!("172.16.{}.{}:1", i / 250, i % 250);
        let r = c.get("/").remote_addr(addr(&peer)).send().await;
        assert_eq!(r.status(), 200);
    }
}

#[tokio::test]
async fn real_socket_uses_peer_address() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    tokio::spawn(
        App::new()
            .wrap(RateLimit::per_ip(1, MIN))
            .get("/", || async { "ok" })
            .serve(listener),
    );
    let mut s = tokio::net::TcpStream::connect(local).await.unwrap();
    s.write_all(
        b"GET / HTTP/1.1\r\nHost: x\r\n\r\nGET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
    assert!(out.contains("HTTP/1.1 429 Too Many Requests"), "{out}");
    assert!(out.contains("retry-after: "), "{out}");
}

#[tokio::test]
async fn forwarded_hops_are_decoded_lazily_and_canonicalized() {
    let proxies = Arc::new(TrustedProxies::new(["10.0.0.0/8"]));
    let forwarded = Arc::new(TrustedProxies::new(["10.0.0.0/8"]).header(ClientIpHeader::Forwarded));
    let c = App::new()
        .get("/xff", move |req: Request| {
            let p = proxies.clone();
            async move { p.client_ip(&req).unwrap().to_string() }
        })
        .get("/fwd", move |req: Request| {
            let p = forwarded.clone();
            async move { p.client_ip(&req).unwrap().to_string() }
        })
        .test();

    // Client-written invalid bytes on the left must not hide the hop the
    // trusted proxy appended on the right.
    let r = c
        .get("/xff")
        .remote_addr(addr("10.0.0.1:1"))
        .header(
            "x-forwarded-for",
            HeaderValue::from_bytes(b"\xff, 6.6.6.6").unwrap(),
        )
        .send()
        .await;
    assert_eq!(r.text(), "6.6.6.6");

    // Bracketed IPv4-mapped addresses are IPv4 clients, not one IPv6 /64.
    let r = c
        .get("/xff")
        .remote_addr(addr("10.0.0.1:1"))
        .header("x-forwarded-for", "[::ffff:1.2.3.4]:5555")
        .send()
        .await;
    assert_eq!(r.text(), "1.2.3.4");
    let r = c
        .get("/fwd")
        .remote_addr(addr("10.0.0.1:1"))
        .header("forwarded", r#"for="[::ffff:5.6.7.8]""#)
        .send()
        .await;
    assert_eq!(r.text(), "5.6.7.8");
}

#[tokio::test]
async fn ipv4_mapped_clients_get_separate_buckets() {
    let c = app(RateLimit::per_ip(1, Duration::from_secs(60)).trust_forwarded_for(["10.0.0.0/8"]));
    let send = |client: &'static str| {
        c.get("/")
            .remote_addr(addr("10.0.0.1:1"))
            .header("x-forwarded-for", format!("[::ffff:{client}]:443"))
            .send()
    };
    assert_eq!(send("1.1.1.1").await.status(), 200);
    assert_eq!(send("2.2.2.2").await.status(), 200);
    assert_eq!(send("1.1.1.1").await.status(), 429);
}
