//! `ZAPLINE_MIGRATE_STATUS`: `App::database` compares `migrations/` with
//! sqlx's `_sqlx_migrations` table, prints one JSON line per migration and
//! exits without migrating or serving (what `zapline db status` reads).
//!
//! The hook exits the process, so each test re-runs this binary on itself
//! with `ZAPLINE_TEST_STATUS_CHILD` naming the scenario to set up.
#![cfg(feature = "sqlite")]

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use zapline::db::sqlx::{self, sqlite::SqlitePoolOptions};
use zapline::db::{Database, ENV_MIGRATE_STATUS};
use zapline::App;

/// Which scenario the child process sets up.
const ENV_CHILD: &str = "ZAPLINE_TEST_STATUS_CHILD";
/// The migrations directory the child works in.
const ENV_DIR: &str = "ZAPLINE_TEST_STATUS_DIR";

fn write(dir: &Path, name: &str, sql: &str) {
    std::fs::write(dir.join(name), sql).unwrap();
}

/// A named shared-cache in-memory database: a setup pool and the app's
/// pool see the same data while either keeps a connection open.
fn shared_memory_url(name: &str) -> String {
    format!(
        "sqlite:file:zapline-{name}-{}?mode=memory&cache=shared",
        std::process::id()
    )
}

/// Applies every migration in `dir` to `url` with plain sqlx, keeping the
/// returned pool (and so the in-memory database) alive.
async fn apply(url: &str, dir: &Path) -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .connect(url)
        .await
        .unwrap();
    sqlx::migrate::Migrator::new(dir.to_path_buf())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    pool
}

/// Starts an app on `db`; with the status variable set it never returns.
async fn start(db: Database<sqlx::Sqlite>) {
    App::new().database(db).test().start().await;
    unreachable!("the status hook should have exited the process");
}

/// The child half of every test below.
#[tokio::test]
async fn child() {
    let (Some(scenario), Some(dir)) = (std::env::var(ENV_CHILD).ok(), std::env::var_os(ENV_DIR))
    else {
        return;
    };
    let dir = Path::new(&dir);
    match scenario.as_str() {
        "mixed" => {
            write(
                dir,
                "1_create_notes.sql",
                "CREATE TABLE notes (id INTEGER);\n",
            );
            write(
                dir,
                "2_create_tags.sql",
                "CREATE TABLE tags (id INTEGER);\n",
            );
            let url = shared_memory_url("mixed");
            let _keep = apply(&url, dir).await;
            // Edit an applied migration and add a pending one (with a down
            // file, which is not a migration of its own).
            write(
                dir,
                "2_create_tags.sql",
                "CREATE TABLE tags (id INTEGER, name TEXT);\n",
            );
            write(
                dir,
                "3_add_body_to_notes.up.sql",
                "ALTER TABLE notes ADD COLUMN body TEXT;\n",
            );
            write(
                dir,
                "3_add_body_to_notes.down.sql",
                "ALTER TABLE notes DROP COLUMN body;\n",
            );
            write(dir, "1_create_notes.down.sql", "DROP TABLE notes;\n");
            std::fs::rename(
                dir.join("1_create_notes.sql"),
                dir.join("1_create_notes.up.sql"),
            )
            .unwrap();
            start(Database::sqlite(url).migrations(dir)).await;
        }
        "missing" => {
            write(dir, "1_one.sql", "CREATE TABLE one (id INTEGER);\n");
            write(dir, "2_two.sql", "CREATE TABLE two (id INTEGER);\n");
            let url = shared_memory_url("missing");
            let _keep = apply(&url, dir).await;
            std::fs::remove_file(dir.join("2_two.sql")).unwrap();
            start(Database::sqlite(url).migrations(dir)).await;
        }
        "fresh" => {
            write(
                dir,
                "1_create_notes.sql",
                "CREATE TABLE notes (id INTEGER);\n",
            );
            start(Database::sqlite_memory().migrations(dir)).await;
        }
        other => panic!("unknown scenario {other}"),
    }
}

/// Runs `scenario` in a child process and returns its stdout and JSON lines.
fn status_of(scenario: &str) -> (String, Vec<Value>) {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["child", "--exact", "--nocapture", "--test-threads=1"])
        .env(ENV_CHILD, scenario)
        .env(ENV_DIR, tmp.path())
        .env(ENV_MIGRATE_STATUS, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The process exited inside the hook, before the harness could report.
    assert!(!stdout.contains("test result"), "{stdout}");
    // libtest prints `test child ... ` without a newline before the output.
    let lines = stdout
        .lines()
        .filter_map(|l| l.find("{\"version\"").map(|i| &l[i..]))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (stdout, lines)
}

/// `(version, description, applied, checksum_ok, missing)` per line.
fn rows(lines: &[Value]) -> Vec<(i64, String, bool, bool, bool)> {
    lines
        .iter()
        .map(|l| {
            (
                l["version"].as_i64().unwrap(),
                l["description"].as_str().unwrap().to_string(),
                l["applied"].as_bool().unwrap(),
                l["checksum_ok"].as_bool().unwrap(),
                l["missing"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[test]
fn status_reports_applied_pending_and_modified() {
    let (stdout, lines) = status_of("mixed");
    assert!(
        stdout.contains("migration status: 3 migrations"),
        "{stdout}"
    );
    assert_eq!(
        rows(&lines),
        [
            (1, "create notes".into(), true, true, false),
            (2, "create tags".into(), true, false, false),
            (3, "add body to notes".into(), false, true, false),
        ]
    );
}

#[test]
fn status_on_a_fresh_database_lists_everything_as_pending() {
    let (stdout, lines) = status_of("fresh");
    assert!(
        stdout.contains("migration status: 1 migrations"),
        "{stdout}"
    );
    assert_eq!(
        rows(&lines),
        [(1, "create notes".into(), false, true, false)]
    );
}

#[test]
fn an_applied_migration_whose_file_is_gone_is_reported_missing() {
    let (_, lines) = status_of("missing");
    assert_eq!(
        rows(&lines),
        [
            (1, "one".into(), true, true, false),
            (2, String::new(), true, true, true),
        ]
    );
}
