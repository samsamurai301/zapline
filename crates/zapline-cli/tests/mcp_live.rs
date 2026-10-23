//! `zapline mcp`'s live tools against a real zapline app served on a socket
//! in this process (no cargo involved): recent requests and errors with
//! their diagnostic codes, routes called over HTTP, and error explanations.
#![cfg(debug_assertions)]

use std::sync::OnceLock;

use serde_json::{json, Value};
use zapline::prelude::*;

#[derive(Debug)]
struct Repo;

#[derive(serde::Deserialize)]
struct NewTodo {
    title: String,
}

/// One app for the whole file: requests from every test land in its log,
/// so each test uses its own paths.
fn app_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                App::new()
                    .dev_console()
                    .get("/hello", || async { "hi" })
                    .get("/repo", |_repo: Dep<Repo>| async { "unreachable" })
                    .get("/boom", || async {
                        let v: Vec<u8> = Vec::new();
                        if v.is_empty() {
                            panic!("nothing in the list");
                        }
                        "unreachable"
                    })
                    .post("/todos", |Json(t): Json<NewTodo>| async move {
                        (StatusCode::CREATED, Json(json!({ "title": t.title })))
                    })
                    .get("/big", || async { "x".repeat(10_000) })
                    .serve_with_shutdown(listener, std::future::pending())
                    .await
                    .unwrap();
            });
        });
        format!("http://{addr}")
    })
}

fn call(tool: &str, mut args: Value) -> (String, bool) {
    args["url"] = json!(app_url());
    let reply = zapline_cli::mcp::handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    }))
    .unwrap();
    let result = &reply["result"];
    (
        result["content"][0]["text"].as_str().unwrap().to_string(),
        result["isError"].as_bool().unwrap(),
    )
}

#[test]
fn call_route_returns_status_headers_and_body() {
    let (text, err) = call("call_route", json!({ "path": "/hello" }));
    assert!(!err, "{text}");
    assert_eq!(
        text,
        "200 OK\ncontent-type: text/plain; charset=utf-8\n\nhi\n"
    );

    let (text, _) = call(
        "call_route",
        json!({ "method": "post", "path": "/todos", "body": "{\"title\": \"buy milk\"}" }),
    );
    assert_eq!(
        text,
        "201 Created\ncontent-type: application/json\n\n{\"title\":\"buy milk\"}\n"
    );
}

#[test]
fn call_route_cuts_long_bodies() {
    let (text, _) = call("call_route", json!({ "path": "/big" }));
    assert!(text.len() < 2_200, "{}", text.len());
    assert!(text.ends_with("… [8000 more bytes]\n"), "{text}");
}

#[test]
fn a_500_comes_back_with_its_code_and_fix() {
    let (text, err) = call("call_route", json!({ "path": "/repo" }));
    assert!(!err);
    assert!(
        text.starts_with("500 Internal Server Error\ncontent-type: application/problem+json\n"),
        "{text}"
    );
    assert!(text.contains("x-zapline-diagnostic: ZL0002"), "{text}");
    assert!(text.contains(r#""code":"ZL0002""#), "{text}");
    assert!(
        text.contains(r#""fix":".provide(value) // value: mcp_live::Repo"#),
        "{text}"
    );
}

#[test]
fn a_rejection_names_its_code_and_fix() {
    let (text, _) = call(
        "call_route",
        json!({ "method": "POST", "path": "/todos", "body": "{\"title\":", "headers": { "content-type": "application/json" } }),
    );
    assert!(text.starts_with("422 Unprocessable Entity\n"), "{text}");
    assert!(text.contains("x-zapline-diagnostic: ZL0062"), "{text}");
    assert!(
        text.contains("ZL0062: Malformed JSON body\n  fix: Send valid JSON"),
        "{text}"
    );
}

#[test]
fn recent_errors_list_codes_locations_and_fixes() {
    call("call_route", json!({ "path": "/repo?from=errors" }));
    call("call_route", json!({ "path": "/boom" }));
    let (text, err) = call("recent_errors", json!({ "limit": 50 }));
    assert!(!err, "{text}");
    assert!(
        text.contains("ZL0002 500 GET /repo?from=errors: no provider registered for `mcp_live::Repo`\n  fix: .provide(value) // value: mcp_live::Repo"),
        "{text}"
    );
    assert!(text.contains("ZL0080 500 GET /boom: panicked: nothing in the list\n  at: crates/zapline-cli/tests/mcp_live.rs:"), "{text}");
    // The fix is already there: no second copy appended.
    assert_eq!(text.matches("ZL0002 No provider").count(), 0, "{text}");
}

#[test]
fn recent_requests_are_one_line_each() {
    call("call_route", json!({ "path": "/hello?from=requests" }));
    call("call_route", json!({ "path": "/repo?from=requests" }));
    let (text, err) = call("recent_requests", json!({ "limit": 50 }));
    assert!(!err, "{text}");
    let hello = text
        .lines()
        .find(|l| l.contains("/hello?from=requests"))
        .unwrap();
    assert!(
        hello.starts_with("200 GET /hello?from=requests "),
        "{hello}"
    );
    let repo = text
        .lines()
        .find(|l| l.contains("/repo?from=requests"))
        .unwrap();
    assert!(
        repo.starts_with("500 GET /repo?from=requests ") && repo.ends_with(" ZL0002"),
        "{repo}"
    );
    // The console's own requests are not listed.
    assert!(!text.contains("/_dev"), "{text}");

    // One request line, how many are left out, and the fix of any code shown.
    let (one, _) = call("recent_requests", json!({ "limit": 1 }));
    let lines: Vec<&str> = one.lines().collect();
    assert!(lines[0].starts_with(char::is_numeric), "{one}");
    assert!(
        lines[1].starts_with("(+") && lines[1].ends_with(" older)"),
        "{one}"
    );
    assert!(
        lines[2..]
            .iter()
            .all(|l| l.starts_with("ZL") && l.contains(": fix: ")),
        "{one}"
    );
}

#[test]
fn explain_error_needs_no_app() {
    let reply = zapline_cli::mcp::handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "explain_error", "arguments": { "code": "zl44" } }
    }))
    .unwrap();
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("ZL0044: No database registered"), "{text}");
    assert!(text.contains(".database(Database::from_env())"), "{text}");
}

#[test]
fn an_app_without_the_console_is_reported_with_its_code() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            App::new()
                .get("/", || async { "plain" })
                .serve_with_shutdown(listener, std::future::pending())
                .await
                .unwrap();
        });
    });
    let reply = zapline_cli::mcp::handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "recent_errors", "arguments": { "url": format!("http://{addr}") } }
    }))
    .unwrap();
    assert_eq!(reply["result"]["isError"], true);
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("has no dev console [ZL0090]"), "{text}");
    // The fix is appended from the registry.
    assert!(text.contains("App::new().dev_console()"), "{text}");
}
