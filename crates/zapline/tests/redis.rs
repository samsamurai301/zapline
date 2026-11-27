//! Redis: the client, and the cache, live updates, sessions and rate limits
//! shared between instances. Runs when `ZAPLINE_TEST_REDIS_URL` points at a
//! Redis server it may write to (keys under `zapline:`).
#![cfg(feature = "redis")]

use std::time::Duration;

use zapline::cache::Cache;
use zapline::live::Live;
use zapline::middleware::RateLimit;
use zapline::prelude::*;
use zapline::redis::{Redis, Reply};

fn redis() -> Option<Redis> {
    match std::env::var("ZAPLINE_TEST_REDIS_URL") {
        Ok(url) => Some(Redis::new(&url).unwrap()),
        Err(_) => {
            eprintln!("skipped: ZAPLINE_TEST_REDIS_URL is not set");
            None
        }
    }
}

/// A key prefix of its own for each test, so they can run in parallel.
fn unique(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{name}-{nanos}")
}

#[tokio::test]
async fn commands_and_replies() {
    let Some(redis) = redis() else { return };
    let key = unique("zapline:test:k");
    assert_eq!(
        redis.cmd(["SET", &key, "héllo"]).await.unwrap(),
        Reply::Status("OK".into())
    );
    assert_eq!(
        redis.cmd(["GET", &key]).await.unwrap().text(),
        Some("héllo")
    );
    assert_eq!(
        redis
            .cmd(["GET", &unique("zapline:test:none")])
            .await
            .unwrap(),
        Reply::Nil
    );
    let n = unique("zapline:test:n");
    assert_eq!(
        redis.cmd(["INCRBY", &n, "41"]).await.unwrap().int(),
        Some(41)
    );
    assert_eq!(redis.cmd(["INCR", &n]).await.unwrap(), Reply::Int(42));
    let err = redis.cmd(["INCR", &key]).await.unwrap_err();
    assert!(
        err.message().contains("not an integer"),
        "{}",
        err.message()
    );
    let list = redis.cmd(["MGET", &key, &n]).await.unwrap();
    assert_eq!(
        list,
        Reply::Array(vec![
            Reply::Bulk("héllo".as_bytes().to_vec()),
            Reply::Bulk(b"42".to_vec())
        ])
    );
    // Binary-safe both ways.
    redis
        .cmd([
            b"SET".as_slice(),
            key.as_bytes(),
            &[0, 159, 146, 150, b'\r', b'\n'],
        ])
        .await
        .unwrap();
    assert_eq!(
        redis.cmd(["GET", &key]).await.unwrap(),
        Reply::Bulk(vec![0, 159, 146, 150, 13, 10])
    );
    redis.cmd(["DEL", &key, &n]).await.unwrap();
}

#[tokio::test]
async fn reconnects_after_the_server_drops_idle_connections() {
    let Some(redis) = redis() else { return };
    // The one connection, now idle in the pool.
    let id = redis.cmd(["CLIENT", "ID"]).await.unwrap().int().unwrap();
    // Another client kills it.
    let other = Redis::new(&std::env::var("ZAPLINE_TEST_REDIS_URL").unwrap()).unwrap();
    other
        .cmd(["CLIENT", "KILL", "ID", &id.to_string()])
        .await
        .unwrap();
    assert_eq!(
        redis.cmd(["PING"]).await.unwrap(),
        Reply::Status("PONG".into())
    );
}

#[tokio::test]
async fn unreachable_redis_is_a_503() {
    let redis = Redis::new("redis://127.0.0.1:1").unwrap();
    let err = redis.cmd(["PING"]).await.unwrap_err();
    assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);

    // The cache degrades to misses instead of failing.
    let cache = Cache::redis(redis);
    cache.set("k", &1, Duration::from_secs(5)).await.unwrap();
    assert_eq!(cache.get::<i32>("k").await, None);
    let v: i32 = cache
        .get_or_set("k", Duration::from_secs(5), || async { Ok(7) })
        .await
        .unwrap();
    assert_eq!(v, 7);
}

