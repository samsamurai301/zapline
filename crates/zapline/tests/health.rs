//! Health checks: liveness, readiness, combined report, check semantics
//! (critical, timeouts, panics, caching) and the readiness toggle.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zapline::health::{Health, HealthCheck, HealthState};
use zapline::prelude::*;
use zapline::test::TestClient;

async fn get(c: &TestClient, path: &str) -> (StatusCode, Value) {
    let r = c.get(path).send().await;
    assert_eq!(r.header("content-type"), Some("application/json"), "{path}");
    assert_eq!(r.header("cache-control"), Some("no-store"), "{path}");
    (r.status(), r.json())
}

/// Drop the timing fields so reports compare exactly.
fn without_timings(mut v: Value) -> Value {
    if let Some(checks) = v.get_mut("checks").and_then(Value::as_object_mut) {
        for c in checks.values_mut() {
            let ms = c.as_object_mut().unwrap().remove("duration_ms");
            assert!(ms.and_then(|m| m.as_f64()).is_some_and(|m| m >= 0.0));
        }
    }
    if let Some(obj) = v.as_object_mut() {
        if let Some(up) = obj.remove("uptime_seconds") {
            assert!(up.as_f64().is_some_and(|u| u >= 0.0));
        }
    }
    v
}

#[tokio::test]
async fn defaults_without_checks() {
    let c = App::new().plugin(Health::new()).test();
    assert_eq!(
        get(&c, "/health/live").await,
        (StatusCode::OK, json!({"status": "up"}))
    );
    let (s, body) = get(&c, "/health/ready").await;
    assert_eq!(s, 200);
    assert_eq!(body, json!({"status": "up", "checks": {}}));
    let (s, body) = get(&c, "/health").await;
    assert_eq!(s, 200);
    assert!(body["uptime_seconds"].as_f64().is_some());
    assert_eq!(without_timings(body), json!({"status": "up", "checks": {}}));
    // HEAD works for probes that use it.
    let r = c.request(Method::HEAD, "/health/live").send().await;
    assert_eq!(r.status(), 200);
}

#[derive(Clone)]
struct Db {
    up: Arc<std::sync::atomic::AtomicBool>,
}

#[tokio::test]
async fn checks_report_up_down_and_errors() {
    let db = Db {
        up: Arc::new(true.into()),
    };
    let flag = db.up.clone();
    let c = App::new()
        .state(db)
        .plugin(
            Health::new()
                .info("orders", "1.2.3")
                .check("db", |res: Resources| async move {
                    let db = res.require::<Db>()?;
                    if db.up.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err(Error::service_unavailable("connection refused"))
                    }
                })
                .check("disk", |_res: Resources| async move { Ok(()) }),
        )
        .test();

    let (s, body) = get(&c, "/health/ready").await;
    assert_eq!(s, 200);
    assert_eq!(
        without_timings(body),
        json!({
            "status": "up",
            "checks": {"db": {"status": "up"}, "disk": {"status": "up"}},
            "info": {"name": "orders", "version": "1.2.3"}
        })
    );

    flag.store(false, Ordering::SeqCst);
    let (s, body) = get(&c, "/health/ready").await;
    assert_eq!(s, 503);
    assert_eq!(
        without_timings(body),
        json!({
            "status": "down",
            "checks": {
                "db": {"status": "down", "error": "connection refused"},
                "disk": {"status": "up"}
            },
            "info": {"name": "orders", "version": "1.2.3"}
        })
    );
    let (s, body) = get(&c, "/health").await;
    assert_eq!(s, 503);
    assert_eq!(body["status"], "down");
    assert_eq!(body["checks"]["db"]["status"], "down");
    assert_eq!(body["info"]["name"], "orders");
    // Liveness never runs checks.
    assert_eq!(get(&c, "/health/live").await.0, 200);
}

#[tokio::test]
async fn question_mark_errors_include_their_source() {
    let c = App::new()
        .plugin(Health::new().check("file", |_res: Resources| async move {
            std::fs::read("/definitely/not/here/zapline")?;
            Ok(())
        }))
        .test();
    let (s, body) = get(&c, "/health/ready").await;
    assert_eq!(s, 503);
    let err = body["checks"]["file"]["error"].as_str().unwrap();
    assert!(err.starts_with("internal server error: "), "{err}");
    assert!(err.to_lowercase().contains("no such file"), "{err}");
}

