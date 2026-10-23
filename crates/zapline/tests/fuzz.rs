//! OpenAPI-driven fuzzing: `client.fuzz()` must find deliberate bugs (a
//! panic on a boundary value, a 5xx, an undeclared status, a response that
//! breaks its schema, a hang), shrink them to small requests, stay quiet on
//! a correct app, and be deterministic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zapline::openapi::OpenApi;
use zapline::prelude::*;
use zapline::test::{FailureKind, FuzzConfig, FuzzReport};

// ---------------------------------------------------------------------------
// An app with bugs

/// Panics on `n = 0`: a boundary value of `u32`.
#[get("/ratio/{n}")]
async fn ratio(Path(n): Path<u32>) -> String {
    (100 / n).to_string()
}

#[derive(Deserialize, JsonSchema)]
struct Label {
    text: String,
    /// Optional, so the shrinker can drop it.
    color: Option<String>,
}

/// Panics on an empty `text`.
#[post("/labels")]
async fn label(Json(label): Json<Label>) -> String {
    let first = label.text.chars().next().unwrap();
    format!("{first}{}", label.color.unwrap_or_default())
}

#[derive(Deserialize, JsonSchema)]
struct Search {
    q: String,
}

/// A 500 for long queries.
#[get("/search")]
async fn search(Query(s): Query<Search>) -> Result<String> {
    if s.q.chars().count() > 200 {
        return Err(Error::internal("query too long for the index"));
    }
    Ok(s.q)
}

/// Answers 418, which its signature does not declare.
#[get("/teapot")]
async fn teapot() -> (StatusCode, &'static str) {
    (StatusCode::IM_A_TEAPOT, "short and stout")
}

#[derive(Serialize, JsonSchema)]
struct Stats {
    count: u32,
}

/// Never answers for the largest `u8`.
#[get("/slow/{n}")]
async fn slow(Path(n): Path<u8>) -> String {
    if n == u8::MAX {
        std::future::pending::<()>().await;
    }
    n.to_string()
}

fn buggy() -> App {
    App::new()
        .mount((ratio, label, search, teapot, slow))
        // Declared as `Stats`, but `count` comes back as a string.
        .get("/stats", || async { Json(json!({ "count": "many" })) })
        .response::<Stats>(200, "Usage statistics")
        .openapi(OpenApi::new("buggy", "1"))
}

async fn fuzz_buggy(config: FuzzConfig) -> FuzzReport {
    buggy()
        .test()
        .fuzz()
        .config(config.timeout(Duration::from_millis(200)))
        .await
}

fn failure<'r>(report: &'r FuzzReport, operation: &str) -> &'r zapline::test::FuzzFailure {
    report
        .failures()
        .iter()
        .find(|f| f.operation() == operation)
        .unwrap_or_else(|| panic!("no failure for {operation}:\n{report}"))
}

#[tokio::test]
async fn finds_a_panic_on_a_boundary_value() {
    let report = fuzz_buggy(FuzzConfig::new().only("GET /ratio/{n}")).await;
    assert_eq!(report.failures().len(), 1, "{report}");
    let f = failure(&report, "GET /ratio/{n}");
    let FailureKind::Panic(msg) = f.kind() else {
        panic!("expected a panic: {report}");
    };
    assert!(msg.contains("divide by zero"), "{msg}");
    assert!(
        msg.contains("tests/fuzz.rs"),
        "the location is reported: {msg}"
    );
    assert_eq!(f.request().uri(), "/ratio/0");
    assert_eq!(f.status(), None);
    assert_eq!(f.operation_id(), "ratio");
}

#[tokio::test]
async fn shrinks_a_failing_body_to_a_minimal_request() {
    let report = fuzz_buggy(FuzzConfig::new().only("/labels")).await;
    let f = failure(&report, "POST /labels");
    assert!(matches!(f.kind(), FailureKind::Panic(_)), "{report}");
    // `color` is optional and irrelevant: shrinking removes it.
    let body: Value = serde_json::from_slice(f.request().body()).unwrap();
    assert_eq!(body, json!({ "text": "" }), "{report}");
    assert_eq!(
        f.request().to_rust(),
        r##"client.request(Method::POST, "/labels").header("content-type", "application/json").body(r#"{"text":""}"#).send().await"##
    );
}

