//! Idempotency keys: replays, conflicts, key reuse, scoping and expiry.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use zapline::header::{HeaderName, HeaderValue};
use zapline::middleware::{CatchPanic, Idempotency};
use zapline::prelude::*;
use zapline::test::TestClient;

/// An app whose `POST /charge` counts its runs and answers with the count.
fn app(mw: Idempotency) -> (TestClient, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let c = App::new()
        .wrap(mw)
        .post("/charge", move || {
            let r = r.clone();
            async move {
                let n = r.fetch_add(1, Ordering::SeqCst) + 1;
                (StatusCode::CREATED, format!("charge #{n}"))
            }
        })
        .patch("/charge", || async { "patched" })
        .get("/charge", || async { "read" })
        .test();
    (c, runs)
}

#[tokio::test]
async fn a_retry_replays_the_stored_response() {
    let (c, runs) = app(Idempotency::new());
    let first = c
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", "k-1")
        .body("amount=10")
        .send()
        .await;
    assert_eq!(first.status(), 201);
    assert_eq!(first.text(), "charge #1");
    assert_eq!(first.header("idempotent-replayed"), None);

    let retry = c
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", "k-1")
        .body("amount=10")
        .send()
        .await;
    assert_eq!(retry.status(), 201);
    assert_eq!(retry.text(), "charge #1");
    assert_eq!(retry.header("idempotent-replayed"), Some("true"));
    assert_eq!(
        retry.header("content-type"),
        first.header("content-type"),
        "headers are replayed"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn quoted_keys_match_bare_ones() {
    let (c, runs) = app(Idempotency::new());
    c.post("/charge")
        .bearer("client")
        .header("idempotency-key", "abc")
        .send()
        .await;
    let r = c
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", "\"abc\"")
        .send()
        .await;
    assert_eq!(r.header("idempotent-replayed"), Some("true"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn requests_without_a_key_or_with_safe_methods_pass_through() {
    let (c, runs) = app(Idempotency::new());
    for _ in 0..2 {
        assert_eq!(c.post("/charge").send().await.status(), 201);
    }
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    let r = c
        .get("/charge")
        .bearer("client")
        .header("idempotency-key", "k")
        .send()
        .await;
    assert_eq!(r.text(), "read");
}

#[tokio::test]
async fn patch_is_covered_too() {
    let (c, _) = app(Idempotency::new());
    c.patch("/charge")
        .bearer("client")
        .header("idempotency-key", "p")
        .send()
        .await;
    let r = c
        .patch("/charge")
        .bearer("client")
        .header("idempotency-key", "p")
        .send()
        .await;
    assert_eq!(r.text(), "patched");
    assert_eq!(r.header("idempotent-replayed"), Some("true"));
}

#[tokio::test]
async fn reusing_a_key_for_another_request_is_422() {
    let (c, runs) = app(Idempotency::new());
    c.post("/charge")
        .bearer("client")
        .header("idempotency-key", "k-2")
        .body("amount=10")
        .send()
        .await;
    let r = c
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", "k-2")
        .body("amount=99")
        .send()
        .await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.text().contains(r#""code":"idempotency_key_reused""#));
    // Another path is another request as well.
    let r = c
        .post("/charge?x=1")
        .bearer("client")
        .header("idempotency-key", "k-2")
        .body("amount=10")
        .send()
        .await;
    assert_eq!(r.status(), 422);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_retry_during_the_first_request_is_409() {
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let c = App::new()
        .wrap(Idempotency::new())
        .post("/slow", move || {
            let gate = gate.clone();
            async move {
                gate.notified().await;
                "done"
            }
        })
        .test();

    let first = c
        .post("/slow")
        .bearer("client")
        .header("idempotency-key", "s")
        .send();
    let second = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let r = c
            .post("/slow")
            .bearer("client")
            .header("idempotency-key", "s")
            .send()
            .await;
        release.notify_one();
        r
    };
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.text(), "done");
    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert!(second.text().contains(r#""code":"idempotency_key_in_use""#));
}

#[tokio::test]
async fn required_keys_and_invalid_keys_are_400() {
    let (c, runs) = app(Idempotency::new().required());
    let r = c.post("/charge").send().await;
    assert_eq!(r.status(), 400);
    assert!(r.text().contains(r#""code":"idempotency_key_missing""#));
    assert_eq!(
        c.get("/charge").send().await.status(),
        200,
        "GET needs none"
    );

    for bad in ["has space", "\"\"", &"x".repeat(256)] {
        let r = c
            .post("/charge")
            .bearer("client")
            .header("idempotency-key", bad)
            .send()
            .await;
        assert_eq!(r.status(), 400, "{bad:?}");
        assert!(r.text().contains(r#""code":"idempotency_key_invalid""#));
    }
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn keys_are_scoped_by_authorization_by_default() {
    let (c, runs) = app(Idempotency::new());
    let alice = c
        .post("/charge")
        .bearer("alice")
        .header("idempotency-key", "same")
        .send()
        .await;
    let bob = c
        .post("/charge")
        .bearer("bob")
        .header("idempotency-key", "same")
        .send()
        .await;
    assert_eq!(alice.text(), "charge #1");
    assert_eq!(bob.text(), "charge #2", "bob never sees alice's response");
    assert_eq!(bob.header("idempotent-replayed"), None);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn requests_without_an_owner_skip_idempotency() {
    // No Authorization and no `.scope`: clients can't be told apart, so
    // sharing one key namespace would leak responses between them.
    let (c, runs) = app(Idempotency::new().required());
    for _ in 0..2 {
        let r = c
            .post("/charge")
            .header("idempotency-key", "anon")
            .send()
            .await;
        assert_eq!(r.status(), 201);
        assert_eq!(r.header("idempotent-replayed"), None);
    }
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_changed_content_type_is_a_different_request() {
    let (c, runs) = app(Idempotency::new());
    let send = |ct: &'static str| {
        c.post("/charge")
            .bearer("client")
            .header("idempotency-key", "ct")
            .header("content-type", ct)
            .body("{}")
            .send()
    };
    assert_eq!(send("application/json").await.status(), 201);
    assert_eq!(send("text/plain").await.status(), 422);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn multipart_uploads_pass_through_unbuffered() {
    let (c, runs) = app(Idempotency::new());
    for _ in 0..2 {
        let r = c
            .post("/charge")
            .bearer("client")
            .header("idempotency-key", "upload")
            .header("content-type", "multipart/form-data; boundary=x")
            .body("--x--\r\n")
            .send()
            .await;
        assert_eq!(r.header("idempotent-replayed"), None);
    }
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_slow_handler_keeps_its_key_past_the_lock_timeout() {
    slow_handler_keeps_its_key(Idempotency::new(), "slow").await;
}

#[cfg(feature = "redis")]
#[tokio::test]
async fn redis_renews_the_lock_of_a_slow_handler() {
    let Ok(url) = std::env::var("ZAPLINE_TEST_REDIS_URL") else {
        eprintln!("skipped: ZAPLINE_TEST_REDIS_URL is not set");
        return;
    };
    let redis = zapline::redis::Redis::new(&url).unwrap();
    let key = format!("{:?}", std::time::SystemTime::now()).replace(' ', "-");
    slow_handler_keeps_its_key(Idempotency::new().redis(redis), &key).await;
}

/// The handler takes 5 lock timeouts; a retry near the end still gets 409.
async fn slow_handler_keeps_its_key(mw: Idempotency, key: &str) {
    let lock = Duration::from_millis(60);
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let c = App::new()
        .wrap(mw.lock_timeout(lock))
        .post("/slow", move || {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                "done"
            }
        })
        .test();
    let send = || {
        c.post("/slow")
            .bearer("client")
            .header("idempotency-key", key)
            .send()
    };
    let retry = async {
        tokio::time::sleep(Duration::from_millis(240)).await;
        send().await
    };
    let (first, retry) = tokio::join!(send(), retry);
    assert_eq!(first.text(), "done");
    assert_eq!(retry.status(), 409, "the lock was renewed");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(send().await.header("idempotent-replayed"), Some("true"));
}

#[tokio::test]
async fn one_scope_cannot_flush_another_scopes_responses() {
    // max_keys 20: each scope may hold 2.
    let (c, runs) = app(Idempotency::new().max_keys(20));
    let send = |who: &'static str, key: String| {
        c.post("/charge")
            .bearer(who)
            .header("idempotency-key", key.as_str())
            .send()
    };
    send("victim", "pay".into()).await;
    for i in 0..50 {
        send("flooder", format!("junk-{i}")).await;
    }
    let retry = send("victim", "pay".into()).await;
    assert_eq!(retry.header("idempotent-replayed"), Some("true"));
    assert_eq!(runs.load(Ordering::SeqCst), 51);
}

#[tokio::test]
async fn custom_scopes_and_opting_out() {
    let mw = Idempotency::new().scope(|req| {
        req.headers()
            .get("x-account")?
            .to_str()
            .ok()
            .map(str::to_owned)
    });
    let (c, runs) = app(mw);
    let send = |account: Option<&'static str>| {
        let mut r = c
            .post("/charge")
            .bearer("client")
            .header("idempotency-key", "k");
        if let Some(a) = account {
            r = r.header("x-account", a);
        }
        r.send()
    };
    send(Some("a")).await;
    assert_eq!(
        send(Some("a")).await.header("idempotent-replayed"),
        Some("true")
    );
    assert_eq!(send(Some("b")).await.header("idempotent-replayed"), None);
    // No scope: idempotency is skipped, so both run.
    send(None).await;
    send(None).await;
    assert_eq!(runs.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn server_errors_are_not_stored() {
    server_errors_release_the_key(Idempotency::new(), "f").await;
}

#[cfg(feature = "redis")]
#[tokio::test]
async fn redis_releases_keys_after_server_errors() {
    let Ok(url) = std::env::var("ZAPLINE_TEST_REDIS_URL") else {
        eprintln!("skipped: ZAPLINE_TEST_REDIS_URL is not set");
        return;
    };
    let redis = zapline::redis::Redis::new(&url).unwrap();
    let key = format!("release-{:?}", std::time::SystemTime::now());
    let key = key.replace(' ', "-");
    server_errors_release_the_key(Idempotency::new().redis(redis), &key).await;
}

async fn server_errors_release_the_key(mw: Idempotency, key: &str) {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let key = key.to_owned();
    let c = App::new()
        .wrap(mw)
        .post("/flaky", move || {
            let r = r.clone();
            async move {
                if r.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Error::service_unavailable("try again"))
                } else {
                    Ok("ok")
                }
            }
        })
        .test();
    let send = || {
        c.post("/flaky")
            .bearer("client")
            .header("idempotency-key", key.as_str())
            .send()
    };
    assert_eq!(send().await.status(), 503);
    let r = send().await;
    assert_eq!(r.text(), "ok", "the retry runs again");
    assert_eq!(r.header("idempotent-replayed"), None);
    assert_eq!(send().await.header("idempotent-replayed"), Some("true"));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn client_errors_are_stored() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let c = App::new()
        .wrap(Idempotency::new())
        .post("/pay", move || {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(Error::bad_request("card declined"))
            }
        })
        .test();
    for _ in 0..2 {
        let r = c
            .post("/pay")
            .bearer("client")
            .header("idempotency-key", "d")
            .send()
            .await;
        assert_eq!(r.status(), 400);
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejections_that_may_change_are_not_stored() {
    let (c, runs) = app(Idempotency::new());
    // 404 (here: no such route) and friends are not stored, so the key
    // stays free and they can't fill the store.
    for _ in 0..2 {
        let r = c
            .post("/nowhere")
            .bearer("client")
            .header("idempotency-key", "n")
            .send()
            .await;
        assert_eq!(r.status(), 404);
        assert_eq!(r.header("idempotent-replayed"), None);
    }
    // The same key then works for a real request.
    let r = c
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", "n")
        .send()
        .await;
    assert_eq!(r.status(), 201);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cookies_reach_the_first_response_only() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let c = App::new()
        .wrap(Idempotency::new())
        .post("/charge", move || {
            r.fetch_add(1, Ordering::SeqCst);
            async {
                (
                    [(
                        HeaderName::from_static("set-cookie"),
                        HeaderValue::from_static("sid=rolled"),
                    )],
                    "charged",
                )
            }
        })
        .test();
    let send = || {
        c.post("/charge")
            .bearer("client")
            .header("idempotency-key", "c")
            .send()
    };
    assert_eq!(send().await.header("set-cookie"), Some("sid=rolled"));
    // Apps that refresh a session cookie on every response still get
    // replays, and a replay never hands out a session.
    let retry = send().await;
    assert_eq!(retry.header("idempotent-replayed"), Some("true"));
    assert_eq!(retry.text(), "charged");
    assert_eq!(retry.header("set-cookie"), None);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_client_with_only_running_requests_gets_429_not_others() {
    // max_keys 10: each scope may hold 1.
    let c = App::new()
        .wrap(Idempotency::new().max_keys(10))
        .post("/slow", || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "done"
        })
        .test();
    let send = |who: &'static str, key: &'static str| {
        c.post("/slow")
            .bearer(who)
            .header("idempotency-key", key)
            .send()
    };
    let later = |who, key| async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        send(who, key).await
    };
    let (first, same_client, other) =
        tokio::join!(send("a", "1"), later("a", "2"), later("b", "1"));
    assert_eq!(first.text(), "done");
    assert_eq!(same_client.status(), 429);
    assert!(same_client
        .text()
        .contains(r#""code":"idempotency_keys_exhausted""#));
    assert_eq!(other.text(), "done", "other clients are unaffected");
}

#[tokio::test]
async fn bodies_over_the_app_limit_are_left_to_their_route() {
    let c = App::new()
        .body_limit(8)
        .wrap(Idempotency::new())
        .post("/upload", |body: BodyStream| async move {
            let mut body = body.with_limit(1024);
            let mut n = 0;
            while let Some(chunk) = body.chunk().await? {
                n += chunk.len();
            }
            Ok::<_, Error>(format!("{n} bytes"))
        })
        .test();
    let r = c
        .post("/upload")
        .bearer("client")
        .header("idempotency-key", "big")
        .body("x".repeat(100))
        .send()
        .await;
    assert_eq!(r.text(), "100 bytes", "not 413");
}

#[tokio::test]
async fn stored_responses_expire() {
    let (c, runs) = app(Idempotency::new().ttl(Duration::from_millis(100)));
    let send = || {
        c.post("/charge")
            .bearer("client")
            .header("idempotency-key", "t")
            .send()
    };
    send().await;
    assert_eq!(send().await.header("idempotent-replayed"), Some("true"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(send().await.text(), "charge #2");
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_panicking_handler_releases_the_key() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let c = App::new()
        .wrap(CatchPanic::new())
        .wrap(Idempotency::new())
        .post("/boom", move || {
            let r = r.clone();
            async move {
                if r.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("first try fails");
                }
                "recovered"
            }
        })
        .test();
    let send = || {
        c.post("/boom")
            .bearer("client")
            .header("idempotency-key", "p")
            .send()
    };
    assert_eq!(send().await.status(), 500);
    let r = send().await;
    assert_eq!(r.status(), 200, "not 409: the key was released");
    assert_eq!(r.text(), "recovered");
}

#[tokio::test]
async fn a_full_store_drops_the_oldest_response() {
    let (c, runs) = app(Idempotency::new().max_keys(2));
    for key in ["a", "b", "c"] {
        c.post("/charge")
            .bearer("client")
            .header("idempotency-key", key)
            .send()
            .await;
    }
    // At the cap, a scope's oldest responses make room for its new ones:
    // "a" is gone, "c" is still stored.
    let c_retry = c
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", "c")
        .send()
        .await;
    assert_eq!(c_retry.header("idempotent-replayed"), Some("true"));
    let a_retry = c
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", "a")
        .send()
        .await;
    assert_eq!(a_retry.header("idempotent-replayed"), None);
    assert_eq!(runs.load(Ordering::SeqCst), 4);
}

#[cfg(feature = "redis")]
#[tokio::test]
async fn redis_shares_keys_between_instances() {
    use zapline::redis::Redis;
    let Ok(url) = std::env::var("ZAPLINE_TEST_REDIS_URL") else {
        eprintln!("skipped: ZAPLINE_TEST_REDIS_URL is not set");
        return;
    };
    let redis = Redis::new(&url).unwrap();
    let key = format!(
        "redis-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // Two "instances": separate apps sharing one Redis.
    let (one, runs_one) = app(Idempotency::new().redis(redis.clone()));
    let (two, runs_two) = app(Idempotency::new().redis(redis));
    let first = one
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", key.as_str())
        .body("x")
        .send()
        .await;
    assert_eq!(first.text(), "charge #1");
    let retry = two
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", key.as_str())
        .body("x")
        .send()
        .await;
    assert_eq!(retry.status(), 201);
    assert_eq!(retry.text(), "charge #1");
    assert_eq!(retry.header("idempotent-replayed"), Some("true"));
    let reused = two
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", key.as_str())
        .body("y")
        .send()
        .await;
    assert_eq!(reused.status(), 422);
    assert_eq!(runs_one.load(Ordering::SeqCst), 1);
    assert_eq!(runs_two.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "redis")]
#[tokio::test]
async fn unreachable_redis_fails_closed() {
    use zapline::redis::Redis;
    let (c, runs) = app(Idempotency::new().redis(Redis::new("redis://127.0.0.1:1").unwrap()));
    let r = c
        .post("/charge")
        .bearer("client")
        .header("idempotency-key", "u")
        .send()
        .await;
    assert_eq!(r.status(), 503);
    assert!(r.text().contains(r#""code":"idempotency_unavailable""#));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    // Requests without a key don't touch Redis.
    assert_eq!(c.post("/charge").send().await.status(), 201);
}
