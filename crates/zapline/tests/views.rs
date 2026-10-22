//! `zapline::view`: templates, blocks for htmx, `HxRequest`, `Hx` headers
//! and hot reload.
#![cfg(feature = "views")]

use zapline::prelude::*;
use zapline::view::{context, Hx, HxRequest, Views};

const TEMPLATES: &str = "tests/fixtures/templates";

async fn todos(views: Views, hx: HxRequest) -> Result<Html<String>> {
    let todos = ["a", "<b>"];
    views.page_or_block(&hx, "todos.html", "list", context! { todos })
}

fn app() -> App {
    App::new()
        .views(TEMPLATES)
        .get("/todos", todos)
        .post("/todos", || async {
            Hx::trigger("todo-added").with(StatusCode::CREATED)
        })
        .get(
            "/missing",
            |v: Views| async move { v.render("nope.html", ()) },
        )
}

#[tokio::test]
async fn full_page_extends_the_layout_and_escapes() {
    let res = app().test().get("/todos").send().await;
    assert_eq!(res.status(), 200);
    let html = res.text();
    assert!(
        html.starts_with("<!doctype html><title>Todos</title>"),
        "{html}"
    );
    assert!(html.contains("<li>a</li><li>&lt;b&gt;</li>"), "{html}");
}

#[tokio::test]
async fn htmx_requests_get_only_the_block() {
    let client = app().test();
    let res = client
        .get("/todos")
        .header("hx-request", "true")
        .send()
        .await;
    assert_eq!(res.text(), "<li>a</li><li>&lt;b&gt;</li>");

    // Boosted navigation and history restores need the whole page.
    let res = client
        .get("/todos")
        .header("hx-request", "true")
        .header("hx-boosted", "true")
        .send()
        .await;
    assert!(res.text().starts_with("<!doctype html>"));
}

#[tokio::test]
async fn hx_response_headers() {
    let res = app().test().post("/todos").send().await;
    assert_eq!(res.status(), 201);
    assert_eq!(res.header("hx-trigger"), Some("todo-added"));
}

#[tokio::test]
async fn missing_template_is_a_500_naming_it() {
    // The details go to the log (and to the client only under ZAPLINE_DEV=1).
    let res = app().test().get("/missing").send().await;
    assert_eq!(res.status(), 500);
    assert_eq!(res.text(), r#"{"error":"internal server error"}"#);
}

#[tokio::test]
async fn templates_reload_without_a_rebuild_in_debug() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("page.html");
    std::fs::write(&file, "v1").unwrap();
    let client = App::new()
        .state(Views::new(dir.path(), true))
        .get("/", |v: Views| async move { v.render("page.html", ()) })
        .test();
    assert_eq!(client.get("/").send().await.text(), "v1");
    std::fs::write(&file, "v2").unwrap();
    assert_eq!(client.get("/").send().await.text(), "v2");
}

#[tokio::test]
async fn views_with_adds_globals_and_filters() {
    let client = App::new()
        .views_with(TEMPLATES, |env| {
            env.add_global("site", "Acme");
            env.add_template("inline.html", "{{ site | upper }}")
                .unwrap();
        })
        .get("/", |v: Views| async move { v.render("inline.html", ()) })
        .test();
    assert_eq!(client.get("/").send().await.text(), "ACME");
}
