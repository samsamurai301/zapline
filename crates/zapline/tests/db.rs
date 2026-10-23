//! `zapline::db` with in-memory SQLite: pool, migrations, extractor, error
//! mapping and the health check.
#![cfg(all(feature = "sqlite", feature = "json"))]

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

// ---------------------------------------------------------------------------
// Migration status, rollback, redo and reset

mod day_two {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use zapline::db::sqlx::sqlite::SqlitePoolOptions;
    use zapline::db::sqlx::{self, SqlitePool};
    use zapline::db::{Database, MigrationInfo, MigrationState, Migrations, ENV_MIGRATE};
    use zapline::prelude::*;

    /// Two reversible pairs and a one-way migration with a down file added
    /// later, as `zapline db new` and older projects write them.
    fn migrations() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "20260101000000_create_notes.up.sql",
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);",
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
            (
                "20260103000000_create_audit.sql",
                "CREATE TABLE audit (id INTEGER PRIMARY KEY);",
            ),
            ("20260103000000_create_audit.down.sql", "DROP TABLE audit;"),
        ];
        for (name, sql) in files {
            std::fs::write(dir.path().join(name), sql).unwrap();
        }
        dir
    }

    async fn pool() -> SqlitePool {
        // One connection: every connection to `sqlite::memory:` is its own database.
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    async fn tables(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE '\\_%' ESCAPE '\\' AND name NOT LIKE 'sqlite%' ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn columns(pool: &SqlitePool, table: &str) -> Vec<String> {
        sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM notes")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn states(status: &[MigrationInfo]) -> Vec<MigrationState> {
        status.iter().map(|m| m.state).collect()
    }

    #[tokio::test]
    async fn status_up_down_redo_and_reset() {
        use MigrationState::*;
        let dir = migrations();
        let pool = pool().await;
        let m = Migrations::open(dir.path()).await.unwrap();

        let status = m.status(&pool).await.unwrap();
        assert_eq!(states(&status), [Pending, Pending, Pending]);
        assert_eq!(status[1].name, "add tags to notes");
        assert!(status
            .iter()
            .all(|m| m.reversible && m.applied_at.is_none()));

        let applied = m.up(&pool).await.unwrap();
        assert_eq!(applied.len(), 3);
        assert_eq!(tables(&pool).await, ["audit", "notes"]);
        let status = m.status(&pool).await.unwrap();
        assert_eq!(states(&status), [Applied, Applied, Applied]);
        let at = status[0].applied_at.as_deref().unwrap();
        assert_eq!(at.len(), "2026-01-01 00:00:00".len(), "{at}");
        assert!(m.up(&pool).await.unwrap().is_empty(), "nothing pending");

        // Down two: the one-way migration with a down file, then the column.
        let reverted = m.down(&pool, 2).await.unwrap();
        let versions: Vec<i64> = reverted.iter().map(|m| m.version).collect();
        assert_eq!(versions, [20260103000000, 20260102000000]);
        assert_eq!(tables(&pool).await, ["notes"]);
        assert_eq!(columns(&pool, "notes").await, ["id", "body"]);
        assert_eq!(
            states(&m.status(&pool).await.unwrap()),
            [Applied, Pending, Pending]
        );

        // Redo re-runs only the last applied migration; newer ones stay pending.
        sqlx::query("INSERT INTO notes (body) VALUES ('gone after redo')")
            .execute(&pool)
            .await
            .unwrap();
        let redone = m.redo(&pool).await.unwrap().unwrap();
        assert_eq!((redone.version, redone.state), (20260101000000, Applied));
        assert_eq!(
            count(&pool).await,
            0,
            "redo dropped and recreated the table"
        );
        assert_eq!(
            states(&m.status(&pool).await.unwrap()),
            [Applied, Pending, Pending]
        );

        // Reset: everything down, everything up.
        m.up(&pool).await.unwrap();
        sqlx::query("INSERT INTO notes (body, tags) VALUES ('x', 'y')")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(m.reset(&pool).await.unwrap().len(), 3);
        assert_eq!(count(&pool).await, 0);
        assert_eq!(columns(&pool, "notes").await, ["id", "body", "tags"]);

        // Down past the first migration stops at zero.
        assert_eq!(m.down(&pool, 10).await.unwrap().len(), 3);
        assert!(tables(&pool).await.is_empty());
        assert!(m.down(&pool, 1).await.unwrap().is_empty());
        assert!(m.redo(&pool).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn one_way_migrations_are_not_reverted() {
        let dir = migrations();
        std::fs::remove_file(dir.path().join("20260103000000_create_audit.down.sql")).unwrap();
        let pool = pool().await;
        let m = Migrations::open(dir.path()).await.unwrap();
        m.up(&pool).await.unwrap();
        assert!(!m.status(&pool).await.unwrap()[2].reversible);

        let err = m.down(&pool, 2).await.unwrap_err();
        assert!(
            err.message()
                .contains("20260103000000_create_audit.down.sql"),
            "{}",
            err.message()
        );
        // Nothing ran: both tables are still there.
        assert_eq!(tables(&pool).await, ["audit", "notes"]);
        assert_eq!(columns(&pool, "notes").await, ["id", "body", "tags"]);
        assert!(m.reset(&pool).await.is_err());
    }

    #[tokio::test]
    async fn status_reports_edited_and_missing_files() {
        use MigrationState::*;
        let dir = migrations();
        let pool = pool().await;
        Migrations::open(dir.path())
            .await
            .unwrap()
            .up(&pool)
            .await
            .unwrap();

        let up = dir.path().join("20260102000000_add_tags_to_notes.up.sql");
        std::fs::write(&up, "ALTER TABLE notes ADD COLUMN labels TEXT;").unwrap();
        std::fs::remove_file(dir.path().join("20260103000000_create_audit.sql")).unwrap();
        std::fs::remove_file(dir.path().join("20260103000000_create_audit.down.sql")).unwrap();

        let m = Migrations::open(dir.path()).await.unwrap();
        let status = m.status(&pool).await.unwrap();
        assert_eq!(states(&status), [Applied, ChecksumMismatch, Missing]);
        assert_eq!(status[2].name, "create audit");
        assert!(
            m.up(&pool).await.is_err(),
            "an edited migration blocks `up`"
        );
        assert!(
            m.down(&pool, 1).await.is_err(),
            "a missing file can't be reverted"
        );
    }

    #[tokio::test]
    async fn a_missing_directory_has_no_migrations() {
        let pool = pool().await;
        let m = Migrations::open("tests/fixtures/no-such-dir")
            .await
            .unwrap();
        assert!(m.status(&pool).await.unwrap().is_empty());
        assert!(m.up(&pool).await.unwrap().is_empty());
        assert!(m.down(&pool, 1).await.unwrap().is_empty());
    }

    // -- ZAPLINE_MIGRATE: the app runs the command and exits ----------------

    const CHILD_DB: &str = "ZAPLINE_TEST_CHILD_DB";
    const CHILD_DIR: &str = "ZAPLINE_TEST_CHILD_DIR";

    /// Does nothing on its own: `run_app` re-runs this test binary with
    /// only this test selected and the variables set, so the app's startup
    /// hook sees `ZAPLINE_MIGRATE` and exits the process.
    #[tokio::test]
    async fn child_app() {
        let (Ok(db), Ok(dir)) = (std::env::var(CHILD_DB), std::env::var(CHILD_DIR)) else {
            return;
        };
        let app = App::new()
            .database(Database::sqlite(format!("sqlite://{db}?mode=rwc")).migrations(dir));
        // `App::seed` works on the default backend, which is Postgres when
        // that feature is on too (db_postgres.rs covers seeding there).
        #[cfg(not(feature = "postgres"))]
        let app = app.seed(|db| async move {
            sqlx::query("INSERT INTO notes (body) VALUES ('seeded')")
                .execute(&*db)
                .await?;
            Ok(())
        });
        app.test().start().await;
        panic!("the app served instead of exiting");
    }

    fn run_app(db: &Path, dir: &Path, envs: &[(&str, &str)]) -> (bool, String) {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "day_two::child_app",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DB, db)
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

    #[test]
    fn the_app_runs_migration_commands_from_the_environment() {
        let dir = migrations();
        let db: PathBuf = dir.path().join("app.db");
        let mig = dir.path();

        let (ok, out) = run_app(&db, mig, &[(ENV_MIGRATE, "status")]);
        assert!(ok, "{out}");
        assert_eq!(out.matches("zapline-migration\t").count(), 3, "{out}");
        assert!(
            out.contains("\tpending\treversible\t-\tcreate notes"),
            "{out}"
        );
        assert!(out.contains("zapline-migrate: ok status"), "{out}");

        let (ok, out) = run_app(&db, mig, &[(ENV_MIGRATE, "up")]);
        assert!(ok, "{out}");
        assert!(out.contains("applied 20260101000000 create notes"), "{out}");
        assert!(out.contains("migrations are up to date"), "{out}");

        let (ok, out) = run_app(&db, mig, &[(ENV_MIGRATE, "down:2")]);
        assert!(ok, "{out}");
        assert!(
            out.contains("reverted 20260103000000 create audit"),
            "{out}"
        );
        assert!(
            out.contains("reverted 20260102000000 add tags to notes"),
            "{out}"
        );

        let (ok, out) = run_app(&db, mig, &[(ENV_MIGRATE, "redo")]);
        assert!(ok, "{out}");
        assert!(out.contains("redone 20260101000000 create notes"), "{out}");

        let (ok, out) = run_app(&db, mig, &[(ENV_MIGRATE, "status")]);
        assert!(ok, "{out}");
        assert_eq!(out.matches("\tapplied\t").count(), 1, "{out}");

        // Reset with the seed flag: down, up, then the app's seed data (if
        // it has any).
        let (ok, out) = run_app(&db, mig, &[(ENV_MIGRATE, "reset"), ("ZAPLINE_SEED", "1")]);
        assert!(ok, "{out}");
        assert!(out.contains("applied 3 migration(s)"), "{out}");
        assert_eq!(
            out.contains("seed data inserted"),
            cfg!(not(feature = "postgres")),
            "{out}"
        );

        let (ok, out) = run_app(&db, mig, &[(ENV_MIGRATE, "sideways")]);
        assert!(!ok);
        assert!(out.contains("not a migration command"), "{out}");
    }
}
