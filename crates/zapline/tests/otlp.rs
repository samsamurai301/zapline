//! OTLP export: request spans, child spans and events reach a collector
//! (an in-process fake) as OpenTelemetry JSON, continuing upstream traces.
#![cfg(all(feature = "otlp", feature = "macros"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use zapline::prelude::*;
use zapline::telemetry::{Telemetry, Trace};

#[derive(Clone, Default)]
struct Received(Arc<Mutex<Vec<Value>>>);

async fn collect(State(received): State<Received>, headers: HeaderMap, body: String) -> StatusCode {
    assert_eq!(headers.get("x-api-key").unwrap(), "k=1");
    received
        .0
        .lock()
        .unwrap()
        .push(serde_json::from_str(&body).unwrap());
    StatusCode::OK
}

async fn collector(received: Received) -> String {
    let app = App::new().state(received).post("/v1/traces", collect);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    format!("http://{addr}")
}

#[get("/users/{id}")]
async fn user(id: Path<u32>) -> Result<String> {
    tracing::info_span!("load_user", user.id = id.0).in_scope(|| {
        tracing::info!(cached = false, "loading");
    });
    if id.0 == 0 {
        return Err(Error::internal("boom"));
    }
    Ok("ada".into())
}

fn spans(batches: &[Value]) -> Vec<Value> {
    batches
        .iter()
        .flat_map(|b| b["resourceSpans"].as_array().unwrap().clone())
        .flat_map(|rs| rs["scopeSpans"].as_array().unwrap().clone())
        .flat_map(|ss| ss["spans"].as_array().unwrap().clone())
        .collect()
}

fn attr<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)
        .map(|a| &a["value"])
}

#[tokio::test]
async fn spans_reach_the_collector() {
    let received = Received::default();
    let base = collector(received.clone()).await;
    std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=k%3D1");
    let dispatch = Telemetry::new()
        .service_name("shop")
        .otlp(&base)
        .writer(std::io::sink)
        .into_dispatch()
        .unwrap();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let client = App::new().plugin(Trace::new()).mount(user).test();
    client
        .get("/users/7")
        .header(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        )
        .send()
        .await
        .assert_status(200);
    client.get("/users/0").send().await.assert_status(500);

    let mut all = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        all = spans(&received.0.lock().unwrap());
        if all.len() >= 4 {
            break;
        }
    }
    assert_eq!(all.len(), 4, "{all:#?}");
    let batch = &received.0.lock().unwrap()[0];
    let resource = &batch["resourceSpans"][0]["resource"];
    assert_eq!(
        attr(resource, "service.name").unwrap()["stringValue"],
        "shop"
    );

    let requests: Vec<&Value> = all.iter().filter(|s| s["kind"] == 2).collect();
    assert_eq!(requests.len(), 2);
    let ok = requests
        .iter()
        .find(|s| attr(s, "http.response.status_code").unwrap()["intValue"] == "200")
        .unwrap();
    assert_eq!(ok["name"], "GET /users/{id}");
    // It continues the upstream trace.
    assert_eq!(ok["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(ok["parentSpanId"], "00f067aa0ba902b7");
    assert_eq!(
        attr(ok, "http.request.method").unwrap()["stringValue"],
        "GET"
    );
    assert!(attr(ok, "traceparent").is_none());
    assert_eq!(ok["status"], serde_json::json!({}));
    let start: u128 = ok["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
    let end: u128 = ok["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
    assert!(end >= start);

    let child = all
        .iter()
        .find(|s| s["name"] == "load_user" && s["traceId"] == ok["traceId"])
        .unwrap();
    assert_eq!(child["kind"], 1);
    assert_eq!(child["parentSpanId"], ok["spanId"]);
    assert_eq!(attr(child, "user.id").unwrap()["intValue"], "7");
    let event = &child["events"][0];
    assert_eq!(event["name"], "loading");
    assert_eq!(attr(event, "cached").unwrap()["boolValue"], false);

    let failed = requests
        .iter()
        .find(|s| s["traceId"] != ok["traceId"])
        .unwrap();
    assert_eq!(failed["status"]["code"], 2);
    assert!(failed.get("parentSpanId").is_none());
    assert_eq!(failed["traceId"].as_str().unwrap().len(), 32);
}
