//! The admin panel on SQLite and (when `ZAPLINE_TEST_POSTGRES_URL` is set)
//! Postgres: schema discovery, browsing, search, sorting, paging, forms.
#![cfg(any(feature = "sqlite", feature = "postgres"))]

use zapline::admin::AdminBackend;
use zapline::db::sqlx::{self, Pool};
use zapline::middleware::Next;
use zapline::prelude::*;
use zapline::test::TestClient;

async fn exec<DB: AdminBackend>(pool: &Pool<DB>, sql: &str)
where
    for<'c> &'c Pool<DB>: sqlx::Executor<'c, Database = DB>,
{
    sqlx::raw_sql(sql).execute(pool).await.unwrap();
}

fn client<DB: AdminBackend>(pool: Pool<DB>, configure: fn(Admin) -> Admin) -> TestClient {
    App::new().state(pool).admin_in::<DB>(configure).test()
}

use zapline::admin::Admin;

/// A form post as a browser sends it (same origin).
async fn post(client: &TestClient, uri: &str, form: &str) -> zapline::test::TestResponse {
    client
        .post(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("host", "localhost")
        .header("origin", "http://localhost")
        .body(form.to_owned())
        .send()
        .await
}

async fn check_all<DB: AdminBackend>(pool: Pool<DB>, schema: &str)
where
    for<'c> &'c Pool<DB>: sqlx::Executor<'c, Database = DB>,
    String: for<'r> sqlx::Decode<'r, DB> + sqlx::Type<DB>,
    usize: sqlx::ColumnIndex<DB::Row>,
    for<'q> <DB as sqlx::Database>::Arguments<'q>: sqlx::IntoArguments<'q, DB>,
{
    exec(&pool, schema).await;
    let mut seed = String::new();
    for i in 1..=60 {
        seed.push_str(&format!(
            "INSERT INTO posts (title, body, views) VALUES ('Post {i:02}', {}, {i});",
            if i % 2 == 0 { "'even body'" } else { "NULL" }
        ));
    }
    exec(&pool, &seed).await;
    exec(
        &pool,
        "INSERT INTO users (email, password_hash) VALUES ('ada@example.com', 'argon2-secret');",
    )
    .await;

    let admin = client(pool.clone(), |a| a);

    // The index lists the app's tables with row counts, not the framework's.
    let res = admin.get("/admin").send().await;
    res.assert_status(200);
    let page = res.text();
    for table in ["posts", "users", "tags"] {
        assert!(
            page.contains(&format!("href=\"/admin/{table}\"")),
            "{table}: {page}"
        );
    }
    assert!(!page.contains("_sqlx_migrations"), "{page}");
    assert!(page.contains("<td>60</tr>"), "{page}");

    // Newest first, 50 per page, with a next link; NULL shown as null.
    let page = admin.get("/admin/posts").send().await.text();
    assert!(page.contains("60 rows"), "{page}");
    assert!(page.find("Post 60").unwrap() < page.find("Post 59").unwrap());
    assert!(!page.contains("Post 10<"), "{page}");
    assert!(page.contains("offset=50"), "{page}");
    assert!(page.contains("<span class=dim>null</span>"));
    let page = admin.get("/admin/posts?offset=50").send().await.text();
    assert!(page.contains("Post 10") && page.contains("Post 01") && page.contains("Previous"));

    // Sorting by a column, and search (case-insensitive, LIKE wildcards literal).
    let page = admin.get("/admin/posts?sort=title").send().await.text();
    assert!(page.find("Post 01").unwrap() < page.find("Post 02").unwrap());
    let page = admin.get("/admin/posts?sort=nope;DROP").send().await.text();
    assert!(page.contains("Post 60"));
    let page = admin.get("/admin/posts?q=POST+0").send().await.text();
    assert!(page.contains("9 rows matching"), "{page}");
    let page = admin.get("/admin/posts?q=%25").send().await.text();
    assert!(page.contains("0 rows"), "{page}");
    let page = admin.get("/admin/posts?q=42").send().await.text();
    assert!(page.contains("1 row matching"), "{page}");

    // Create: empty optional fields become NULL or the column default.
    let res = post(
        &admin,
        "/admin/posts",
        "title=Hello+%26+welcome&body=&views=&score=",
    )
    .await;
    res.assert_status(303);
    assert_eq!(res.header("location"), Some("/admin/posts"));
    let page = admin.get("/admin/posts?q=welcome").send().await.text();
    assert!(page.contains("Hello &amp; welcome"), "{page}");
    assert!(page.contains("href=\"/admin/posts/61\""), "{page}");

    // Edit: the form shows the row; saving updates it.
    let page = admin.get("/admin/posts/61").send().await.text();
    assert!(page.contains("value=\"Hello &amp; welcome\""), "{page}");
    assert!(page.contains("value=\"0\""), "{page}");
    post(
        &admin,
        "/admin/posts/61",
        "title=Edited&body=Some+text&views=7&score=1.5",
    )
    .await
    .assert_status(303);
    let page = admin.get("/admin/posts/61").send().await.text();
    assert!(
        page.contains("value=\"Edited\"")
            && page.contains("value=\"7\"")
            && page.contains("Some text")
    );

    // A database error shows the form again with the message and the input.
    let res = post(&admin, "/admin/posts/61", "title=Edited&views=lots").await;
    res.assert_status(422);
    assert!(
        res.text().contains("role=alert") && res.text().contains("value=\"lots\""),
        "{}",
        res.text()
    );

    // Secrets are never shown or written; booleans are checkboxes.
    let page = admin.get("/admin/users/1").send().await.text();
    assert!(
        !page.contains("password_hash") && !page.contains("argon2-secret"),
        "{page}"
    );
    assert!(page.contains("type=checkbox name=is_admin"), "{page}");
    post(
        &admin,
        "/admin/users/1",
        "email=ada%40example.com&is_admin=on",
    )
    .await
    .assert_status(303);
    let page = admin.get("/admin/users").send().await.text();
    assert!(page.contains("✓"), "{page}");
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hash, "argon2-secret");

    // Unique violations come back as a form error, not a crash.
    exec(
        &pool,
        "INSERT INTO users (email, password_hash) VALUES ('bob@example.com', 'x');",
    )
    .await;
    let res = post(&admin, "/admin/users/2", "email=ada%40example.com").await;
    res.assert_status(422);

    // Delete.
    post(&admin, "/admin/posts/61/delete", "")
        .await
        .assert_status(303);
    admin.get("/admin/posts/61").send().await.assert_status(404);

    // No single-column primary key: browse only.
    exec(
        &pool,
        "INSERT INTO tags (post_id, name) VALUES (1, 'rust');",
    )
    .await;
    let page = admin.get("/admin/tags").send().await.text();
    assert!(page.contains("rust") && !page.contains("New row"), "{page}");
    post(&admin, "/admin/tags", "post_id=2&name=web")
        .await
        .assert_status(404);

    // Unknown tables, framework tables and cross-site posts are refused.
    admin.get("/admin/nope").send().await.assert_status(404);
    admin
        .get("/admin/_sqlx_migrations")
        .send()
        .await
        .assert_status(404);
    admin
        .post("/admin/posts/1/delete")
        .header("host", "localhost")
        .header("origin", "https://evil.example")
        .send()
        .await
        .assert_status(403);

    // Other machines can't see an unprotected admin.
    admin
        .get("/admin")
        .remote_addr("203.0.113.9:4000".parse().unwrap())
        .send()
        .await
        .assert_status(404);

    // Read-only, excluded tables and hidden columns.
    let ro = client(pool.clone(), |a| {
        a.read_only().exclude(["tags"]).hide(["posts.body"])
    });
    let page = ro.get("/admin/posts/1").send().await.text();
    assert!(
        !page.contains("Save") && !page.contains("Delete") && !page.contains("even body"),
        "{page}"
    );
    assert!(!ro.get("/admin").send().await.text().contains("/admin/tags"));
    ro.get("/admin/tags").send().await.assert_status(404);
    assert_ne!(post(&ro, "/admin/posts/1/delete", "").await.status(), 303);
}

