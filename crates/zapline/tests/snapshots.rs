//! Snapshot tests from recorded traffic: recording in the dev console,
//! redaction, and replay (pass, fail, update).
#![cfg(all(debug_assertions, feature = "json"))]

use std::net::SocketAddr;
use std::path::Path as FsPath;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use zapline::prelude::*;
use zapline::sse::Event;
use zapline::test::TestClient;

/// A small API with the volatile values real apps have: ids handed out by
/// the store, UUIDs, timestamps and tokens.
#[derive(Clone)]
struct Store {
    next: Arc<AtomicU64>,
    items: Arc<Mutex<Vec<(u64, String)>>>,
    /// Changes what `GET /items/{id}` says, to simulate a regression.
    shout: bool,
}

fn api(first_id: u64, shout: bool) -> App {
    let store = Store {
        next: Arc::new(AtomicU64::new(first_id)),
        items: Arc::default(),
        shout,
    };
    App::new()
        .dev_console()
        .state(store)
        .post(
            "/items",
            |State(s): State<Store>, Json(body): Json<Value>| async move {
                let id = s.next.fetch_add(1, Ordering::SeqCst);
                let name = body["name"].as_str().unwrap_or("").to_owned();
                s.items.lock().unwrap().push((id, name.clone()));
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": id,
                        "name": name,
                        "uuid": format!("67e55044-10b1-426f-9247-{:012x}", fastrand::u64(..) >> 16),
                        "created_at": format!("2024-05-01T12:{:02}:00Z", fastrand::u8(0..60)),
                        "token": format!("tok{:016x}A{:016x}", fastrand::u64(..), fastrand::u64(..)),
                    })),
                )
            },
        )
        .get(
            "/items/{id}",
            |State(s): State<Store>, Path(id): Path<u64>| async move {
                let items = s.items.lock().unwrap();
                let Some((id, name)) = items.iter().find(|(i, _)| *i == id).cloned() else {
                    return Err(Error::not_found());
                };
                let name = if s.shout { name.to_uppercase() } else { name };
                Ok(Json(json!({ "id": id, "name": name })))
            },
        )
        .get("/hello", |q: Query<Value>| async move {
            format!("hello {}", q.0["who"].as_str().unwrap_or("nobody"))
        })
}

async fn recordings(client: &TestClient) -> Value {
    let res = client.get("/_dev/recordings").send().await;
    res.assert_status(200);
    res.json()
}

async fn start(client: &TestClient) {
    client
        .post("/_dev/recordings/start")
        .send()
        .await
        .assert_status(303)
        .assert_header("location", "/_dev");
}

/// Record a create-then-fetch session and return it as a snapshot file.
async fn record_session(first_id: u64) -> Value {
    let client = api(first_id, false).test();
    start(&client).await;
    let res = client
        .post("/items")
        .json(&json!({"name": "Ada"}))
        .send()
        .await;
    res.assert_status(201);
    let id = res.json::<Value>()["id"].as_u64().unwrap();
    client
        .get(&format!("/items/{id}"))
        .send()
        .await
        .assert_status(200);
    client
        .get("/hello?who=you")
        .send()
        .await
        .assert_text("hello you");
    recordings(&client).await
}

fn write(dir: &FsPath, name: &str, snapshot: &Value) {
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_string_pretty(snapshot).unwrap(),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Recording

#[tokio::test]
async fn records_requests_and_responses_with_secrets_redacted() {
    let client = api(1, false).test();
    start(&client).await;
    client
        .post("/items?draft=1")
        .bearer("super-secret-token")
        .header("cookie", "sid=abc123")
        .header("x-api-key", "k-123")
        .header("user-agent", "curl/8")
        .header("hx-request", "true")
        .json(&json!({"name": "Ada"}))
        .send()
        .await
        .assert_status(201);
    client.get("/missing").send().await.assert_status(404);
    // The console's own requests are not recorded.
    client.get("/_dev").send().await.assert_status(200);

    let rec = recordings(&client).await;
    assert_eq!(rec["zapline_snapshot"], 1);
    let ex = rec["exchanges"].as_array().unwrap();
    assert_eq!(ex.len(), 2, "{rec:#}");

    let req = &ex[0]["request"];
    assert_eq!(req["method"], "POST");
    assert_eq!(req["path"], "/items");
    assert_eq!(req["query"], "draft=1");
    assert_eq!(req["json"], json!({"name": "Ada"}));
    let headers = &req["headers"];
    assert_eq!(headers["authorization"], "[redacted]");
    assert_eq!(headers["cookie"], "[redacted]");
    assert_eq!(headers["x-api-key"], "[redacted]");
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["hx-request"], "true");
    assert!(headers.get("user-agent").is_none(), "noise is dropped");
    let text = rec.to_string();
    for secret in ["super-secret-token", "abc123", "k-123"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }

    let res = &ex[0]["response"];
    assert_eq!(res["status"], 201);
    assert_eq!(res["content_type"], "application/json");
    assert_eq!(res["json"]["name"], "Ada");
    assert_eq!(ex[1]["response"]["status"], 404);
}

