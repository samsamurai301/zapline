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

## List endpoints: paging, sorting, filters, search

`Listing` turns the query string into a safe, paged query. Callers get pagination, sorting, filters and search; you write one expression:

```rust
use zapline::db::{sqlx, Db, ListParams, Listing, Paged};

#[get("/api/posts")]
async fn list(db: Db, params: ListParams) -> Result<Json<Paged<Post>>> {
    let page = Listing::new("posts", "id, title, published, author_id")
        .sortable(["title", "published"])
        .search(["title", "body"])
        .filter_bool("published")
        .filter_int("author_id")
        .fetch(&db, &params)
        .await?;
    Ok(Json(page))
}
```

`GET /api/posts?q=rust&published=true&sort=-id&limit=20&offset=40` returns:

```json
{"items": [...], "total": 57, "limit": 20, "offset": 40}
```

| Query | Meaning |
|---|---|
| `limit`, `offset` | Page size (default 50, capped at 200; change with `.page_size(default, max)`) and start. |
| `sort=title`, `sort=-title`, `sort=-done,title` | Ascending, descending, several keys. Only `.sortable(..)` columns and the key (`id`) are allowed. Ties are broken by the key, so pages are stable. |
| `q=words` | Case-insensitive substring match over the `.search(..)` columns. `%` and `_` match themselves. |
| `published=true`, `author_id=7`, `status=paid` | Equality filters declared with `.filter_bool`, `.filter_int` and `.filter_text`. |

Nothing from the request reaches the SQL text: values are bound as parameters, and sort columns must be on the allow-list. Bad input is a `400` with code `invalid_sort`, `invalid_filter` or `invalid_page`, and the message lists what's allowed.

For per-user or per-tenant lists, `.scope("user_id", user.id)` adds a condition the request can't remove. `zapline generate resource` writes a `listing()` like this for every resource, with sorting on every column but long text, search on the text fields and a filter per string, number, boolean and reference field; a reference (`author:references`) also gets a nested list, `GET /api/authors/{id}/posts`, which is `listing().scope("author_id", id)`. It works on SQLite and Postgres, and `ListParams` documents `limit`, `offset`, `sort` and `q` in the OpenAPI spec.

## Migrations

Migrations are plain SQL files in `migrations/`, applied in file-name order, each exactly once (sqlx tracks them in `_sqlx_migrations`).

```sh
zapline db new create_posts     # migrations/20261022062406_create_posts.sql
zapline db migrate              # apply pending migrations, then exit
```

`zapline g migration` writes the SQL too, from the name and `field:type` arguments ([CLI](cli.md#migrations-from-the-command-line)):

```sh
zapline g migration add_published_at_to_posts published_at:datetime?   # ALTER TABLE posts ADD COLUMN ...
zapline g migration create_tags name slug                             # CREATE TABLE tags (...)
```

The app also applies pending migrations at startup, so `zapline dev` and deploys need no extra step. Turn that off with `Database::from_env().migrate(false)`, or read them from elsewhere with `.migrations("db/migrations")`. `.migrations(zapline::embed!("migrations"))` compiles them into the binary, with the same checksums as the files ([One binary](deployment.md#one-binary-one-command)). `zapline db migrate` runs your app with `ZAPLINE_MIGRATE_ONLY=1`, so it uses exactly the URL and settings the app uses.

> In Docker, copy `migrations/` next to the binary (the generated Dockerfiles do).

## Dates, times and UUIDs

Turn on zapline's `chrono` and `uuid` features and use `zapline::chrono` and `zapline::uuid` in models: sqlx, serde and the OpenAPI schema (`format: date`, `date-time`, `uuid`) all understand them, with no extra dependencies. `zapline g resource` turns the features on when its fields need them.

```rust,ignore
use zapline::chrono::{DateTime, NaiveDate, Utc};
use zapline::uuid::Uuid;

#[model]
#[derive(sqlx::FromRow)]
pub struct Event {
    pub id: i64,
    pub day: NaiveDate,             // DATE on Postgres, TEXT ("2026-01-31") on SQLite
    pub starts_at: DateTime<Utc>,   // TIMESTAMPTZ / TEXT
    pub token: Uuid,                // UUID / BLOB
    pub created_at: DateTime<Utc>,  // DEFAULT CURRENT_TIMESTAMP works on both
}

#[model]
pub struct NewEvent {
    pub day: NaiveDate,
    // Accepts RFC 3339 and what <input type="datetime-local"> sends ("2026-01-31T09:30", as UTC).
    #[serde(deserialize_with = "zapline::extract::datetime")]
    pub starts_at: DateTime<Utc>,
}
```

`zapline::extract::optional_datetime` is the same for an `Option<DateTime<Utc>>` field (empty means `None`).

## Seed data

Demo or development data goes in `App::seed`, after `.database(..)`:

```rust,ignore
use zapline::db::{Database, Factory};

App::new()
    .database(Database::from_env())
    .seed(|db| async move {
        Factory::new("todos").set("title", "Try zapline").create(&db).await?;
        Factory::new("todos").set("done", true).create_many(&db, 5).await?;
        Ok(())
    })
```

```sh
zapline db seed     # migrate, run the seed once, exit
```

Normal runs and tests never run it. `zapline db seed` starts the app with `ZAPLINE_SEED=1`, like `db migrate`, so it uses the app's own `DATABASE_URL`. Running it twice inserts the rows twice. The `fullstack` template comes with a seed.

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

### Test data with `Factory`

`Factory` inserts a row and fills in every column you don't set that the database requires (`NOT NULL`, no default), read from the schema: unique numbers, text like `title 7`, `user7@example.com` for email columns, `false`, the current time, `{}` for JSON. A required foreign key gets a parent row, made the same way. Set only what the test is about:

```rust,ignore
use zapline::db::{sqlx, DefaultDb, Factory};

#[tokio::test]
async fn lists_only_published_posts() {
    let client = app(Database::sqlite_memory()).test();
    let pool: sqlx::Pool<DefaultDb> = client.resources().await.require().unwrap();

    Factory::new("posts").set("published", true).create_many(&pool, 2).await.unwrap();
    Factory::new("posts").set("published", false).create(&pool).await.unwrap();   // its author is created too

    let posts: Vec<serde_json::Value> = client.get("/posts").send().await.json();
    assert_eq!(posts.len(), 2);
}
```

`create` returns a `Record`: `get("title")` as text, `int("id")`, `json()` with typed numbers and booleans (compare it with your API's response), or `parse::<Post>()` into your own type. `.null("column")` stores `NULL`. It works on SQLite and Postgres, in tests and in seeds.

If your SQL is portable between SQLite and Postgres you can test against SQLite and run Postgres in production; otherwise point a test at a real Postgres with `Database::postgres(url)`.

## Going further

[Ecosystem](ecosystem.md) has complete, tested apps for transactions, savepoints and idempotency keys with sqlx, and for SeaORM, Diesel and Leptos.
