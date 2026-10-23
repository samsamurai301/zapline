//! Generated clients for a representative app: snapshots of the TypeScript
//! and JavaScript output, the `/_zapline/api.js` route, and (when `tsc` and
//! `node` are installed) a strict type-check and a real run of the client
//! against the app over a socket.
//!
//! Regenerate the snapshots with
//! `ZAPLINE_UPDATE_SNAPSHOTS=1 cargo test -p zapline --all-features --test client`.
#![cfg(all(
    feature = "macros",
    feature = "openapi",
    feature = "query",
    feature = "validate",
    feature = "multipart",
    feature = "form"
))]

#[path = "fixtures/client/app.rs"]
mod app;
mod server_support;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use zapline::openapi::{codegen, spec_for, CLIENT_JS_PATH};
use zapline::prelude::*;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/client")
}

fn spec() -> Value {
    spec_for(app::app(), app::api())
}

/// Compares `actual` with the snapshot `name`, or rewrites it.
fn snapshot(name: &str, actual: &str) {
    let path = fixtures().join(name);
    if std::env::var_os("ZAPLINE_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .map_or(expected.lines().count().min(actual.lines().count()), |i| i)
            + 1;
        panic!(
            "{name} is out of date (first difference at line {line}); rerun with \
             ZAPLINE_UPDATE_SNAPSHOTS=1 and review the diff.\n--- generated ---\n{actual}"
        );
    }
}

#[test]
fn typescript_snapshot() {
    let ts = codegen::typescript(&spec()).unwrap();
    assert_eq!(ts.skipped, vec!["GET /"]);
    assert_eq!(ts.operations, 13);
    snapshot("api.ts", &ts.source);
}

#[test]
fn javascript_snapshot() {
    let js = codegen::javascript(&spec()).unwrap();
    assert_eq!(js.operations, 13);
    snapshot("api.js", &js.source);
}

#[test]
fn catch_all_parameters_are_marked() {
    let spec = spec();
    let param = &spec["paths"]["/docs-files/{path}"]["get"]["parameters"][0];
    assert_eq!(param["x-zapline-catch-all"], true);
    let other = &spec["paths"]["/tasks/{id}"]["get"]["parameters"][0];
    assert!(other.get("x-zapline-catch-all").is_none());
}

#[tokio::test]
async fn client_js_route_serves_the_module() {
    let c = app::app().openapi(app::api().client_js().no_docs()).test();
    let r = c.get(CLIENT_JS_PATH).send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    let expected = codegen::javascript(&spec()).unwrap().source;
    assert_eq!(r.text(), expected);

    // Not part of the API it describes.
    let served: Value = c.get("/openapi.json").send().await.json();
    assert!(served["paths"].get(CLIENT_JS_PATH).is_none());
}

#[tokio::test]
async fn client_js_is_opt_in() {
    let c = app::app().openapi(app::api()).test();
    assert_eq!(c.get(CLIENT_JS_PATH).send().await.status(), 404);
}

#[tokio::test]
async fn client_js_is_public_behind_required_auth() {
    let c = app::app()
        .openapi(app::api().client_js())
        .wrap_route(
            Authenticate::new(|_: &Request| async { Ok::<_, Error>(None::<String>) }).required(),
        )
        .test();
    assert_eq!(c.get("/tasks").send().await.status(), 401);
    assert_eq!(c.get(CLIENT_JS_PATH).send().await.status(), 200);
}

/// Runs only in the child process spawned by `export_without_openapi_exits`.
#[test]
fn export_without_openapi_child() {
    if std::env::var_os(zapline::openapi::ENV_EXPORT_OPENAPI).is_none() {
        return;
    }
    let _ = app::app().run("127.0.0.1:0");
    unreachable!("App::run should have exited");
}

/// Asked for a spec it doesn't have, an app exits with a dedicated code
/// instead of starting to serve (`zapline dev` relies on it).
#[test]
fn export_without_openapi_exits() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "export_without_openapi_child",
            "--exact",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(
            zapline::openapi::ENV_EXPORT_OPENAPI,
            dir.path().join("spec.json"),
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no OpenAPI document"));
}