#[tokio::test]
async fn the_cache_is_shared_and_expires() {
    let Some(redis) = redis() else { return };
    let (a, b) = (Cache::redis(redis.clone()), Cache::redis(redis.clone()));
    let p = unique("posts");
    a.set(&format!("{p}:1"), &"one", Duration::from_secs(30))
        .await
        .unwrap();
    a.set(&format!("{p}:2"), &"two", Duration::from_secs(30))
        .await
        .unwrap();
    a.set(&format!("{p}*:3"), &"glob", Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(
        b.get::<String>(&format!("{p}:1")).await.as_deref(),
        Some("one")
    );

    b.delete(&format!("{p}:1")).await;
    assert_eq!(a.get::<String>(&format!("{p}:1")).await, None);
    // The prefix is literal: `*` in it matches only itself.
    b.delete_prefix(&format!("{p}*")).await;
    assert_eq!(a.get::<String>(&format!("{p}*:3")).await, None);
    assert_eq!(
        a.get::<String>(&format!("{p}:2")).await.as_deref(),
        Some("two")
    );
    b.delete_prefix(&format!("{p}:")).await;
    assert_eq!(a.get::<String>(&format!("{p}:2")).await, None);

    a.set(&format!("{p}:short"), &1, Duration::from_millis(50))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(b.get::<i32>(&format!("{p}:short")).await, None);

    let mut computed = 0;
    for _ in 0..2 {
        let v: u32 = b
            .get_or_set(&format!("{p}:computed"), Duration::from_secs(30), || {
                computed += 1;
                async { Ok(5) }
            })
            .await
            .unwrap();
        assert_eq!(v, 5);
    }
    assert_eq!(computed, 1);
    b.delete_prefix(&p).await;
}

async fn next(sub: &mut zapline::live::Subscription) -> zapline::live::Update {
    tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("an update within 5 s")
        .expect("the hub is open")
}

#[tokio::test]
async fn live_updates_reach_other_instances() {
    let Some(redis) = redis() else { return };
    let topic = unique("orders");
    let (hub_a, hub_b) = (
        Live::new().redis(redis.clone()),
        Live::new().redis(redis.clone()),
    );
    let a = App::new().live(hub_a.clone()).test();
    let b = App::new().live(hub_b.clone()).test();
    a.start().await;
    b.start().await;

    let mut on_a = hub_a.subscribe(&topic);
    let mut on_b = hub_b.subscribe(&topic);
    // Give both listeners time to subscribe in Redis.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 0..3 {
        hub_a.publish(&topic, "created", i.to_string());
    }
    for i in 0..3 {
        // Each instance gets each update exactly once, in order.
        assert_eq!(next(&mut on_a).await.data, i.to_string());
        let remote = next(&mut on_b).await;
        assert_eq!(
            (remote.topic.as_str(), remote.event.as_str()),
            (topic.as_str(), "created")
        );
        assert_eq!(remote.data, i.to_string());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(200), on_a.next())
            .await
            .is_err()
    );
    a.shutdown().await;
    b.shutdown().await;
}

#[cfg(feature = "cookies")]
#[tokio::test]
async fn sessions_live_in_redis() {
    use zapline::redis::RedisStore;
    use zapline::session::{SessionRecord, SessionStore};

    let Some(redis) = redis() else { return };
    let store = RedisStore::new(redis);
    let id = unique("sess");
    assert!(store.load(&id).await.unwrap().is_none());
    let mut data = serde_json::Map::new();
    data.insert("user_id".into(), 7.into());
    let record = SessionRecord {
        data,
        expires_at: std::time::SystemTime::now() + Duration::from_secs(60),
    };
    store.save(&id, &record).await.unwrap();
    let loaded = store.load(&id).await.unwrap().unwrap();
    assert_eq!(loaded.data["user_id"], 7);
    store.delete(&id).await.unwrap();
    assert!(store.load(&id).await.unwrap().is_none());

    // Short sessions expire in Redis too.
    let short = SessionRecord {
        data: serde_json::Map::new(),
        expires_at: std::time::SystemTime::now() + Duration::from_millis(1100),
    };
    store.save(&id, &short).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(store.load(&id).await.unwrap().is_none());
}

#[get("/")]
async fn index() -> &'static str {
    "ok"
}

#[tokio::test]
async fn rate_limits_hold_across_instances() {
    let Some(redis) = redis() else { return };
    let header = unique("client");
    let limit =
        || RateLimit::by_header("x-client", 3, Duration::from_secs(60)).redis(redis.clone());
    let a = App::new().wrap(limit()).mount(index).test();
    let b = App::new().wrap(limit()).mount(index).test();
    let send = |client: &zapline::test::TestClient| {
        client.get("/").header("x-client", header.as_str()).send()
    };
    let first = send(&a).await;
    first.assert_status(200);
    assert_eq!(first.header("ratelimit-remaining"), Some("2"));
    send(&b).await.assert_status(200);
    send(&a).await.assert_status(200);
    let res = send(&b).await;
    res.assert_status(429);
    assert!(res
        .header("retry-after")
        .is_some_and(|s| s.parse::<u64>().unwrap() >= 1));
    // Another client has its own bucket.
    a.get("/")
        .header("x-client", unique("someone-else").as_str())
        .send()
        .await
        .assert_status(200);

    // Redis down: requests go through.
    let open = App::new()
        .wrap(
            RateLimit::global(1, Duration::from_secs(60))
                .redis(Redis::new("redis://127.0.0.1:1").unwrap()),
        )
        .mount(index)
        .test();
    for _ in 0..3 {
        open.get("/").send().await.assert_status(200);
    }
}