#[tokio::test]
async fn non_critical_failures_degrade() {
    let c = App::new()
        .plugin(
            Health::new()
                .check("db", |_res: Resources| async move { Ok(()) })
                .check("cache", |_res: Resources| async move {
                    Err(Error::internal("cache miss storm"))
                })
                .critical(false),
        )
        .test();
    let (s, body) = get(&c, "/health/ready").await;
    assert_eq!(s, 200);
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["checks"]["cache"]["status"], "down");
    assert_eq!(body["checks"]["cache"]["error"], "cache miss storm");
    assert_eq!(body["checks"]["db"]["status"], "up");
}

#[tokio::test]
async fn checks_run_concurrently_with_timeouts() {
    let slow = |ms: u64| {
        move |_res: Resources| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(())
        }
    };
    let c = App::new()
        .plugin(
            Health::new()
                .timeout(Duration::from_millis(300))
                .check("a", slow(150))
                .check("b", slow(150))
                .check("c", slow(150))
                .check("hang", slow(60_000)),
        )
        .test();
    c.start().await;
    let t = Instant::now();
    let (s, body) = get(&c, "/health/ready").await;
    let elapsed = t.elapsed();
    assert_eq!(s, 503);
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
    assert_eq!(body["checks"]["a"]["status"], "up");
    assert_eq!(body["checks"]["hang"]["status"], "down");
    assert_eq!(body["checks"]["hang"]["error"], "timed out after 300ms");
    let ms = body["checks"]["hang"]["duration_ms"].as_f64().unwrap();
    assert!((290.0..1500.0).contains(&ms), "{ms}");
}

#[tokio::test]
async fn panicking_check_is_down_not_a_crash() {
    let c = App::new()
        .plugin(
            Health::new()
                .check("buggy", |_res: Resources| async move {
                    if true {
                        panic!("oops");
                    }
                    Ok(())
                })
                .check("fine", |_res: Resources| async move { Ok(()) }),
        )
        .test();
    let (s, body) = get(&c, "/health/ready").await;
    assert_eq!(s, 503);
    assert_eq!(body["checks"]["buggy"]["error"], "check panicked");
    assert_eq!(body["checks"]["fine"]["status"], "up");
    // The app keeps serving.
    assert_eq!(get(&c, "/health/live").await.0, 200);
}

struct Threshold {
    calls: Arc<AtomicUsize>,
    limit: usize,
}

impl HealthCheck for Threshold {
    async fn check(&self, res: Resources) -> Result<()> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let label = res.get::<&'static str>().unwrap_or("none");
        if n > self.limit {
            return Err(Error::service_unavailable(format!("{label}: over limit")));
        }
        Ok(())
    }
}

#[tokio::test]
async fn struct_checks_and_caching() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = App::new()
        .state("queue")
        .plugin(
            Health::new()
                .check(
                    "queue",
                    Threshold {
                        calls: calls.clone(),
                        limit: 1,
                    },
                )
                .cache_for(Duration::from_millis(200)),
        )
        .test();
    assert_eq!(get(&c, "/health/ready").await.0, 200);
    assert_eq!(get(&c, "/health/ready").await.0, 200); // cached
    assert_eq!(get(&c, "/health").await.0, 200); // shares the cache
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let (s, body) = get(&c, "/health/ready").await;
    assert_eq!(s, 503);
    assert_eq!(body["checks"]["queue"]["error"], "queue: over limit");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn readiness_toggle_via_state_and_resources() {
    let c = App::new()
        .plugin(Health::new().info("svc", "0.1.0"))
        .post("/drain", |State(h): State<HealthState>| async move {
            h.set_ready(false);
            "draining"
        })
        .post("/resume", |State(h): State<HealthState>| async move {
            h.set_ready(true);
            "ready"
        })
        .test();
    assert_eq!(get(&c, "/health/ready").await.0, 200);
    c.post("/drain").send().await;
    let (s, body) = get(&c, "/health/ready").await;
    assert_eq!(s, 503);
    assert_eq!(
        body,
        json!({"status": "down", "reason": "not ready", "info": {"name": "svc", "version": "0.1.0"}})
    );
    assert_eq!(get(&c, "/health").await.0, 503);
    assert_eq!(get(&c, "/health/live").await.0, 200);
    c.post("/resume").send().await;
    assert_eq!(get(&c, "/health/ready").await.0, 200);

    // Hooks reach it through resources: not ready until warm-up is done.
    let warm = Arc::new(tokio::sync::Notify::new());
    let w = warm.clone();
    let c = App::new()
        .plugin(Health::new())
        .on_startup(move |res: Resources| async move {
            let health = res.require::<HealthState>()?;
            health.set_ready(false);
            tokio::spawn(async move {
                w.notified().await;
                health.set_ready(true);
            });
            Ok(())
        })
        .test();
    assert_eq!(get(&c, "/health/ready").await.0, 503);
    warm.notify_one();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(get(&c, "/health/ready").await.0, 200);
    assert!(c
        .resources()
        .await
        .require::<HealthState>()
        .unwrap()
        .is_ready());
}

