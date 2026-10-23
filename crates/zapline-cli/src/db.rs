//! `zapline db`: create and apply SQL migrations.
//!
//! Migrations are plain SQL files in `migrations/`, named
//! `<UTC timestamp>_<description>.sql` (the sqlx convention). `zapline db
//! migrate` doesn't connect to the database itself: it runs the app with
//! `ZAPLINE_MIGRATE_ONLY=1`, so `App::database` applies the migrations with
//! the app's own URL and settings and exits before serving. `zapline db
//! status` works the same way with `ZAPLINE_MIGRATE_STATUS=1`: the app prints
//! what its database has applied and exits.
//!
//! New migrations come from [`crate::generate::migration`] (`zapline db new`
//! is `zapline generate migration`).

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

/// Environment variable that makes `App::database` print one JSON line per
/// migration (applied, checksum intact) and exit without migrating.
pub const ENV_MIGRATE_STATUS: &str = "ZAPLINE_MIGRATE_STATUS";

/// Environment variable that makes the app migrate, run `App::seed` and exit.
pub const ENV_SEED: &str = "ZAPLINE_SEED";
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

/// A file name for a new migration `name` in `root`'s migrations
/// directory: timestamped now, or later if that version is taken, since two
/// migrations created in the same second (by back-to-back generators) must
/// still get distinct versions that sort in creation order.
pub fn new_migration_name(root: &Path, name: &str) -> Result<String> {
    let dir = root.join(MIGRATIONS_DIR);
    let mut secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    while same_version_exists(&dir, secs) {
        secs += 1;
    }
    migration_file_name(name, secs)
}

/// Creates `migrations/<timestamp>_<name>.sql` under `root` with `sql` as
/// its contents and returns its path.
pub fn create_migration(root: &Path, name: &str, sql: &str) -> Result<PathBuf> {
    let dir = root.join(MIGRATIONS_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(new_migration_name(root, name)?);
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

/// One line of the app's `ZAPLINE_MIGRATE_STATUS` output.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct MigrationRow {
    /// The version (the file name's timestamp).
    pub version: i64,
    /// The rest of the file name, with spaces for underscores.
    pub description: String,
    /// Whether the database has run it.
    pub applied: bool,
    /// `false` when the file changed after it was applied.
    pub checksum_ok: bool,
    /// The database ran it but the file is gone.
    #[serde(default)]
    pub missing: bool,
}

/// Where a migration stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationState {
    /// Run, and unchanged since.
    Applied,
    /// Not run yet: the next `zapline db migrate` or app start runs it.
    Pending,
    /// Run, but the file was edited afterwards; sqlx refuses to start.
    Modified,
    /// Run, but the file is no longer in `migrations/`; sqlx refuses too.
    Missing,
}

impl MigrationRow {
    /// Where this migration stands.
    pub fn state(&self) -> MigrationState {
        match (self.applied, self.missing, self.checksum_ok) {
            (false, _, _) => MigrationState::Pending,
            (true, true, _) => MigrationState::Missing,
            (true, false, false) => MigrationState::Modified,
            (true, false, true) => MigrationState::Applied,
        }
    }
}

/// Reads the app's status lines; `None` when the end marker is missing (the
/// app does not register a database, or an older zapline).
pub fn parse_status(stdout: &str) -> Option<Vec<MigrationRow>> {
    if !stdout.lines().any(|l| l.starts_with("migration status: ")) {
        return None;
    }
    Some(
        stdout
            .lines()
            .filter(|l| l.starts_with("{\"version\""))
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect(),
    )
}

/// The status table and whether every applied migration is intact.
pub fn render_status(rows: &[MigrationRow], color: bool) -> (String, bool) {
    if rows.is_empty() {
        return (format!("no migrations in {MIGRATIONS_DIR}/\n"), true);
    }
    let width = rows
        .iter()
        .map(|r| r.version.to_string().len())
        .max()
        .unwrap_or(7)
        .max("VERSION".len());
    let mut out = format!("{:<width$}  {:<8}  DESCRIPTION\n", "VERSION", "STATUS");
    let mut counts = [0usize; 4];
    for r in rows {
        let state = r.state();
        let (word, style, extra) = match state {
            MigrationState::Applied => ("applied", "32", ""),
            MigrationState::Pending => ("pending", "33", ""),
            MigrationState::Modified => ("modified", "1;31", "  (edited after it was applied)"),
            MigrationState::Missing => ("missing", "1;31", "  (applied, but the file is gone)"),
        };
        counts[state as usize] += 1;
        out.push_str(&format!(
            "{:<width$}  {}  {}{extra}\n",
            r.version,
            ui::paint(color, style, format!("{word:<8}")),
            r.description,
        ));
    }
    let [applied, pending, modified, missing] = counts;
    let mut summary = vec![format!("{applied} applied"), format!("{pending} pending")];
    if modified > 0 {
        summary.push(format!("{modified} modified after apply"));
    }
    if missing > 0 {
        summary.push(format!("{missing} missing"));
    }
    out.push_str(&summary.join(", "));
    out.push('\n');
    (out, modified + missing == 0)
}