async fn admins_only(req: Request, next: Next) -> Response {
    if req.headers().get("x-admin").is_none() {
        return Error::forbidden().into_response();
    }
    next.run(req).await
}

#[cfg(feature = "sqlite")]
async fn sqlite() -> Pool<sqlx::Sqlite> {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap()
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_admin() {
    check_all(
        sqlite().await,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, body TEXT, \
           views INTEGER NOT NULL DEFAULT 0, score REAL);
         CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, \
           password_hash TEXT NOT NULL, is_admin BOOLEAN NOT NULL DEFAULT FALSE, \
           created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
         CREATE TABLE tags (post_id INTEGER NOT NULL, name TEXT NOT NULL, PRIMARY KEY (post_id, name));
         CREATE TABLE _sqlx_migrations (version INTEGER PRIMARY KEY);",
    )
    .await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn protected_admin_runs_the_guard_first() {
    let pool = sqlite().await;
    let admin = App::new()
        .state(pool)
        .admin_in::<sqlx::Sqlite>(|a| a.protect(admins_only).path("/backoffice/"))
        .test();
    admin.get("/backoffice").send().await.assert_status(403);
    admin
        .get("/backoffice")
        .remote_addr("203.0.113.9:4000".parse().unwrap())
        .header("x-admin", "yes")
        .send()
        .await
        .assert_status(200)
        .assert_contains("No tables yet");
    admin.get("/admin").send().await.assert_status(404);
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn the_last_admin_call_wins() {
    let admin = App::new()
        .state(sqlite().await)
        .admin_in::<sqlx::Sqlite>(|a| a)
        .admin_in::<sqlx::Sqlite>(|a| a.protect(admins_only))
        .test();
    admin.get("/admin").send().await.assert_status(403);
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_admin() {
    let Ok(url) = std::env::var("ZAPLINE_TEST_POSTGRES_URL") else {
        eprintln!("skipped: ZAPLINE_TEST_POSTGRES_URL is not set");
        return;
    };
    let schema = format!("admin_test_{}", std::process::id());
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
        "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, body TEXT, \
           views INTEGER NOT NULL DEFAULT 0, score DOUBLE PRECISION);
         CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
           email VARCHAR(255) NOT NULL UNIQUE, password_hash TEXT NOT NULL, \
           is_admin BOOLEAN NOT NULL DEFAULT FALSE, created_at TIMESTAMPTZ NOT NULL DEFAULT now());
         CREATE TABLE tags (post_id BIGINT NOT NULL, name TEXT NOT NULL, PRIMARY KEY (post_id, name));
         CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY);",
    )
    .await;
    exec(&setup, &format!("DROP SCHEMA {schema} CASCADE")).await;
}