// ---------------------------------------------------------------------------
// External tools: skipped (with a note) when they are not installed.

fn tool(name: &str) -> Option<PathBuf> {
    let found = Command::new(name).arg("--version").output();
    match found {
        Ok(out) if out.status.success() => Some(PathBuf::from(name)),
        _ => {
            eprintln!("note: `{name}` is not installed; skipping this check");
            None
        }
    }
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{cmd:?} failed:\n{text}");
    text
}

/// Strict `tsc --noEmit`, with the options that catch the most.
fn tsc(dir: &Path, file: &str, js: bool) -> String {
    let tsc = tool("tsc").unwrap();
    let mut cmd = Command::new(tsc);
    cmd.current_dir(dir).args([
        "--noEmit",
        "--strict",
        "--exactOptionalPropertyTypes",
        "--noUncheckedIndexedAccess",
        "--noImplicitOverride",
        "--target",
        "es2022",
        "--lib",
        "es2022,dom",
        "--module",
        "esnext",
        "--moduleResolution",
        "bundler",
    ]);
    if js {
        cmd.args(["--allowJs", "--checkJs"]);
    }
    cmd.arg(file);
    run(&mut cmd)
}

#[test]
fn typescript_client_type_checks() {
    if tool("tsc").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("api.ts"),
        codegen::typescript(&spec()).unwrap().source,
    )
    .unwrap();
    std::fs::copy(fixtures().join("usage.ts"), dir.path().join("usage.ts")).unwrap();
    tsc(dir.path(), "usage.ts", false);
}

#[test]
fn javascript_client_type_checks() {
    if tool("tsc").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("api.js"),
        codegen::javascript(&spec()).unwrap().source,
    )
    .unwrap();
    std::fs::copy(fixtures().join("usage.js"), dir.path().join("usage.js")).unwrap();
    tsc(dir.path(), "usage.js", true);
}

/// Runs `e2e.mjs` with node against the app on a real socket, importing the
/// module the app itself serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn javascript_client_talks_to_the_app() {
    if tool("node").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let server =
        server_support::TestServer::spawn(app::app().openapi(app::api().client_js())).await;
    let base = format!("http://{}", server.addr);

    // The module as served, so this exercises the route too.
    let module = app::app()
        .openapi(app::api().client_js())
        .test()
        .get(CLIENT_JS_PATH)
        .send()
        .await
        .text();
    std::fs::write(dir.path().join("api.js"), module).unwrap();
    std::fs::copy(fixtures().join("e2e.mjs"), dir.path().join("e2e.mjs")).unwrap();

    let dir_path = dir.path().to_owned();
    let out = tokio::task::spawn_blocking(move || {
        run(Command::new("node")
            .current_dir(&dir_path)
            .arg("e2e.mjs")
            .arg(base))
    })
    .await
    .unwrap();
    let last = out.lines().last().unwrap_or_default();
    let got: Value = serde_json::from_str(last).unwrap_or_else(|_| panic!("{out}"));
    let expected = serde_json::json!({
        "list": 2,
        "created": "write docs",
        "validation": {
            "fields": ["priority", "title"],
            "title": "length must be between 1 and 80",
            "operation": "createTask"
        },
        "typeMismatch": { "fields": ["title"], "status": 422 },
        "notFound": [404, null, "getTask"],
        "project": "web app#7",
        "upload": { "files": ["a.txt", "-"], "bytes": 6 },
        "login": "ada@example.com",
        "badLogin": [401, "bad_credentials", false],
        "area": 6,
        "file": "file guide/intro.md",
        "imported": 2,
        "exported": 2,
        "events": true,
        "deleted": true,
        "csrf": { "post": "tok123", "get": null },
        "aborted": "AbortError"
    });
    assert_eq!(got, expected, "{out}");
}