#[tokio::test]
async fn finds_server_errors_undeclared_statuses_bad_bodies_and_hangs() {
    let report = fuzz_buggy(FuzzConfig::new()).await;
    assert!(!report.is_clean());
    assert_eq!(report.operations(), 6);

    let f = failure(&report, "GET /search");
    assert_eq!(f.kind(), &FailureKind::ServerError(500), "{report}");
    assert_eq!(f.status(), Some(500));
    // Shrunk to just over the limit, not the 8 KiB string that found it.
    let q = f.request().uri().strip_prefix("/search?q=").unwrap();
    assert!(q.len() > 200 && q.len() < 1024, "{}", f.request().uri());

    let f = failure(&report, "GET /teapot");
    assert_eq!(f.kind(), &FailureKind::UndeclaredStatus(418), "{report}");

    let f = failure(&report, "GET /stats");
    let FailureKind::SchemaMismatch(errors) = f.kind() else {
        panic!("expected a schema mismatch: {report}");
    };
    assert_eq!(errors, &["/count: expected integer, got string \"many\""]);

    let f = failure(&report, "GET /slow/{n}");
    assert_eq!(f.kind(), &FailureKind::Timeout, "{report}");
    assert_eq!(f.request().uri(), "/slow/255");

    // The report reads well and says how to reproduce each failure.
    let text = report.to_string();
    assert!(text.starts_with("zapline fuzz: 6 operations, "), "{text}");
    assert!(text.contains("6 failures"), "{text}");
    assert!(text.contains("request: GET /ratio/0"), "{text}");
    assert!(text.contains("reproduce: client.request(Method::GET, \"/ratio/0\").send().await"));
    assert!(text.contains("ZAPLINE_FUZZ_SEED=24301"), "{text}");
}

#[tokio::test]
async fn assert_clean_panics_with_the_report() {
    let result = tokio::spawn(async {
        fuzz_buggy(FuzzConfig::new().only("/teapot"))
            .await
            .assert_clean();
    })
    .await;
    let msg = *result
        .unwrap_err()
        .into_panic()
        .downcast::<String>()
        .unwrap();
    assert!(
        msg.contains("1) GET /teapot (teapot): undeclared status 418 I'm a teapot"),
        "{msg}"
    );
}

#[tokio::test]
async fn runs_are_deterministic() {
    let summary = |r: &FuzzReport| {
        r.failures()
            .iter()
            .map(|f| {
                (
                    f.operation().to_owned(),
                    f.kind().clone(),
                    f.request().to_rust(),
                )
            })
            .collect::<Vec<_>>()
    };
    let a = fuzz_buggy(FuzzConfig::new().seed(7).cases(40)).await;
    let b = fuzz_buggy(FuzzConfig::new().seed(7).cases(40)).await;
    assert_eq!(a.requests(), b.requests());
    assert_eq!(summary(&a), summary(&b));
    assert_eq!(a.seed(), 7);
}

#[tokio::test]
async fn the_served_document_is_the_contract() {
    // Declaring 418 in the served spec makes the teapot correct.
    let app = App::new()
        .mount(teapot)
        .openapi(OpenApi::new("t", "1").modify(|spec| {
            spec["paths"]["/teapot"]["get"]["responses"]["418"] =
                json!({ "description": "Teapot" });
        }));
    app.test().fuzz().await.assert_clean();
}

#[tokio::test]
async fn panics_caught_by_middleware_are_still_reported() {
    let app = App::new()
        .mount(ratio)
        .wrap(zapline::middleware::CatchPanic::new());
    let report = app.test().fuzz().await;
    let f = failure(&report, "GET /ratio/{n}");
    let FailureKind::Panic(msg) = f.kind() else {
        panic!("expected a panic: {report}");
    };
    assert!(msg.contains("divide by zero"), "{msg}");
    assert_eq!(f.status(), Some(500));
}

// ---------------------------------------------------------------------------
// A correct app

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct Item {
    id: u64,
    name: String,
    price: f64,
    tags: Vec<String>,
    kind: Kind,
}

#[derive(Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Book,
    Game,
}