#[tokio::test]
async fn shared_state_created_up_front() {
    let state = HealthState::new();
    let c = App::new().plugin(Health::new().state(state.clone())).test();
    state.set_ready(false);
    assert_eq!(get(&c, "/health/ready").await.0, 503);
    state.set_ready(true);
    assert_eq!(get(&c, "/health/ready").await.0, 200);
}

#[tokio::test]
async fn custom_and_disabled_paths_and_hidden_details() {
    let c = App::new()
        .plugin(
            Health::new()
                .live_path("/livez")
                .ready_path("/readyz")
                .path("")
                .details(false)
                .info("svc", "2.0")
                .check("db", |_res: Resources| async move {
                    Err(Error::internal("secret host db-7:5432"))
                }),
        )
        .test();
    assert_eq!(
        get(&c, "/livez").await,
        (StatusCode::OK, json!({"status": "up"}))
    );
    let (s, body) = get(&c, "/readyz").await;
    assert_eq!(s, 503);
    assert_eq!(
        body,
        json!({"status": "down", "info": {"name": "svc", "version": "2.0"}})
    );
    assert!(!body.to_string().contains("secret"));
    for gone in ["/health", "/health/live", "/health/ready"] {
        assert_eq!(c.get(gone).send().await.status(), 404, "{gone}");
    }
}

#[tokio::test]
async fn json_escaping_of_names_errors_and_info() {
    let c = App::new()
        .plugin(
            Health::new()
                .info("svc \"quoted\"", "1.0\n")
                .check("we\"ird\\name", |_res: Resources| async move {
                    Err(Error::internal("line1\nline2 \"q\""))
                }),
        )
        .test();
    let (s, body) = get(&c, "/health/ready").await;
    assert_eq!(s, 503);
    assert_eq!(
        body["checks"]["we\"ird\\name"]["error"],
        "line1\nline2 \"q\""
    );
    assert_eq!(body["info"]["name"], "svc \"quoted\"");
    assert_eq!(body["info"]["version"], "1.0\n");
}

#[test]
#[should_panic(expected = "duplicate health check `db`")]
fn duplicate_check_names_panic() {
    let _ = Health::new()
        .check("db", |_res: Resources| async move { Ok(()) })
        .check("db", |_res: Resources| async move { Ok(()) });
}

#[test]
#[should_panic(expected = "`critical` must be called after `check`")]
fn critical_without_check_panics() {
    let _ = Health::new().critical(false);
}

#[tokio::test]
async fn probes_over_a_real_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = HealthState::new();
    tokio::spawn(
        App::new()
            .plugin(
                Health::new()
                    .state(state.clone())
                    .check("db", |_res: Resources| async move { Ok(()) }),
            )
            .serve(listener),
    );
    async fn probe(addr: std::net::SocketAddr, path: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }
    let res = probe(addr, "/health/ready").await;
    assert!(res.starts_with("HTTP/1.1 200"), "{res}");
    assert!(res.contains(r#""db":{"status":"up""#), "{res}");
    state.set_ready(false);
    let res = probe(addr, "/health/ready").await;
    assert!(res.starts_with("HTTP/1.1 503"), "{res}");
    let res = probe(addr, "/health/live").await;
    assert!(res.starts_with("HTTP/1.1 200"), "{res}");
}
