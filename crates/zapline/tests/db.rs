//! `zapline::db` with in-memory SQLite: pool, migrations, extractor, error
//! mapping and the health check.
#![cfg(feature = "sqlite")]

use serde::{Deserialize, Serialize};
use serde_json::json;
use zapline::db::sqlx::{self, FromRow};
use zapline::db::{Database, DbCheck, SqliteDb as Db};
use zapline::prelude::*;

const MIGRATIONS: &str = "tests/fixtures/db_migrations";

#[derive(Serialize, Deserialize, FromRow)]
struct Note {
    id: i64,
    slug: String,
    body: String,
}

#[derive(Deserialize)]
struct NewNote {
    slug: String,
    body: String,
}

async fn create(db: Db, Json(new): Json<NewNote>) -> Result<(StatusCode, Json<Note>)> {
    let note =
        sqlx::query_as::<_, Note>("INSERT INTO notes (slug, body) VALUES (?, ?) RETURNING *")
            .bind(new.slug)
            .bind(new.body)
            .fetch_one(&*db)
            .await?;
    Ok((StatusCode::CREATED, Json(note)))
}

async fn get_one(db: Db, Path(slug): Path<String>) -> Result<Json<Note>> {
    let note = sqlx::query_as::<_, Note>("SELECT * FROM notes WHERE slug = ?")
        .bind(slug)
        .fetch_one(&*db)
        .await?;
    Ok(Json(note))
}

fn app() -> App {
    App::new()
        .database(Database::sqlite_memory().migrations(MIGRATIONS))
        .post("/notes", create)
        .get("/notes/{slug}", get_one)
        .plugin(Health::new().check("database", DbCheck::<sqlx::Sqlite>::default()))
}

#[tokio::test]
async fn migrations_run_and_queries_work() {
    let client = app().test();
    let res = client
        .post("/notes")
        .json(&json!({"slug": "hello", "body": "world"}))
        .send()
        .await;
    assert_eq!(res.status(), 201, "{}", res.text());
    let note: Note = res.json();
    assert_eq!(note.id, 1);

    let res = client.get("/notes/hello").send().await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.json::<Note>().body, "world");
}

#[tokio::test]
async fn missing_row_is_404_and_duplicate_is_409() {
    let client = app().test();
    assert_eq!(client.get("/notes/nope").send().await.status(), 404);

    let body = json!({"slug": "dup", "body": "x"});
    assert_eq!(client.post("/notes").json(&body).send().await.status(), 201);
    let res = client.post("/notes").json(&body).send().await;
    assert_eq!(res.status(), 409);
    assert_eq!(res.json::<serde_json::Value>()["code"], "unique_violation");
}

#[tokio::test]
async fn every_app_gets_its_own_database() {
    let a = app().test();
    let b = app().test();
    let body = json!({"slug": "only-in-a", "body": "x"});
    assert_eq!(a.post("/notes").json(&body).send().await.status(), 201);
    assert_eq!(b.get("/notes/only-in-a").send().await.status(), 404);
}

#[tokio::test]
async fn readiness_pings_the_database() {
    let client = app().test();
    let res = client.get("/health/ready").send().await;
    assert_eq!(res.status(), 200, "{}", res.text());
}

#[tokio::test]
#[should_panic(expected = "ZAPLINE_TEST_UNSET_DATABASE_URL environment variable is not set")]
async fn missing_database_url_fails_startup_with_a_hint() {
    let client = App::new()
        .database(Database::from_env_var("ZAPLINE_TEST_UNSET_DATABASE_URL"))
        .get("/", || async { "hi" })
        .test();
    client.start().await;
}

#[tokio::test]
async fn db_without_database_is_a_clear_500() {
    async fn handler(_db: Db) -> &'static str {
        "unreachable"
    }
    let res = App::new().get("/", handler).test().get("/").send().await;
    assert_eq!(res.status(), 500);
}
