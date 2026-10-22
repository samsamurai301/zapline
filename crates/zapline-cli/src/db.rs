//! `zapline db`: create and apply SQL migrations.
//!
//! Migrations are plain SQL files in `migrations/`, named
//! `<UTC timestamp>_<description>.sql` (the sqlx convention). `zapline db
//! migrate` doesn't connect to the database itself: it runs the app with
//! `ZAPLINE_MIGRATE_ONLY=1`, so `App::database` applies the migrations with
//! the app's own URL and settings and exits before serving.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::build;
use crate::naming;
use crate::project::Project;
use crate::ui;

/// Environment variable that makes `App::database` migrate and exit.
pub const ENV_MIGRATE_ONLY: &str = "ZAPLINE_MIGRATE_ONLY";

/// Where migrations live, relative to the project root.
pub const MIGRATIONS_DIR: &str = "migrations";

/// How long the app may take to migrate once built.
const RUN_TIMEOUT: Duration = Duration::from_secs(300);

/// `YYYYMMDDHHMMSS` in UTC for `secs` since the Unix epoch.
pub fn timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian
/// (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The file name for a new migration called `name` created at `secs`.
pub fn migration_file_name(name: &str, secs: u64) -> Result<String> {
    let slug = naming::snake_case(name);
    if slug.is_empty() {
        bail!("`{name}` is not a usable migration name; try something like `create_posts`");
    }
    Ok(format!("{}_{slug}.sql", timestamp(secs)))
}

/// Creates `migrations/<timestamp>_<name>.sql` under `root` with `sql` as
/// its contents and returns its path.
pub fn create_migration(root: &Path, name: &str, sql: &str) -> Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let dir = root.join(MIGRATIONS_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    // Two migrations created in the same second must still sort in order.
    let mut secs = now;
    let mut path = dir.join(migration_file_name(name, secs)?);
    while path.exists() || same_version_exists(&dir, secs) {
        secs += 1;
        path = dir.join(migration_file_name(name, secs)?);
    }
    std::fs::write(&path, sql).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

fn same_version_exists(dir: &Path, secs: u64) -> bool {
    let prefix = format!("{}_", timestamp(secs));
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
    })
}

/// `zapline db new <name>`.
pub fn new(root: &Path, name: &str) -> Result<()> {
    let sql = format!("-- {name}\n-- Applied once, in file-name order, by `zapline db migrate` or at app startup.\n\n");
    let path = create_migration(root, name, &sql)?;
    let shown = path.strip_prefix(root).unwrap_or(&path);
    ui::status("Created", shown.display());
    Ok(())
}

/// `zapline db migrate`.
pub fn migrate(project: &Project, bin: Option<&str>) -> Result<()> {
    let captured = build::run_app(
        project,
        bin,
        &[(ENV_MIGRATE_ONLY, OsStr::new("1"))],
        RUN_TIMEOUT,
    )?;
    if !captured.status.success() {
        // The app's own error (connection refused, bad SQL) is on stderr.
        return Err(captured.failure("while migrating"));
    }
    if !captured.stdout.contains("migrations are up to date") {
        bail!(
            "the app exited without migrating\n\
             hint: register the database with `App::database(Database::from_env())` \
             (needs zapline's `postgres` or `sqlite` feature) and call `App::run` in `main`"
        );
    }
    ui::status("Migrated", "the database is up to date");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_calendar_time() {
        assert_eq!(timestamp(0), "19700101000000");
        // 2026-10-22T06:24:06Z
        assert_eq!(timestamp(1_792_650_246), "20261022062406");
        // Leap day.
        assert_eq!(timestamp(951_782_400), "20000229000000");
    }

    #[test]
    fn file_names_are_snake_case() {
        assert_eq!(
            migration_file_name("Create Posts", 0).unwrap(),
            "19700101000000_create_posts.sql"
        );
        assert!(migration_file_name("!!!", 0).is_err());
    }

    #[test]
    fn migrations_created_in_the_same_second_keep_their_order() {
        let tmp = tempfile::tempdir().unwrap();
        let a = create_migration(tmp.path(), "first", "").unwrap();
        let b = create_migration(tmp.path(), "second", "").unwrap();
        let name = |p: &PathBuf| p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name(&a) < name(&b), "{} !< {}", name(&a), name(&b));
    }
}
