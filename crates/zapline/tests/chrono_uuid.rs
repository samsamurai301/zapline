//! The `chrono` and `uuid` features: `#[model]` fields of those types go
//! through serde, sqlx (SQLite) and the OpenAPI schema with nothing but
//! `zapline` as a dependency, and `extract::datetime` reads both what
//! `<input type="datetime-local">` sends and RFC 3339.
#![cfg(all(
    feature = "chrono",
    feature = "uuid",
    feature = "sqlite",
    feature = "macros",
    feature = "openapi",
    feature = "validate",
    feature = "form"
))]

use serde_json::{json, Value};
use zapline::chrono::{DateTime, NaiveDate, Utc};
use zapline::db::sqlx::{self, FromRow};
use zapline::db::{Database, Factory, SqliteDb as Db};
use zapline::openapi::{spec_for, OpenApi};
use zapline::prelude::*;
use zapline::uuid::Uuid;

const SCHEMA: &str = "CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    day TEXT NOT NULL,
    starts_at TEXT,
    token BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);";

/// An event.
#[model]
#[derive(FromRow)]
struct Event {
    id: i64,
    day: NaiveDate,
    starts_at: Option<DateTime<Utc>>,
    token: Uuid,
    created_at: DateTime<Utc>,
}

#[model]
struct NewEvent {
    day: NaiveDate,
    #[serde(default, deserialize_with = "zapline::extract::optional_datetime")]
    starts_at: Option<DateTime<Utc>>,
}

async fn insert(db: &Db, new: &NewEvent) -> Result<Event> {
    Ok(sqlx::query_as::<_, Event>(
        "INSERT INTO events (day, starts_at, token) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(new.day)
    .bind(new.starts_at)
    .bind(Uuid::new_v4())
    .fetch_one(&**db)
    .await?)
}

#[post("/events", status = 201)]
async fn create(db: Db, Json(new): Json<NewEvent>) -> Result<(StatusCode, Json<Event>)> {
    Ok((StatusCode::CREATED, Json(insert(&db, &new).await?)))
}

#[post("/events/form", status = 201)]
async fn create_form(db: Db, Form(new): Form<NewEvent>) -> Result<(StatusCode, Json<Event>)> {
    Ok((StatusCode::CREATED, Json(insert(&db, &new).await?)))
}

#[get("/events/{id}")]
async fn get_one(db: Db, Path(id): Path<i64>) -> Result<Json<Event>> {
    Ok(Json(
        sqlx::query_as::<_, Event>("SELECT * FROM events WHERE id = $1")
            .bind(id)
            .fetch_one(&*db)
            .await?,
    ))
}

fn app(dir: &std::path::Path) -> App {
    std::fs::write(dir.join("1_events.sql"), SCHEMA).unwrap();
    App::new()
        .database(Database::sqlite_memory().migrations(dir))
        .mount((create, create_form, get_one))
}

#[tokio::test]
async fn dates_times_and_uuids_round_trip_through_sqlite_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let client = app(dir.path()).test();

    let res = client
        .post("/events")
        .json(&json!({"day": "2026-01-31", "starts_at": "2026-01-31T09:30:00+01:00"}))
        .send()
        .await;
    assert_eq!(res.status(), 201, "{}", res.text());
    let created: Value = res.json();
    assert_eq!(created["day"], "2026-01-31");
    assert_eq!(created["starts_at"], "2026-01-31T08:30:00Z");
    let token = created["token"].as_str().unwrap();
    assert!(Uuid::parse_str(token).is_ok(), "{created}");
    // The column default (`CURRENT_TIMESTAMP`) decodes as a UTC time.
    let created_at: DateTime<Utc> = created["created_at"].as_str().unwrap().parse().unwrap();
    assert!((Utc::now() - created_at).num_seconds().abs() < 60);

    let id = created["id"].as_i64().unwrap();
    let fetched: Value = client.get(&format!("/events/{id}")).send().await.json();
    assert_eq!(fetched, created);

    // What browsers send for `type=date` and `type=datetime-local`.
    let res = client
        .post("/events/form")
        .form(&[("day", "2026-02-01"), ("starts_at", "2026-02-01T18:45")])
        .send()
        .await;
    assert_eq!(res.status(), 201, "{}", res.text());
    assert_eq!(res.json::<Value>()["starts_at"], "2026-02-01T18:45:00Z");
    // An empty datetime-local is `None`.
    let res = client
        .post("/events/form")
        .form(&[("day", "2026-02-02"), ("starts_at", "")])
        .send()
        .await;
    assert_eq!(res.status(), 201, "{}", res.text());
    assert_eq!(res.json::<Value>()["starts_at"], Value::Null);
    // A malformed one is a 422, not a 500.
    let res = client
        .post("/events/form")
        .form(&[("day", "2026-02-02"), ("starts_at", "tomorrow")])
        .send()
        .await;
    assert_eq!(res.status(), 422, "{}", res.text());
}

#[tokio::test]
async fn factory_fills_date_and_time_columns() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("1_events.sql"),
        "CREATE TABLE days (id INTEGER PRIMARY KEY, day DATE NOT NULL, at TIMESTAMP NOT NULL);",
    )
    .unwrap();
    let client = App::new()
        .database(Database::sqlite_memory().migrations(dir.path()))
        .test();
    let pool: sqlx::Pool<sqlx::Sqlite> = client.resources().await.require().unwrap();
    let row = Factory::new("days").create(&pool).await.unwrap();
    let day: NaiveDate = row.get("day").unwrap().parse().unwrap();
    assert!(day.format("%Y").to_string().starts_with("20"));
}

#[test]
fn the_schema_documents_formats() {
    let dir = tempfile::tempdir().unwrap();
    let spec = spec_for(app(dir.path()), OpenApi::new("t", "1"));
    let event = &spec["components"]["schemas"]["Event"]["properties"];
    assert_eq!(event["day"]["format"], "date", "{event}");
    assert_eq!(event["created_at"]["format"], "date-time", "{event}");
    assert_eq!(event["token"]["format"], "uuid", "{event}");
}

#[test]
fn datetime_helpers_accept_rfc3339_and_datetime_local() {
    #[derive(serde::Deserialize)]
    struct Required {
        #[serde(deserialize_with = "zapline::extract::datetime")]
        at: DateTime<Utc>,
    }
    let parse = |v: Value| serde_json::from_value::<Required>(json!({ "at": v }));
    assert_eq!(
        parse(json!("2026-03-04T05:06:07Z"))
            .unwrap()
            .at
            .to_rfc3339(),
        "2026-03-04T05:06:07+00:00"
    );
    assert_eq!(
        parse(json!("2026-03-04T05:06")).unwrap().at.to_rfc3339(),
        "2026-03-04T05:06:00+00:00"
    );
    assert_eq!(
        parse(json!("2026-03-04 05:06:07.5"))
            .unwrap()
            .at
            .to_rfc3339(),
        "2026-03-04T05:06:07.500+00:00"
    );
    assert!(parse(json!("")).is_err());
    assert!(parse(json!("next week")).is_err());
}
