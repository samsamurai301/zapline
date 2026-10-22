//! End-to-end tests against a fresh in-memory database per test.

use std::time::Duration;

use serde_json::json;
use zapline::db::Database;
use zapline::mail::Mailer;
use zapline::test::TestClient;

use saas::app;
use saas::routes::todos::Todo;

/// A browser with its own cookies, signed up as `email`.
async fn signed_up(client: &TestClient, email: &str) -> TestClient {
    let browser = client.clone().with_cookies();
    browser
        .post("/api/auth/signup")
        .json(&json!({ "email": email, "password": "long enough" }))
        .send()
        .await
        .assert_status(201);
    browser
}

#[tokio::test]
async fn visitors_are_sent_to_the_login_page() {
    let client = app(Database::sqlite_memory()).test();
    let res = client.get("/").send().await;
    res.assert_status(303).assert_header("location", "/login");
    client.get("/api/todos").send().await.assert_status(401);
}

#[tokio::test]
async fn page_renders_and_htmx_gets_the_list_fragment() {
    let client = app(Database::sqlite_memory()).test();
    let ada = signed_up(&client, "ada@example.com").await;

    let page = ada.get("/").send().await;
    page.assert_status(200)
        .assert_contains("<h1>Todos</h1>")
        .assert_contains("ada@example.com");

    let res = ada
        .post("/todos")
        .header("hx-request", "true")
        .form(&[("title", "write tests")])
        .send()
        .await;
    res.assert_status(200).assert_contains("write tests");
    assert!(
        !res.text().contains("<h1>"),
        "htmx gets only the list: {}",
        res.text()
    );
}

#[tokio::test]
async fn each_user_sees_and_changes_only_their_todos() {
    let client = app(Database::sqlite_memory()).test();
    let ada = signed_up(&client, "ada@example.com").await;
    let bob = signed_up(&client, "bob@example.com").await;

    let res = ada
        .post("/api/todos")
        .json(&json!({ "title": "ship" }))
        .send()
        .await;
    res.assert_status(201);
    let todo: Todo = res.json();

    assert_eq!(
        ada.get("/api/todos").send().await.json::<Vec<Todo>>().len(),
        1
    );
    assert!(bob
        .get("/api/todos")
        .send()
        .await
        .json::<Vec<Todo>>()
        .is_empty());

    // Bob can't touch Ada's todo; Ada can.
    let path = format!("/todos/{}/toggle", todo.id);
    bob.post(&path).send().await.assert_status(404);
    bob.delete(&format!("/todos/{}", todo.id))
        .send()
        .await
        .assert_status(404);
    ada.post(&path).send().await.assert_status(200);
    let list: Vec<Todo> = ada.get("/api/todos").send().await.json();
    assert!(list[0].done);
}

#[tokio::test]
async fn validation_errors_are_422() {
    let client = app(Database::sqlite_memory()).test();
    let ada = signed_up(&client, "ada@example.com").await;
    ada.post("/api/todos")
        .json(&json!({ "title": "" }))
        .send()
        .await
        .assert_status(422);
}

#[tokio::test]
async fn signing_up_sends_a_welcome_email() {
    let client = app(Database::sqlite_memory()).test();
    signed_up(&client, "ada@example.com").await;

    // A background worker sends it; give it a moment.
    let mailer: Mailer = client.resources().await.require().unwrap();
    for _ in 0..200 {
        if let Some(mail) = mailer.sent().first() {
            assert_eq!(mail.to, ["ada@example.com"]);
            assert_eq!(mail.subject, "Welcome to saas");
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no welcome email");
}
