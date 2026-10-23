//! JSON columns: a `#[model]` field of type `JsonValue` goes through sqlx
//! (SQLite `TEXT`) and serde, and `extract::json_text` lets one struct take
//! a JSON body or a form whose `<textarea>` holds JSON.
#![cfg(all(
    feature = "sqlite",
    feature = "macros",
    feature = "openapi",
    feature = "validate",
    feature = "form",
    feature = "json"
))]

use serde_json::{json, Value};
use zapline::db::sqlx::types::JsonValue;
use zapline::db::sqlx::{self, FromRow};
use zapline::db::{Database, SqliteDb as Db};
use zapline::prelude::*;

const SCHEMA: &str = "CREATE TABLE notes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    meta TEXT NOT NULL,
    extra TEXT
);";

/// A note.
#[model]
#[derive(FromRow)]
struct Note {
    id: i64,
    meta: JsonValue,
    extra: Option<JsonValue>,
}

#[model]
struct NewNote {
    #[serde(deserialize_with = "zapline::extract::json_text")]
    meta: JsonValue,
    #[serde(default, deserialize_with = "zapline::extract::json_text")]
    extra: Option<JsonValue>,
}

async fn insert(db: &Db, new: &NewNote) -> Result<Note> {
    Ok(sqlx::query_as::<_, Note>(
        "INSERT INTO notes (meta, extra) VALUES ($1, $2) RETURNING id, meta, extra",
    )
    .bind(&new.meta)
    .bind(&new.extra)
    .fetch_one(&**db)
    .await?)
}

#[post("/notes", status = 201)]
async fn create(db: Db, Json(new): Json<NewNote>) -> Result<(StatusCode, Json<Note>)> {
    Ok((StatusCode::CREATED, Json(insert(&db, &new).await?)))
}

#[post("/notes/form", status = 201)]
async fn create_form(db: Db, Form(new): Form<NewNote>) -> Result<(StatusCode, Json<Note>)> {
    Ok((StatusCode::CREATED, Json(insert(&db, &new).await?)))
}

fn app(dir: &std::path::Path) -> App {
    std::fs::write(dir.join("1_notes.sql"), SCHEMA).unwrap();
    App::new()
        .database(Database::sqlite_memory().migrations(dir))
        .mount((create, create_form))
}

#[tokio::test]
async fn json_values_round_trip_through_sqlite_from_json_and_forms() {
    let dir = tempfile::tempdir().unwrap();
    let client = app(dir.path()).test();

    let meta = json!({"tags": ["a", "b"], "n": 1, "none": null});
    let res = client
        .post("/notes")
        .json(&json!({ "meta": meta }))
        .send()
        .await;
    assert_eq!(res.status(), 201, "{}", res.text());
    let created: Value = res.json();
    assert_eq!(created["meta"], meta);
    assert_eq!(created["extra"], Value::Null);

    // A textarea sends the JSON as text; an empty one is `None`.
    let res = client
        .post("/notes/form")
        .form(&[("meta", r#"{"tags": ["c"]}"#), ("extra", "")])
        .send()
        .await;
    assert_eq!(res.status(), 201, "{}", res.text());
    let created: Value = res.json();
    assert_eq!(created["meta"], json!({"tags": ["c"]}));
    assert_eq!(created["extra"], Value::Null);
    let res = client
        .post("/notes/form")
        .form(&[("meta", "[1, 2]"), ("extra", "plain text")])
        .send()
        .await;
    assert_eq!(res.status(), 201, "{}", res.text());
    assert_eq!(res.json::<Value>()["extra"], "plain text");

    // Malformed JSON is a 422, not a 500.
    let res = client
        .post("/notes/form")
        .form(&[("meta", "{not json")])
        .send()
        .await;
    assert_eq!(res.status(), 422, "{}", res.text());
    assert!(res.text().contains("not valid JSON"), "{}", res.text());
}