#[tokio::test]
async fn records_nothing_until_turned_on_and_stops_when_told() {
    let client = api(1, false).test();
    client.get("/hello").send().await.assert_status(200);
    assert_eq!(recordings(&client).await["exchanges"], json!([]));

    start(&client).await;
    client.get("/hello?who=a").send().await;
    client
        .post("/_dev/recordings/stop")
        .send()
        .await
        .assert_status(303);
    client.get("/hello?who=b").send().await;
    let rec = recordings(&client).await;
    assert_eq!(rec["exchanges"].as_array().unwrap().len(), 1);
    assert_eq!(rec["exchanges"][0]["request"]["query"], "who=a");

    client
        .post("/_dev/recordings/clear")
        .send()
        .await
        .assert_status(303);
    assert_eq!(recordings(&client).await["exchanges"], json!([]));
}

#[tokio::test]
async fn streamed_bodies_and_assets_are_handled() {
    let client = App::new()
        .dev_console()
        .get("/events", || async {
            Sse::new(futures_util::stream::iter([Ok::<
                _,
                std::convert::Infallible,
            >(
                Event::default().data("hi")
            )]))
        })
        .get("/app.css", || async {
            (
                [(header::CONTENT_TYPE, HeaderValue::from_static("text/css"))],
                "body{}",
            )
        })
        .post("/echo", |b: Bytes| async move { b })
        .test();
    start(&client).await;
    let res = client.get("/events").send().await;
    assert!(res.text().contains("data: hi"), "the stream still works");
    client.get("/app.css").send().await.assert_text("body{}");
    // Over the 64 KiB cap: handled, but not recorded.
    let big = vec![b'x'; 70 * 1024];
    let res = client.post("/echo").body(big.clone()).send().await;
    assert_eq!(res.bytes().len(), big.len());
    client
        .post("/echo")
        .body(vec![0xff, 0x00, 0x01])
        .send()
        .await;

    let rec = recordings(&client).await;
    let ex = rec["exchanges"].as_array().unwrap();
    assert_eq!(ex.len(), 2, "styles are not recorded: {rec:#}");
    assert_eq!(ex[0]["response"]["body_not_recorded"], "streamed");
    assert_eq!(ex[1]["request"]["base64"], "/wAB");
    assert_eq!(ex[1]["response"]["base64"], "/wAB");

    let page = client.get("/_dev").send().await;
    page.assert_contains("<h2>Recording</h2>")
        .assert_contains("<strong>Recording</strong> · 2 request(s) recorded · 1 skipped")
        .assert_contains(r#"<form method=post action="/_dev/recordings/stop""#)
        .assert_contains(r#"action="/_dev/recordings/save""#);
}

#[tokio::test]
async fn console_offers_to_start_recording() {
    let client = api(1, false).test();
    client
        .get("/_dev")
        .send()
        .await
        .assert_contains("Not recording · 0 request(s) recorded.")
        .assert_contains(r#"<form method=post action="/_dev/recordings/start""#)
        .assert_contains("Authorization and cookie values are never recorded");
}

#[tokio::test]
async fn recording_controls_are_local_and_same_origin_only() {
    let client = api(1, false).test();
    let remote = SocketAddr::from(([203, 0, 113, 7], 4000));
    client
        .get("/_dev/recordings")
        .remote_addr(remote)
        .send()
        .await
        .assert_status(404);
    client
        .post("/_dev/recordings/start")
        .remote_addr(remote)
        .send()
        .await
        .assert_status(404);
    // A page on another site posting a form to localhost.
    client
        .post("/_dev/recordings/start")
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .assert_status(404);
    client
        .post("/_dev/recordings/start")
        .header("origin", "https://evil.example")
        .header("host", "localhost:8080")
        .send()
        .await
        .assert_status(404);
    client
        .post("/_dev/recordings/bogus")
        .send()
        .await
        .assert_status(404);
    client.get("/hello").send().await;
    assert_eq!(recordings(&client).await["exchanges"], json!([]));
}

#[tokio::test]
async fn saving_needs_a_valid_name_and_a_recording() {
    let client = api(1, false).test();
    let res = client
        .post("/_dev/recordings/save")
        .header("accept", "application/json")
        .form(&[("name", "../../etc/passwd")])
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json::<Value>()["code"], "invalid_name");
    let res = client
        .post("/_dev/recordings/save")
        .header("accept", "application/json")
        .form(&[("name", "empty")])
        .send()
        .await;
    res.assert_status(409);
    assert_eq!(res.json::<Value>()["code"], "nothing_recorded");
}

#[tokio::test]
async fn not_recording_without_the_console() {
    let client = App::new().get("/", || async { "hi" }).test();
    client.get("/").send().await;
    client
        .get("/_dev/recordings")
        .send()
        .await
        .assert_status(404);
}

// ---------------------------------------------------------------------------
// Replay

#[tokio::test]
async fn replay_passes_despite_volatile_values_and_new_ids() {
    let dir = tempfile::tempdir().unwrap();
    // Recorded against a store that already had 99 items...
    write(dir.path(), "items", &record_session(100).await);
    let recorded = std::fs::read_to_string(dir.path().join("items.json")).unwrap();
    assert!(recorded.contains("\"path\": \"/items/100\""), "{recorded}");

    // ...and replayed against an empty one: `POST /items` now answers id 1,
    // so the recorded `GET /items/100` is sent as `GET /items/1`.
    let report = api(1, false)
        .test()
        .replay_snapshots(dir.path())
        .report()
        .await;
    assert!(report.passed(), "{report}");
    assert_eq!((report.files, report.exchanges, report.updated), (1, 3, 0));
    assert!(report.to_string().contains("all match"));

    // Awaiting directly passes too.
    api(1, false).test().replay_snapshots(dir.path()).await;
    // A single file works as well.
    api(1, false)
        .test()
        .replay_snapshots(dir.path().join("items.json"))
        .await;
}

#[tokio::test]
async fn replay_fails_with_a_readable_diff() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "items", &record_session(1).await);

    let report = api(1, true)
        .test()
        .replay_snapshots(dir.path())
        .report()
        .await;
    assert!(!report.passed());
    assert_eq!(report.failures.len(), 1, "{report}");
    let failure = &report.failures[0];
    assert!(failure.contains("items.json #2 GET /items/1"), "{failure}");
    assert!(
        failure.contains("body (- recorded, + actual):"),
        "{failure}"
    );
    assert!(failure.contains("    -   \"name\": \"Ada\""), "{failure}");
    assert!(failure.contains("    +   \"name\": \"ADA\""), "{failure}");
    assert!(
        failure.contains("      \"id\": 1,"),
        "unchanged context: {failure}"
    );
    let text = report.to_string();
    assert!(
        text.contains("1 of 3 recorded response(s) changed"),
        "{text}"
    );
    assert!(
        text.contains("ZAPLINE_UPDATE_SNAPSHOTS=1 cargo test"),
        "{text}"
    );

    // Awaiting the replay panics with the same report.
    let client = api(1, true).test();
    let path = dir.path().to_path_buf();
    let err = tokio::spawn(async move { client.replay_snapshots(path).await })
        .await
        .unwrap_err();
    let panic = err.into_panic();
    let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(message.contains("\"name\": \"ADA\""), "{message}");
}

