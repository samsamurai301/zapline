//! Diagnostic codes and the dev error page: 500s and panics in a debug build
//! answer this machine with the error, its code and fix, and a prompt to
//! paste into an AI assistant.
#![cfg(all(debug_assertions, feature = "json"))]

use std::net::SocketAddr;

use serde_json::Value;
use zapline::middleware::CatchPanic;
use zapline::prelude::*;

#[derive(Debug)]
struct Repo;

async fn needs_repo(_repo: Dep<Repo>) -> &'static str {
    "unreachable"
}

async fn boom() -> &'static str {
    let items: Vec<u32> = Vec::new();
    if items.is_empty() {
        panic!("no items to show");
    }
    "unreachable"
}

async fn fails() -> Result<&'static str> {
    let n: u32 = "x".parse()?;
    let _ = n;
    Ok("unreachable")
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct NewTodo {
    title: String,
}

async fn create(Json(_t): Json<NewTodo>) -> &'static str {
    "ok"
}

fn app() -> App {
    App::new()
        .dev_errors()
        .get("/repo", needs_repo)
        .get("/boom", boom)
        .get("/fails", fails)
        .post("/todos", create)
        .get("/missing", || async { Err::<&str, _>(Error::not_found()) })
}

#[tokio::test]
async fn json_clients_get_problem_json_with_code_and_fix() {
    let client = app().test();
    let res = client
        .get("/repo")
        .header("accept", "application/json")
        .send()
        .await;
    res.assert_status(500)
        .assert_header("content-type", "application/problem+json")
        .assert_header("x-zapline-diagnostic", "ZL0002");
    let body: Value = res.json();
    assert_eq!(body["code"], "ZL0002");
    assert_eq!(body["status"], 500);
    assert_eq!(body["title"], "No provider for a dependency");
    assert_eq!(body["error_code"], "missing_dependency");
    assert_eq!(body["request"], "GET /repo");
    assert!(
        body["type"]
            .as_str()
            .unwrap()
            .ends_with("/errors.html#zl0002"),
        "{body}"
    );
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("no provider registered for `dev_errors::Repo`"),
        "{detail}"
    );
    assert_eq!(body["error"], body["detail"]);
    // The fix names the missing type.
    let fix = body["fix"].as_str().unwrap();
    assert!(
        fix.contains(".provide(value) // value: dev_errors::Repo"),
        "{fix}"
    );
}

#[tokio::test]
async fn browsers_get_an_html_page_with_a_copyable_prompt() {
    let client = app().test();
    let res = client
        .get("/repo?x=1")
        .header("accept", "text/html,application/xhtml+xml")
        .header("authorization", "Bearer secret-token")
        .send()
        .await;
    res.assert_status(500)
        .assert_header("content-type", "text/html; charset=utf-8")
        .assert_contains("<h1>ZL0002 · No provider for a dependency</h1>")
        .assert_contains("no provider registered for `dev_errors::Repo`")
        .assert_contains("<h2>Fix</h2>")
        .assert_contains("zapline explain ZL0002")
        .assert_contains("<pre id=prompt>My zapline app answered GET /repo?x=1 with 500")
        .assert_contains("Copy fix prompt")
        .assert_contains("Suggested fix: .provide(value)");
    let page = res.text();
    assert!(!page.contains("secret-token"), "credentials are hidden");
    assert!(page.contains("<tr><th>authorization<td><code>[hidden]</code>"));
}

#[tokio::test]
async fn panics_show_where_they_happened() {
    let client = app().test();
    let res = client
        .get("/boom")
        .header("accept", "application/json")
        .send()
        .await;
    res.assert_status(500);
    let body: Value = res.json();
    assert_eq!(body["code"], "ZL0080");
    assert_eq!(body["error_code"], "panic");
    assert_eq!(body["detail"], "panicked: no items to show");
    let at = body["location"].as_str().unwrap();
    assert!(
        at.starts_with("crates/zapline/tests/dev_errors.rs:"),
        "{at}"
    );

    let html = client
        .get("/boom")
        .header("accept", "text/html")
        .send()
        .await;
    html.assert_status(500)
        .assert_contains("ZL0080 · Handler panicked")
        .assert_contains("Panicked at <code id=location>crates/zapline/tests/dev_errors.rs:");
}

#[tokio::test]
async fn catch_panic_keeps_its_response_and_feeds_the_page() {
    let client = App::new()
        .dev_errors()
        .wrap(CatchPanic::new())
        .get("/boom", boom)
        .test();
    let res = client
        .get("/boom")
        .header("accept", "application/json")
        .send()
        .await;
    let body: Value = res.json();
    assert_eq!(body["code"], "ZL0080");
    assert!(body["location"].as_str().is_some(), "{body}");
}

#[tokio::test]
async fn unhandled_errors_get_the_generic_code_and_their_cause() {
    let client = app().test();
    let res = client
        .get("/fails")
        .header("accept", "application/json")
        .send()
        .await;
    let body: Value = res.json();
    assert_eq!(body["code"], "ZL0081");
    assert_eq!(
        body["detail"],
        "internal server error: invalid digit found in string"
    );
}

