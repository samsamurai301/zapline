//! `zapline::db` against a real Postgres. Runs only when
//! `ZAPLINE_TEST_POSTGRES_URL` is set (CI starts a Postgres service), e.g.
//! `ZAPLINE_TEST_POSTGRES_URL=postgres://postgres@localhost/postgres`.
#![cfg(feature = "postgres")]

use serde_json::json;
use zapline::db::sqlx;
use zapline::db::{Database, Db, DbCheck};
use zapline::prelude::*;

fn url() -> Option<String> {
    std::env::var("ZAPLINE_TEST_POSTGRES_URL").ok()
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}-{}", std::process::id(), fastrand_u64())
}

fn fastrand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

async fn create(db: Db<sqlx::Postgres>, Json(v): Json<serde_json::Value>) -> Result<StatusCode> {
    sqlx::query("INSERT INTO notes (slug, body) VALUES ($1, $2)")
        .bind(v["slug"].as_str().unwrap_or_default())
        .bind(v["body"].as_str().unwrap_or_default())
        .execute(&*db)
        .await?;
    Ok(StatusCode::CREATED)
}

async fn body(db: Db<sqlx::Postgres>, Path(slug): Path<String>) -> Result<String> {
    let (body,): (String,) = sqlx::query_as("SELECT body FROM notes WHERE slug = $1")
        .bind(slug)
        .fetch_one(&*db)
        .await?;
    Ok(body)
}

fn app(url: String) -> App {
    App::new()
        .database(Database::postgres(url).migrations("tests/fixtures/pg_migrations"))
        .post("/notes", create)
        .get("/notes/{slug}", body)
        .plugin(Health::new().check("database", DbCheck::<sqlx::Postgres>::default()))
}

#[tokio::test]
async fn postgres_round_trip_and_error_mapping() {
    let Some(url) = url() else {
        eprintln!("ZAPLINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let client = app(url).test();
    let slug = unique("note");
    let note = json!({"slug": slug, "body": "hello"});

    assert_eq!(client.post("/notes").json(&note).send().await.status(), 201);
    let res = client.get(&format!("/notes/{slug}")).send().await;
    assert_eq!(res.text(), "hello");

    let dup = client.post("/notes").json(&note).send().await;
    assert_eq!(dup.status(), 409);
    assert_eq!(client.get("/notes/missing-slug").send().await.status(), 404);
    assert_eq!(client.get("/health/ready").send().await.status(), 200);
}
