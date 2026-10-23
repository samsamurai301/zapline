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

// ---------------------------------------------------------------------------
// Migration status, rollback, redo and reset, each in a database of its own

mod day_two {
    use std::path::Path;
    use std::process::Command;

    use zapline::db::sqlx::{self, PgPool};
    use zapline::db::{Database, MigrationState, Migrations, ENV_MIGRATE};
    use zapline::prelude::*;

    /// A new empty database on the test server, dropped by `drop_database`.
    pub(super) async fn create_database(base: &str) -> (PgPool, String, String) {
        let admin = PgPool::connect(base).await.unwrap();
        let name = format!("zl_migrate_{}", super::unique("db").replace('-', "_"));
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .unwrap();
        let (prefix, _) = base.split('?').next().unwrap().rsplit_once('/').unwrap();
        (admin, name.clone(), format!("{prefix}/{name}"))
    }

    pub(super) async fn drop_database(admin: &PgPool, name: &str) {
        sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .execute(admin)
            .await
            .unwrap();
    }

    fn migrations() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "20260101000000_create_notes.up.sql",
                "CREATE TABLE notes (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL);",
            ),
            ("20260101000000_create_notes.down.sql", "DROP TABLE notes;"),
            (
                "20260102000000_add_tags_to_notes.up.sql",
                "ALTER TABLE notes ADD COLUMN tags TEXT;",
            ),
            (
                "20260102000000_add_tags_to_notes.down.sql",
                "ALTER TABLE notes DROP COLUMN tags;",
            ),
        ];
        for (name, sql) in files {
            std::fs::write(dir.path().join(name), sql).unwrap();
        }
        dir
    }

    async fn columns(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_name = 'notes' AND table_schema = current_schema() ORDER BY ordinal_position",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn postgres_status_up_down_redo_and_reset() {
        use MigrationState::*;
        let Some(base) = super::url() else {
            return;
        };
        let (admin, name, url) = create_database(&base).await;
        let pool = PgPool::connect(&url).await.unwrap();
        let dir = migrations();
        let m = Migrations::open(dir.path()).await.unwrap();

        let states =
            |s: Vec<zapline::db::MigrationInfo>| s.into_iter().map(|m| m.state).collect::<Vec<_>>();
        assert_eq!(states(m.status(&pool).await.unwrap()), [Pending, Pending]);
        assert_eq!(m.up(&pool).await.unwrap().len(), 2);
        let status = m.status(&pool).await.unwrap();
        assert_eq!(
            status.iter().map(|m| m.state).collect::<Vec<_>>(),
            [Applied, Applied]
        );
        assert_eq!(status[0].applied_at.as_deref().map(str::len), Some(19));
        assert_eq!(columns(&pool).await, ["id", "body", "tags"]);

        let reverted = m.down(&pool, 1).await.unwrap();
        assert_eq!(reverted[0].version, 20260102000000);
        assert_eq!(columns(&pool).await, ["id", "body"]);

        sqlx::query("INSERT INTO notes (body) VALUES ('x')")
            .execute(&pool)
            .await
            .unwrap();
        m.redo(&pool).await.unwrap().unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(states(m.status(&pool).await.unwrap()), [Applied, Pending]);

        assert_eq!(m.reset(&pool).await.unwrap().len(), 2);
        assert_eq!(columns(&pool).await, ["id", "body", "tags"]);
        assert_eq!(m.down(&pool, 5).await.unwrap().len(), 2);
        assert!(columns(&pool).await.is_empty());

        pool.close().await;
        drop_database(&admin, &name).await;
    }

    const CHILD_URL: &str = "ZAPLINE_TEST_CHILD_PG_URL";
    const CHILD_DIR: &str = "ZAPLINE_TEST_CHILD_DIR";

    /// Re-run by `run_app` in a child process (see tests/db.rs).
    #[tokio::test]
    async fn child_app() {
        let (Ok(url), Ok(dir)) = (std::env::var(CHILD_URL), std::env::var(CHILD_DIR)) else {
            return;
        };
        App::new()
            .database(Database::postgres(url).migrations(dir))
            .seed(|db| async move {
                sqlx::query("INSERT INTO notes (body) VALUES ('seeded')")
                    .execute(&*db)
                    .await?;
                Ok(())
            })
            .test()
            .start()
            .await;
        panic!("the app served instead of exiting");
    }

    fn run_app(url: &str, dir: &Path, envs: &[(&str, &str)]) -> (bool, String) {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "day_two::child_app", "--nocapture"])
            .env(CHILD_URL, url)
            .env(CHILD_DIR, dir)
            .env_remove("ZAPLINE_MIGRATE_ONLY")
            .env_remove("ZAPLINE_SEED");
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    #[tokio::test]
    async fn postgres_app_migrates_resets_and_seeds_from_the_environment() {
        let Some(base) = super::url() else {
            return;
        };
        let (admin, name, url) = create_database(&base).await;
        let dir = migrations();

        let (ok, out) = run_app(&url, dir.path(), &[(ENV_MIGRATE, "up")]);
        assert!(ok, "{out}");
        assert!(
            out.contains("applied 20260102000000 add tags to notes"),
            "{out}"
        );

        let (ok, out) = run_app(&url, dir.path(), &[(ENV_MIGRATE, "down:1")]);
        assert!(ok, "{out}");
        let (ok, out) = run_app(&url, dir.path(), &[(ENV_MIGRATE, "status")]);
        assert!(ok, "{out}");
        assert!(out.contains("\tapplied\treversible\t"), "{out}");
        assert!(
            out.contains("\tpending\treversible\t-\tadd tags to notes"),
            "{out}"
        );

        let (ok, out) = run_app(
            &url,
            dir.path(),
            &[(ENV_MIGRATE, "reset"), ("ZAPLINE_SEED", "1")],
        );
        assert!(ok, "{out}");
        assert!(out.contains("seed data inserted"), "{out}");
        let pool = PgPool::connect(&url).await.unwrap();
        let bodies: Vec<String> = sqlx::query_scalar("SELECT body FROM notes")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(bodies, ["seeded"]);

        pool.close().await;
        drop_database(&admin, &name).await;
    }
}