#[tokio::test]
async fn rejections_keep_their_body_and_name_their_code() {
    let client = app().test();
    let res = client
        .post("/todos")
        .header("content-type", "application/json")
        .body("{\"title\":")
        .send()
        .await;
    res.assert_status(422)
        .assert_header("content-type", "application/json")
        .assert_header("x-zapline-diagnostic", "ZL0062");
    let body: Value = res.json();
    assert_eq!(body["code"], "invalid_json");

    let res = client.post("/todos").body("{}").send().await;
    res.assert_status(415)
        .assert_header("x-zapline-diagnostic", "ZL0064");
}

#[tokio::test]
async fn the_apps_own_client_errors_are_left_alone() {
    let client = app().test();
    let res = client.get("/missing").send().await;
    res.assert_status(404)
        .assert_text(r#"{"error":"not found"}"#);
    assert!(res.header("x-zapline-diagnostic").is_none());
}

#[tokio::test]
async fn other_machines_get_the_generic_error() {
    let client = app().test();
    let remote = SocketAddr::from(([203, 0, 113, 7], 4000));
    let res = client
        .get("/repo")
        .header("accept", "text/html")
        .remote_addr(remote)
        .send()
        .await;
    res.assert_status(500)
        .assert_header("content-type", "application/json")
        .assert_text(r#"{"error":"internal server error","code":"missing_dependency"}"#);
    assert!(res.header("x-zapline-diagnostic").is_none());
}

#[tokio::test]
async fn off_unless_asked_for() {
    let client = App::new().get("/repo", needs_repo).test();
    let res = client
        .get("/repo")
        .header("accept", "text/html")
        .send()
        .await;
    res.assert_status(500)
        .assert_text(r#"{"error":"internal server error","code":"missing_dependency"}"#);
}

#[tokio::test]
async fn the_console_lists_errors_with_codes_as_json() {
    let client = App::new()
        .dev_console()
        .get("/repo", needs_repo)
        .get("/ok", || async { "ok" })
        .test();
    client.get("/ok").send().await.assert_status(200);
    client.get("/repo").send().await.assert_status(500);

    let api = client.get("/_dev/api").send().await;
    api.assert_status(200)
        .assert_header("content-type", "application/json");
    let data: Value = api.json();
    let requests = data["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2, "{data}");
    assert_eq!(requests[0]["path"], "/repo");
    assert_eq!(requests[0]["status"], 500);
    assert_eq!(requests[0]["code"], "ZL0002");
    assert_eq!(requests[1]["path"], "/ok");
    assert!(requests[1].get("code").is_none());

    let errors = data["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["code"], "ZL0002");
    assert_eq!(errors[0]["error_code"], "missing_dependency");
    assert!(errors[0]["fix"].as_str().unwrap().contains(".provide("));
    assert!(errors[0]["at_ms"].as_u64().unwrap() > 0);
    let routes = data["routes"].as_array().unwrap();
    assert!(routes.iter().any(|r| r["path"] == "/repo"));

    client
        .get("/_dev")
        .send()
        .await
        .assert_contains("<h2>Recent errors</h2>")
        .assert_contains("<code>ZL0002</code>");
    // Only this machine.
    client
        .get("/_dev/api")
        .remote_addr(SocketAddr::from(([203, 0, 113, 7], 4000)))
        .send()
        .await
        .assert_status(404);
}

// ---- codes in startup errors and panics ------------------------------------

#[test]
fn startup_errors_carry_the_code_and_fix() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt
        .block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            App::new()
                .on_startup(|res: Resources| async move {
                    res.require::<String>()?;
                    Ok(())
                })
                .serve(listener)
                .await
        })
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.starts_with("startup failed: 500 Internal Server Error: resource `alloc::string::String` is not registered [ZL0004]"),
        "{text}"
    );
    assert!(
        text.contains("\n  fix: Register it first: .state(value) // value: alloc::string::String"),
        "{text}"
    );
    assert!(text.ends_with("\n  see: zapline explain ZL0004"), "{text}");
}

#[test]
#[should_panic(
    expected = "duplicate route: GET /a [ZL0010]\n  fix: Remove one of the two registrations of GET /a"
)]
fn duplicate_routes_name_their_code() {
    let _ = App::new()
        .get("/a", || async { "1" })
        .get("/a", || async { "2" })
        .route_table();
}

#[test]
#[should_panic(expected = "[ZL0011]")]
fn conflicting_routes_name_their_code() {
    let _ = App::new()
        .get("/users/{id}", || async { "1" })
        .post("/users/{name}", || async { "2" })
        .route_table();
}

#[test]
fn errors_expose_their_diagnostic_code() {
    let res = Resources::default();
    let err = res.dep::<Repo>().unwrap_err();
    assert_eq!(err.diagnostic_code(), Some("ZL0002"));
    assert_eq!(err.code(), Some("missing_dependency"));
    assert!(err.to_string().ends_with("[ZL0002]"), "{err}");
    assert_eq!(Error::payload_too_large().diagnostic_code(), Some("ZL0067"));
    assert_eq!(Error::not_found().diagnostic_code(), None);
}

#[cfg(feature = "config")]
#[test]
fn config_errors_keep_their_code_through_question_mark() {
    use zapline::config::Config;
    fn load() -> Result<u16> {
        let config = Config::builder().toml_str("name = \"x\"").build()?;
        Ok(config.get::<u16>("server.port")?)
    }
    let err = load().unwrap_err();
    assert_eq!(err.diagnostic_code(), Some("ZL0032"));
}
