//! The pages, driven like a browser without JavaScript: GET a page, POST a
//! form, follow the redirect.

use leptos_ssr::app;
use zapline::db::Database;
use zapline::test::TestClient;

async fn add(client: &TestClient, text: &str) {
    let res = client.post("/notes").form(&[("text", text)]).send().await;
    res.assert_status(303).assert_header("location", "/");
}

#[tokio::test]
async fn the_board_is_a_complete_html_document() {
    let client = app(Database::sqlite_memory()).test();
    let res = client.get("/").send().await;
    res.assert_status(200)
        .assert_header("content-type", "text/html; charset=utf-8")
        .assert_contains("<!DOCTYPE html><html lang=\"en\">")
        .assert_contains("<title>Notes</title>")
        .assert_contains("0 open, 0 done")
        .assert_contains("Nothing yet.")
        .assert_contains("<form method=\"post\" action=\"/notes\"");
}

#[tokio::test]
async fn adding_and_toggling_notes() {
    let client = app(Database::sqlite_memory()).test();
    add(&client, "  write the guide  ").await;
    add(&client, "ship it").await;

    let page = client.get("/").send().await.text();
    assert!(page.contains("2 open, 0 done"), "{page}");
    assert!(
        page.contains("<span>write the guide</span>"),
        "trimmed: {page}"
    );
    assert!(!page.contains("Nothing yet."));

    let res = client.post("/notes/1/toggle").send().await;
    res.assert_status(303).assert_header("location", "/");
    let page = client.get("/").send().await.text();
    assert!(page.contains("1 open, 1 done"), "{page}");
    assert!(page.contains("class=\"done\""), "{page}");
    assert!(page.contains("Reopen"), "{page}");

    client
        .post("/notes/1/delete")
        .send()
        .await
        .assert_status(303);
    let page = client.get("/").send().await.text();
    assert!(!page.contains("write the guide"), "{page}");
    assert!(page.contains("1 open, 0 done"), "{page}");

    client
        .post("/notes/99/toggle")
        .send()
        .await
        .assert_status(404);
    client
        .post("/notes/99/delete")
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn user_text_is_escaped() {
    let client = app(Database::sqlite_memory()).test();
    add(&client, "<script>alert('x')</script> & \"quotes\"").await;
    let page = client.get("/").send().await.text();
    assert!(!page.contains("<script>alert"), "{page}");
    assert!(page.contains("&lt;script&gt;"), "{page}");
}

#[tokio::test]
async fn invalid_input_shows_the_page_again_with_the_error_and_the_text() {
    let client = app(Database::sqlite_memory()).test();
    add(&client, "keep me").await;

    let long = "x".repeat(201);
    let res = client
        .post("/notes")
        .form(&[("text", long.as_str())])
        .send()
        .await;
    res.assert_status(422)
        .assert_contains("role=\"alert\"")
        .assert_contains("A note has 1 to 200 characters.")
        .assert_contains(&format!("value=\"{long}\""))
        .assert_contains("<span>keep me</span>");

    client
        .post("/notes")
        .form(&[("text", "   ")])
        .send()
        .await
        .assert_status(422)
        .assert_contains("A note has 1 to 200 characters.");
    let page = client.get("/").send().await.text();
    assert!(page.contains("1 open, 0 done"), "nothing was saved: {page}");
}

#[tokio::test]
async fn readiness_pings_the_database() {
    let client = app(Database::sqlite_memory()).test();
    client.get("/health/ready").send().await.assert_status(200);
}