#[derive(Deserialize, JsonSchema, Validate)]
struct NewItem {
    #[validate(length(min = 1, max = 40))]
    name: String,
    #[validate(range(min = 0.0, max = 10000.0))]
    price: f64,
    #[serde(default)]
    tags: Vec<String>,
    kind: Kind,
}

#[derive(Deserialize, JsonSchema)]
struct Page {
    limit: Option<u8>,
    offset: Option<u32>,
}

type Store = Arc<Mutex<HashMap<u64, Item>>>;

/// Creates an item (the `status` makes the spec say 201, not 200).
#[post("/items", status = 201)]
async fn create_item(
    store: State<Store>,
    Valid(Json(new)): Valid<Json<NewItem>>,
) -> (StatusCode, Json<Item>) {
    let mut items = store.lock().unwrap();
    let id = items.len() as u64 + 1;
    let item = Item {
        id,
        name: new.name,
        price: new.price,
        tags: new.tags,
        kind: new.kind,
    };
    items.insert(id, item.clone());
    (StatusCode::CREATED, Json(item))
}

#[get("/items/{id}")]
async fn get_item(store: State<Store>, Path(id): Path<u64>) -> Option<Json<Item>> {
    store.lock().unwrap().get(&id).cloned().map(Json)
}

#[get("/items")]
async fn list_items(store: State<Store>, Query(page): Query<Page>) -> Json<Vec<Item>> {
    let items = store.lock().unwrap();
    let mut all: Vec<Item> = items.values().cloned().collect();
    all.sort_by_key(|i| i.id);
    let offset = page.offset.unwrap_or(0) as usize;
    let limit = page.limit.unwrap_or(20) as usize;
    Json(all.into_iter().skip(offset).take(limit).collect())
}

#[delete("/items/{id}")]
async fn delete_item(
    store: State<Store>,
    Path(id): Path<u64>,
    _token: BearerToken,
) -> Result<StatusCode> {
    match store.lock().unwrap().remove(&id) {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(Error::not_found()),
    }
}

/// Echoes a text body.
#[post("/echo")]
async fn echo(body: String) -> String {
    body
}

#[get("/hello/{name}")]
async fn hello(Path(name): Path<String>) -> String {
    format!("Hello, {name}!")
}

fn clean() -> App {
    App::new()
        .state(Store::default())
        .mount((create_item, get_item, list_items, delete_item, echo, hello))
        .get("/health", || async { "ok" })
        .openapi(OpenApi::new("clean", "1").bearer_auth("bearer"))
}

#[tokio::test]
async fn a_correct_app_is_clean() {
    let report = clean()
        .test()
        .fuzz()
        .config(FuzzConfig::new().bearer("secret"))
        .await;
    report.assert_clean();
    assert_eq!(report.operations(), 7);
    // 100 cases per operation by default.
    assert!(report.requests() >= 700, "{report}");
    assert!(report.to_string().contains("no failures"));
}

#[tokio::test]
async fn filters_and_case_counts() {
    let report = clean()
        .test()
        .fuzz()
        .config(
            FuzzConfig::new()
                .cases(10)
                .only("/items")
                .skip("delete_item"),
        )
        .await;
    report.assert_clean();
    assert_eq!(report.operations(), 3);
    assert_eq!(report.requests(), 30);
}

#[tokio::test]
async fn apps_without_the_openapi_plugin_are_fuzzed_too() {
    App::new()
        .mount((ratio, hello))
        .test()
        .fuzz()
        .config(FuzzConfig::new().skip("ratio"))
        .await
        .assert_clean();
}

#[get("/events")]
async fn events() -> Sse<futures_util::stream::Pending<Result<zapline::sse::Event>>> {
    Sse::new(futures_util::stream::pending())
}

#[get("/ws")]
async fn socket(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|_socket| async {})
}

#[tokio::test]
async fn websockets_and_event_streams_are_skipped() {
    let report = App::new().mount((events, socket)).test().fuzz().await;
    report.assert_clean();
    assert_eq!(report.operations(), 0);
    let skipped: Vec<&str> = report.skipped().iter().map(|(op, _)| op.as_str()).collect();
    assert_eq!(skipped, ["GET /events", "GET /ws"]);
}
