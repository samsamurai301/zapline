# Databases

`zapline::db` connects your app to Postgres or SQLite through [sqlx](https://docs.rs/sqlx): a connection pool created at startup, SQL migrations applied automatically, a `Db` extractor, a readiness check, and database errors that become the right HTTP status.

```toml
[dependencies]
zapline = { version = "0.1", features = ["sqlite"] }   # or "postgres", or both
sqlx = { version = "0.8", default-features = false, features = ["derive"] }   # for #[derive(FromRow)]
```

```rust,ignore
use serde::Serialize;
use zapline::db::{sqlx, Database, Db};
use zapline::prelude::*;

#[derive(Serialize, sqlx::FromRow)]
struct Post { id: i64, title: String }

#[get("/posts/{id}")]
async fn get_post(db: Db, Path(id): Path<i64>) -> Result<Json<Post>> {
    let post = sqlx::query_as::<_, Post>("SELECT id, title FROM posts WHERE id = ?")
        .bind(id)
        .fetch_one(&*db)   // no row → 404
        .await?;
    Ok(Json(post))
}

fn main() -> std::io::Result<()> {
    App::new()
        .database(Database::from_env())   // DATABASE_URL
        .mount(get_post)
        .run("0.0.0.0:8080")
}
```

## Connecting

| Constructor | Use |
|---|---|
| `Database::from_env()` | URL from `DATABASE_URL` at startup; startup fails with a hint if it's missing |
| `Database::url("sqlite://app.db?mode=rwc")` | A fixed URL |
| `Database::sqlite(url)` / `Database::postgres(url)` | Pick the backend explicitly (when both features are on) |
| `Database::sqlite_memory()` | A private in-memory database per app: ideal for tests |

Pool settings: `.max_connections(n)` (default 4 × CPUs, at least 10), `.min_connections(n)`, `.acquire_timeout(d)` (default 5 s; a request that can't get a connection in time gets a 503).

With both features enabled, `Db` and `Database` default to Postgres; use `SqliteDb` (or `Db<sqlx::Sqlite>`) and `Database::sqlite(url)` for SQLite.

## Queries

`Db` dereferences to `sqlx::Pool`, so every sqlx API works: `.fetch_one(&*db)`, `.execute(&*db)`, and `db.begin().await?` for a transaction (commit with `tx.commit().await?`; dropping it rolls back).

`?` on a `sqlx::Error` gives:

| sqlx error | HTTP |
|---|---|
| `RowNotFound` | 404 |
| unique violation | 409, code `unique_violation` |
| foreign-key violation | 409, code `foreign_key_violation` |
| not-null / check violation | 422, code `constraint_violation` |
| pool timeout | 503, code `db_pool_timeout` |
| anything else | 500 (details logged, not sent) |

## Migrations

Migrations are plain SQL files in `migrations/`, applied in file-name order, each exactly once (sqlx tracks them in `_sqlx_migrations`).

```sh
zapline db new create_posts     # migrations/20261022062406_create_posts.sql
zapline db migrate              # apply pending migrations, then exit
```

The app also applies pending migrations at startup, so `zapline dev` and deploys need no extra step. Turn that off with `Database::from_env().migrate(false)`, or read them from elsewhere with `.migrations("db/migrations")`. `zapline db migrate` runs your app with `ZAPLINE_MIGRATE_ONLY=1`, so it uses exactly the URL and settings the app uses.

> In Docker, copy `migrations/` next to the binary (the generated Dockerfiles do).

## Health

```rust,ignore
use zapline::db::DbCheck;

App::new()
    .database(Database::from_env())
    .plugin(Health::new().check("database", DbCheck::new()));   // /health/ready pings the database
```

## Testing

Each `Database::sqlite_memory()` is a fresh, empty database with your migrations applied, so tests run in parallel without stepping on each other:

```rust,ignore
fn app(db: Database) -> App {
    App::new().database(db).mount((list_posts, create_post))
}

fn main() -> std::io::Result<()> {
    app(Database::from_env()).run("0.0.0.0:8080")
}

#[tokio::test]
async fn creates_a_post() {
    let client = app(Database::sqlite_memory()).test();
    let res = client.post("/posts").json(&serde_json::json!({"title": "Hi"})).send().await;
    assert_eq!(res.status(), 201);
}
```

If your SQL is portable between SQLite and Postgres you can test against SQLite and run Postgres in production; otherwise point a test at a real Postgres with `Database::postgres(url)`.