// ---------------------------------------------------------------------------
// What `zapline g resource` generates for Postgres: JSONB columns bound as
// `JsonValue`, a unique index whose name `write_error` matches, foreign-key
// violations it turns into `<parent>_not_found`, and a reference to the
// table itself.

mod generated_resource {
    use serde_json::json;
    use zapline::db::sqlx::types::JsonValue;
    use zapline::db::sqlx::{self, PgPool};

    fn database_error(err: sqlx::Error) -> Box<dyn sqlx::error::DatabaseError> {
        match err {
            sqlx::Error::Database(e) => e,
            other => panic!("not a database error: {other}"),
        }
    }

    #[tokio::test]
    async fn jsonb_unique_index_and_foreign_keys_behave_as_generated_code_expects() {
        let Some(base) = super::url() else {
            return;
        };
        let (admin, name, url) = super::day_two::create_database(&base).await;
        let pool = PgPool::connect(&url).await.unwrap();
        // The SQL `zapline g resource product name` and `zapline g resource
        // review slug:string:unique product:references details:json
        // rating:int:index reply_to:ref:reviews?` write.
        for sql in [
            "CREATE TABLE products (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL)",
            "CREATE TABLE reviews (
                id BIGSERIAL PRIMARY KEY,
                slug TEXT NOT NULL,
                product_id BIGINT NOT NULL REFERENCES products(id) ON DELETE CASCADE,
                details JSONB NOT NULL,
                rating BIGINT NOT NULL,
                reply_to_id BIGINT REFERENCES reviews(id) ON DELETE SET NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
            "CREATE UNIQUE INDEX reviews_slug_key ON reviews (slug)",
            "CREATE INDEX reviews_product_id_idx ON reviews (product_id)",
            "CREATE INDEX reviews_rating_idx ON reviews (rating)",
            "CREATE INDEX reviews_reply_to_id_idx ON reviews (reply_to_id)",
            "INSERT INTO products (name) VALUES ('p')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let insert = |slug: &'static str, product: i64, reply_to: Option<i64>| {
            let pool = pool.clone();
            async move {
                sqlx::query_as::<_, (i64, JsonValue)>(
                    "INSERT INTO reviews (slug, product_id, details, rating, reply_to_id) \
                     VALUES ($1, $2, $3, $4, $5) RETURNING id, details",
                )
                .bind(slug)
                .bind(product)
                .bind(json!({"tags": ["a", "b"], "n": 1}))
                .bind(5_i64)
                .bind(reply_to)
                .fetch_one(&pool)
                .await
            }
        };
        let (first, details) = insert("a", 1, None).await.unwrap();
        assert_eq!(details, json!({"tags": ["a", "b"], "n": 1}));
        insert("b", 1, Some(first)).await.unwrap();

        // PATCH: `COALESCE($1, details)` keeps the value for `None`.
        let patch = |details: Option<JsonValue>| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, JsonValue>(
                    "UPDATE reviews SET details = COALESCE($1, details), \
                     updated_at = CURRENT_TIMESTAMP WHERE id = $2 RETURNING details",
                )
                .bind(&details)
                .bind(first)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(patch(None).await, json!({"tags": ["a", "b"], "n": 1}));
        assert_eq!(patch(Some(json!({"n": 2}))).await, json!({"n": 2}));

        let e = database_error(insert("a", 1, None).await.unwrap_err());
        assert!(e.is_unique_violation());
        assert_eq!(e.constraint(), Some("reviews_slug_key"));
        let e = database_error(insert("c", 999_999, None).await.unwrap_err());
        assert!(e.is_foreign_key_violation());
        let e = database_error(insert("d", 1, Some(999_999)).await.unwrap_err());
        assert!(e.is_foreign_key_violation());
        // What `missing` asks to tell which parent it was.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM products WHERE id = $1")
            .bind(999_999_i64)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);

        pool.close().await;
        super::day_two::drop_database(&admin, &name).await;
    }
}
