//! Factories on SQLite and (when `ZAPLINE_TEST_POSTGRES_URL` is set)
//! Postgres: generated values for required columns, parents for required
//! foreign keys, explicit values, typed records; and `App::seed`.
#![cfg(all(feature = "json", any(feature = "sqlite", feature = "postgres")))]

use zapline::admin::AdminBackend;
use zapline::db::sqlx::{self, Pool};
use zapline::db::Factory;

async fn exec<DB: AdminBackend>(pool: &Pool<DB>, sql: &str)
where
    for<'c> &'c Pool<DB>: sqlx::Executor<'c, Database = DB>,
{
    sqlx::raw_sql(sql).execute(pool).await.unwrap();
}

async fn count<DB: AdminBackend>(pool: &Pool<DB>, table: &str) -> i64
where
    for<'c> &'c Pool<DB>: sqlx::Executor<'c, Database = DB>,
    i64: for<'r> sqlx::Decode<'r, DB> + sqlx::Type<DB>,
    usize: sqlx::ColumnIndex<DB::Row>,
    for<'q> <DB as sqlx::Database>::Arguments<'q>: sqlx::IntoArguments<'q, DB>,
{
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn check_all<DB: AdminBackend>(pool: Pool<DB>, schema: &str)
where
    for<'c> &'c Pool<DB>: sqlx::Executor<'c, Database = DB>,
    i64: for<'r> sqlx::Decode<'r, DB> + sqlx::Type<DB>,
    usize: sqlx::ColumnIndex<DB::Row>,
    for<'q> <DB as sqlx::Database>::Arguments<'q>: sqlx::IntoArguments<'q, DB>,
{
    exec(&pool, schema).await;

    // Every required column gets a value; optional and defaulted ones don't.
    let user = Factory::new("users").create(&pool).await.unwrap();
    let email = user.get("email").unwrap();
    assert!(
        email.starts_with("user") && email.ends_with("@example.com"),
        "{email}"
    );
    assert!(user
        .get("display_name")
        .unwrap()
        .starts_with("display name "));
    assert_eq!(user.get("bio"), None);
    assert_eq!(user.get("admin"), Some("false"));
    assert_eq!(user.get("role"), Some("member"));
    assert!(user.get("created_at").unwrap().len() >= 19);
    assert!(user.int("id").unwrap() > 0);

    // Unique values each time.
    let other = Factory::new("users").create(&pool).await.unwrap();
    assert_ne!(other.get("email"), user.get("email"));

    // A required foreign key gets a parent row of its own...
    let users_before = count(&pool, "users").await;
    let post = Factory::new("posts")
        .set("title", "Hello")
        .set("published", true)
        .set("score", 4.5)
        .create(&pool)
        .await
        .unwrap();
    assert_eq!(count(&pool, "users").await, users_before + 1);
    assert_eq!(post.get("title"), Some("Hello"));
    assert_eq!(post.get("published"), Some("true"));
    assert_eq!(post.int("views"), Some(0));
    let author = post.int("author_id").unwrap();
    assert!(author > user.int("id").unwrap());

    // ...unless you set it. Optional foreign keys stay empty.
    let mine = Factory::new("posts")
        .set("author_id", user.int("id").unwrap())
        .create(&pool)
        .await
        .unwrap();
    assert_eq!(mine.int("author_id"), user.int("id"));
    assert_eq!(mine.get("editor_id"), None);
    assert_eq!(count(&pool, "users").await, users_before + 1);

    // Two levels: a comment makes a post, which makes a user.
    let comment = Factory::new("comments").create(&pool).await.unwrap();
    assert!(comment.int("post_id").is_some());
    assert_eq!(count(&pool, "users").await, users_before + 2);

    // Many at once; NULL on request.
    let many = Factory::new("posts")
        .set("author_id", user.int("id").unwrap())
        .null("score")
        .create_many(&pool, 3)
        .await
        .unwrap();
    assert_eq!(many.len(), 3);
    assert!(many.iter().all(|p| p.get("score").is_none()));
    assert_ne!(many[0].get("title"), many[1].get("title"));

    // Typed.
    let json = post.json();
    assert_eq!(json["title"], "Hello");
    assert_eq!(json["published"], true);
    assert_eq!(json["views"], 0);
    assert_eq!(json["score"], 4.5);
    assert_eq!(json["meta"], serde_json::json!({}));
    #[derive(serde::Deserialize)]
    struct Post {
        id: i64,
        title: String,
        published: bool,
    }
    let typed: Post = post.parse().unwrap();
    assert_eq!(
        (typed.id > 0, typed.title.as_str(), typed.published),
        (true, "Hello", true)
    );

    // A table with nothing required.
    let tag = Factory::new("tags").create(&pool).await.unwrap();
    assert!(tag.int("id").is_some());

    // Mistakes are errors, not panics.
    let err = Factory::new("posts")
        .set("nope", 1)
        .create(&pool)
        .await
        .unwrap_err();
    assert!(
        err.message().contains("posts has no column nope"),
        "{}",
        err.message()
    );
    let err = Factory::new("missing").create(&pool).await.unwrap_err();
    assert!(err.message().contains("no table named missing"));
    let err = Factory::new("posts; drop table users")
        .create(&pool)
        .await
        .unwrap_err();
    assert!(err.message().contains("not a table name"));
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_factories() {
    let pool: Pool<sqlx::Sqlite> = Pool::connect("sqlite::memory:").await.unwrap();
    check_all(
        pool,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, \
           display_name VARCHAR(80) NOT NULL, bio TEXT, admin BOOLEAN NOT NULL, \
           role TEXT NOT NULL DEFAULT 'member', created_at DATETIME NOT NULL);
         CREATE TABLE posts (id INTEGER PRIMARY KEY, author_id INTEGER NOT NULL REFERENCES users(id), \
           editor_id INTEGER REFERENCES users(id), title TEXT NOT NULL, published BOOLEAN NOT NULL, \
           views INTEGER NOT NULL DEFAULT 0, score REAL, meta JSON NOT NULL);
         CREATE TABLE comments (id INTEGER PRIMARY KEY, post_id INTEGER NOT NULL REFERENCES posts, \
           body TEXT NOT NULL);
         CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT);",
    )
    .await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_factories() {
    let Ok(url) = std::env::var("ZAPLINE_TEST_POSTGRES_URL") else {
        eprintln!("skipped: ZAPLINE_TEST_POSTGRES_URL is not set");
        return;
    };
    let schema = format!("factory_test_{}", std::process::id());
    let setup: Pool<sqlx::Postgres> = Pool::connect(&url).await.unwrap();
    exec(
        &setup,
        &format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};"),
    )
    .await;
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .unwrap();
    check_all(
        pool,
        "CREATE TABLE users (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL UNIQUE, \
           display_name VARCHAR(80) NOT NULL, bio TEXT, admin BOOLEAN NOT NULL, \
           role TEXT NOT NULL DEFAULT 'member', created_at TIMESTAMPTZ NOT NULL);
         CREATE TABLE posts (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
           author_id BIGINT NOT NULL REFERENCES users(id), editor_id BIGINT REFERENCES users(id), \
           title TEXT NOT NULL, published BOOLEAN NOT NULL, views INTEGER NOT NULL DEFAULT 0, \
           score DOUBLE PRECISION, meta JSONB NOT NULL);
         CREATE TABLE comments (id SERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES posts, \
           body TEXT NOT NULL);
         CREATE TABLE tags (id SERIAL PRIMARY KEY, name TEXT);",
    )
    .await;
    exec(&setup, &format!("DROP SCHEMA {schema} CASCADE")).await;
}

/// `App::seed` runs only under `zapline db seed` (ZAPLINE_SEED), which exits
/// the process; here we check that normal starts skip it.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn seed_is_skipped_on_normal_starts() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use zapline::db::Database;
    use zapline::prelude::*;

    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    let client = App::new()
        .database(Database::sqlite_memory())
        .seed(move |_db| async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .test();
    client.start().await;
    assert!(!ran.load(Ordering::SeqCst));
}