/// Runs the app with `ZAPLINE_MIGRATE_STATUS=1` and returns its rows.
pub fn status_rows(project: &Project, bin: Option<&str>) -> Result<Vec<MigrationRow>> {
    let captured = build::run_app(
        project,
        bin,
        &[(ENV_MIGRATE_STATUS, OsStr::new("1"))],
        RUN_TIMEOUT,
    )?;
    if !captured.status.success() {
        return Err(captured.failure("while reading the migration status"));
    }
    parse_status(&captured.stdout).with_context(|| {
        "the app exited without reporting its migrations\n\
         hint: register the database with `App::database(Database::from_env())` \
         (needs zapline's `postgres` or `sqlite` feature) and call `App::run` in `main`"
            .to_string()
    })
}

/// `zapline db status` as text: the table, plus an error when an applied
/// migration was edited or deleted.
pub fn status_report(project: &Project, bin: Option<&str>, color: bool) -> Result<(bool, String)> {
    let rows = status_rows(project, bin)?;
    let (mut table, ok) = render_status(&rows, color);
    if !ok {
        table.push_str(
            "\nsqlx refuses to start while an applied migration differs from what ran:\n",
        );
        for r in &rows {
            match r.state() {
                MigrationState::Modified => table.push_str(&format!(
                    "  restore {} as it was (e.g. `git checkout -- <file>`) and put the change \
                     in a new migration (`zapline g migration ..`)\n",
                    migration_file(&project.root, r.version)
                )),
                MigrationState::Missing => table.push_str(&format!(
                    "  version {} ran but its file is gone; restore it from version control\n",
                    r.version
                )),
                MigrationState::Applied | MigrationState::Pending => {}
            }
        }
    }
    Ok((ok, table))
}

/// `migrations/<file>` for `version`, or just the version when no file
/// has it.
fn migration_file(root: &Path, version: i64) -> String {
    let prefix = format!("{version}_");
    std::fs::read_dir(root.join(MIGRATIONS_DIR))
        .ok()
        .and_then(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .find(|n| n.starts_with(&prefix))
        })
        .map_or(format!("migration {version}"), |n| {
            format!("{MIGRATIONS_DIR}/{n}")
        })
}

/// `zapline db status`: prints the table; exits non-zero when an applied
/// migration was edited or deleted.
pub fn status(project: &Project, bin: Option<&str>) -> Result<()> {
    let (ok, report) = status_report(project, bin, ui::stdout_color())?;
    print!("{report}");
    if !ok {
        bail!("an applied migration was edited or deleted");
    }
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

/// `zapline db seed`: runs the app with `ZAPLINE_SEED=1`, which migrates,
/// runs the function given to `App::seed` once, and exits.
pub fn seed(project: &Project, bin: Option<&str>) -> Result<()> {
    let captured = build::run_app(project, bin, &[(ENV_SEED, OsStr::new("1"))], RUN_TIMEOUT)?;
    if !captured.status.success() {
        return Err(captured.failure("while seeding"));
    }
    if !captured.stdout.contains("seed data inserted") {
        bail!(
            "the app exited without seeding\n\
             hint: add `.seed(|db| async move {{ .. }})` after `.database(..)` in `app()`"
        );
    }
    ui::status("Seeded", "the database");
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
    fn status_lines_become_a_table() {
        let stdout = "\
starting\n\
{\"version\":1,\"description\":\"create notes\",\"applied\":true,\"checksum_ok\":true,\"missing\":false}\n\
{\"version\":2,\"description\":\"create tags\",\"applied\":true,\"checksum_ok\":false,\"missing\":false}\n\
{\"version\":3,\"description\":\"add body to notes\",\"applied\":false,\"checksum_ok\":true,\"missing\":false}\n\
{\"version\":4,\"description\":\"\",\"applied\":true,\"checksum_ok\":true,\"missing\":true}\n\
migration status: 4 migrations\n";
        let rows = parse_status(stdout).unwrap();
        let states: Vec<MigrationState> = rows.iter().map(MigrationRow::state).collect();
        assert_eq!(
            states,
            [
                MigrationState::Applied,
                MigrationState::Modified,
                MigrationState::Pending,
                MigrationState::Missing
            ]
        );
        let (table, ok) = render_status(&rows, false);
        assert!(!ok);
        assert_eq!(
            table,
            "\
VERSION  STATUS    DESCRIPTION
1        applied   create notes
2        modified  create tags  (edited after it was applied)
3        pending   add body to notes
4        missing     (applied, but the file is gone)
1 applied, 1 pending, 1 modified after apply, 1 missing
"
        );
        let (table, ok) = render_status(&rows[..1], false);
        assert!(ok);
        assert!(table.ends_with("1 applied, 0 pending\n"), "{table}");
        assert!(render_status(&[], false).0.contains("no migrations"));
        // No end marker: the app never got to report.
        assert_eq!(parse_status("listening on 0.0.0.0:8080\n"), None);
        assert_eq!(
            parse_status("migration status: 0 migrations\n"),
            Some(vec![])
        );
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