#[tokio::test]
async fn replay_reports_status_changes() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "gone",
        &json!({"zapline_snapshot": 1, "exchanges": [{
            "request": {"method": "GET", "path": "/items/42"},
            "response": {"status": 200, "content_type": "application/json", "json": {"id": 42}}
        }]}),
    );
    let report = api(1, false)
        .test()
        .replay_snapshots(dir.path())
        .report()
        .await;
    assert!(
        report.failures[0].contains("status: expected 200, got 404"),
        "{report}"
    );
}

#[tokio::test]
async fn update_mode_rewrites_changed_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "items", &record_session(1).await);

    let report = api(1, true)
        .test()
        .replay_snapshots(dir.path())
        .update(true)
        .report()
        .await;
    assert!(report.passed(), "{report}");
    assert_eq!(report.updated, 1);
    assert!(report.to_string().contains("(1 updated)"));
    let file = std::fs::read_to_string(dir.path().join("items.json")).unwrap();
    assert!(file.contains("\"ADA\""), "{file}");

    // The new behavior is now the recorded one.
    api(1, true).test().replay_snapshots(dir.path()).await;
    let report = api(1, false)
        .test()
        .replay_snapshots(dir.path())
        .report()
        .await;
    assert!(!report.passed());
}

#[tokio::test]
async fn update_mode_fills_in_bodies_that_were_not_recorded() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "hello",
        &json!({"zapline_snapshot": 1, "exchanges": [{
            "request": {"method": "GET", "path": "/hello", "query": "who=x"},
            "response": {"status": 200, "body_not_recorded": "compressed"}
        }]}),
    );
    // Only the status is compared.
    api(1, false).test().replay_snapshots(dir.path()).await;
    let report = api(1, false)
        .test()
        .replay_snapshots(dir.path())
        .update(true)
        .report()
        .await;
    assert_eq!(report.updated, 1);
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("hello.json")).unwrap())
            .unwrap();
    assert_eq!(file["exchanges"][0]["response"]["text"], "hello x");
    assert!(file["exchanges"][0]["response"]
        .get("body_not_recorded")
        .is_none());
}

