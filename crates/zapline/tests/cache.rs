//! `Cache`, `ResponseCache` and `ETag`.
#![cfg(all(feature = "cache", feature = "macros"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use zapline::cache::{Cache, ETag, ResponseCache};
use zapline::prelude::*;

const MINUTE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn values_expire_and_keep_their_type() {
    let cache = Cache::memory();
    cache.set("n", &7u32, MINUTE).await.unwrap();
    cache.set("s", "text", MINUTE).await.unwrap();
    cache
        .set("short", &1, Duration::from_millis(30))
        .await
        .unwrap();
    assert_eq!(cache.get::<u32>("n").await, Some(7));
    assert_eq!(cache.get::<String>("s").await.as_deref(), Some("text"));
    assert_eq!(cache.get::<u32>("s").await, None, "wrong type is a miss");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(cache.get::<u32>("short").await, None);

    cache.delete("n").await;
    assert_eq!(cache.get::<u32>("n").await, None);
}

#[tokio::test]
async fn prefixes_and_clear() {
    let cache = Cache::memory();
    for key in ["posts:1", "posts:2", "users:1"] {
        cache.set(key, &true, MINUTE).await.unwrap();
    }
    cache.delete_prefix("posts:").await;
    assert_eq!(cache.get::<bool>("posts:1").await, None);
    assert_eq!(cache.get::<bool>("users:1").await, Some(true));
    cache.clear().await;
    assert_eq!(cache.get::<bool>("users:1").await, None);
}

#[tokio::test]
async fn a_full_cache_evicts_the_soonest_to_expire() {
    let cache = Cache::memory().max_entries(10);
    for i in 0..10u64 {
        cache
            .set(&format!("k{i}"), &i, Duration::from_secs(100 + i))
            .await
            .unwrap();
    }
    cache.set("new", &99, MINUTE * 10).await.unwrap();
    assert_eq!(cache.get::<u64>("new").await, Some(99));
    assert_eq!(
        cache.get::<u64>("k0").await,
        None,
        "soonest to expire went first"
    );
    assert_eq!(cache.get::<u64>("k9").await, Some(9));
}