#[tokio::test]
async fn redacted_credentials_come_from_the_replay() {
    let app = || {
        App::new()
            .dev_console()
            .get("/me", |headers: HeaderMap| async move {
                match headers.get(header::AUTHORIZATION) {
                    Some(v) if v == "Bearer t" => Ok("ada"),
                    _ => Err(Error::unauthorized()),
                }
            })
    };
    let client = app().test();
    start(&client).await;
    client
        .get("/me")
        .bearer("t")
        .send()
        .await
        .assert_text("ada");
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "me", &recordings(&client).await);

    let report = app().test().replay_snapshots(dir.path()).report().await;
    assert!(
        report.failures[0].contains("status: expected 200, got 401"),
        "the recorded token is not sent: {report}"
    );
    app()
        .test()
        .replay_snapshots(dir.path())
        .header("authorization", "Bearer t")
        .await;
}

#[tokio::test]
async fn each_file_replays_with_its_own_cookie_jar() {
    let app = || {
        App::new()
            .dev_console()
            .post("/login", || async {
                (
                    [(
                        header::SET_COOKIE,
                        HeaderValue::from_static("user=ada; Path=/"),
                    )],
                    "welcome",
                )
            })
            .get("/me", |headers: HeaderMap| async move {
                match headers.get(header::COOKIE) {
                    Some(c) if c == "user=ada" => "ada",
                    _ => "anonymous",
                }
            })
    };
    let client = app().test().with_cookies();
    start(&client).await;
    client.post("/login").send().await;
    client.get("/me").send().await.assert_text("ada");
    let session = recordings(&client).await;
    assert_eq!(
        session["exchanges"][1]["request"]["headers"]["cookie"],
        "[redacted]"
    );

    let anonymous = app().test();
    start(&anonymous).await;
    anonymous.get("/me").send().await.assert_text("anonymous");

    let dir = tempfile::tempdir().unwrap();
    // `a-login` logs in; `b-visitor` replays after it and must not be.
    write(dir.path(), "a-login", &session);
    write(dir.path(), "b-visitor", &recordings(&anonymous).await);
    let report = app().test().replay_snapshots(dir.path()).report().await;
    assert!(report.passed(), "{report}");
    assert_eq!(report.files, 2);
}

#[tokio::test]
async fn unreadable_snapshots_are_reported() {
    let missing = api(1, false)
        .test()
        .replay_snapshots("/definitely/not/here")
        .report()
        .await;
    assert!(
        missing.failures[0].contains("cannot read the snapshot directory"),
        "{missing}"
    );

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("broken.json"), "{not json").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
    write(
        dir.path(),
        "future",
        &json!({"zapline_snapshot": 99, "exchanges": []}),
    );
    let report = api(1, false)
        .test()
        .replay_snapshots(dir.path())
        .report()
        .await;
    assert_eq!(report.files, 2, "{report}");
    assert!(
        report.failures[0].contains("broken.json: cannot read"),
        "{report}"
    );
    assert!(
        report.failures[1].contains("unsupported snapshot version 99"),
        "{report}"
    );
}