#[tokio::test]
async fn get_or_set_computes_once_under_concurrency() {
    let cache = Cache::memory();
    let runs = Arc::new(AtomicUsize::new(0));
    let calls = (0..50).map(|_| {
        let (cache, runs) = (cache.clone(), runs.clone());
        tokio::spawn(async move {
            cache
                .get_or_set("slow", MINUTE, || async {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(String::from("value"))
                })
                .await
                .unwrap()
        })
    });
    for call in calls.collect::<Vec<_>>() {
        assert_eq!(call.await.unwrap(), "value");
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn errors_are_not_cached() {
    let cache = Cache::memory();
    let err = cache
        .get_or_set::<u32, _, _>("flaky", MINUTE, || async {
            Err(Error::service_unavailable("db down"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.status(), 503);
    let ok = cache
        .get_or_set("flaky", MINUTE, || async { Ok(5u32) })
        .await
        .unwrap();
    assert_eq!(ok, 5);
}

/// Counts how often the handlers really ran.
#[derive(Clone, Default)]
struct Hits(Arc<AtomicUsize>);

#[get("/page")]
async fn page(State(hits): State<Hits>) -> Html<String> {
    let n = hits.0.fetch_add(1, Ordering::SeqCst) + 1;
    Html(format!("<p>render {n}</p>"))
}

#[get("/login")]
async fn login() -> Response {
    let mut res = Html("form").into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_static("sid=1"));
    res
}

#[get("/private")]
async fn private() -> Response {
    let mut res = Html("mine").into_response();
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=60"),
    );
    res
}

fn cached_app(hits: &Hits, ttl: Duration) -> zapline::test::TestClient {
    App::new()
        .state(hits.clone())
        .nest(
            "/c",
            Routes::new()
                .mount((page, login, private))
                .wrap(ResponseCache::new(ttl)),
        )
        .test()
}

#[tokio::test]
async fn response_cache_serves_repeats_from_memory() {
    let hits = Hits::default();
    let client = cached_app(&hits, MINUTE);

    let first = client.get("/c/page").send().await;
    first
        .assert_status(200)
        .assert_header("x-cache", "MISS")
        .assert_header("cache-control", "public, max-age=60");
    let second = client.get("/c/page").send().await;
    second
        .assert_header("x-cache", "HIT")
        .assert_header("content-type", "text/html; charset=utf-8");
    assert_eq!(second.text(), "<p>render 1</p>");
    assert_eq!(hits.0.load(Ordering::SeqCst), 1);

    // Another query string, an htmx request and a logged-in user each miss.
    client
        .get("/c/page?x=1")
        .send()
        .await
        .assert_header("x-cache", "MISS");
    client
        .get("/c/page")
        .header("hx-request", "true")
        .send()
        .await
        .assert_header("x-cache", "MISS");
    let with_cookie = client.get("/c/page").header("cookie", "sid=1").send().await;
    assert!(with_cookie.header("x-cache").is_none());
    assert_eq!(hits.0.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn response_cache_skips_personal_responses_and_expires() {
    let hits = Hits::default();
    let client = cached_app(&hits, Duration::from_millis(40));
    for path in ["/c/login", "/c/private"] {
        client.get(path).send().await;
        let res = client.get(path).send().await;
        assert!(res.header("x-cache").is_none(), "{path} must not be cached");
    }

    client.get("/c/page").send().await;
    client
        .get("/c/page")
        .send()
        .await
        .assert_header("x-cache", "HIT");
    tokio::time::sleep(Duration::from_millis(60)).await;
    client
        .get("/c/page")
        .send()
        .await
        .assert_header("x-cache", "MISS");
}

#[tokio::test]
async fn etag_answers_304_for_unchanged_bodies() {
    let hits = Hits::default();
    let client = App::new().state(hits.clone()).wrap(ETag).mount(page).test();

    let res = client.get("/page").send().await;
    res.assert_status(200);
    let tag = res.header("etag").expect("an etag").to_owned();
    assert!(tag.starts_with("W/\""), "{tag}");

    // The body changes on every render here, so the old tag no longer matches.
    client
        .get("/page")
        .header("if-none-match", tag.as_str())
        .send()
        .await
        .assert_status(200);

    let static_client = App::new().wrap(ETag).mount(login).test();
    let tag = static_client
        .get("/login")
        .send()
        .await
        .header("etag")
        .unwrap()
        .to_owned();
    let res = static_client
        .get("/login")
        .header("if-none-match", tag.as_str())
        .send()
        .await;
    res.assert_status(304).assert_header("etag", &tag);
    assert!(res.text().is_empty());
}

#[tokio::test]
async fn handlers_take_the_cache_as_an_argument() {
    #[get("/n")]
    async fn n(cache: Cache) -> Result<String> {
        let v: u32 = cache.get_or_set("n", MINUTE, || async { Ok(1) }).await?;
        Ok(v.to_string())
    }
    let client = App::new().cache(Cache::memory()).mount(n).test();
    assert_eq!(client.get("/n").send().await.text(), "1");
    App::new()
        .mount(n)
        .test()
        .get("/n")
        .send()
        .await
        .assert_status(500);
}

#[tokio::test]
async fn head_requests_never_fill_the_response_cache() {
    let hits = Hits::default();
    let client = cached_app(&hits, MINUTE);
    client
        .request(Method::HEAD, "/c/page")
        .send()
        .await
        .assert_status(200);
    let get = client.get("/c/page").send().await;
    get.assert_header("x-cache", "MISS");
    assert!(get.text().starts_with("<p>render"), "{}", get.text());
    // HEAD may still answer from what a GET stored.
    client
        .request(Method::HEAD, "/c/page")
        .send()
        .await
        .assert_header("x-cache", "HIT");
}
