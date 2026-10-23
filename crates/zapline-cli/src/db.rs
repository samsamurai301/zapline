//! `zapline db`: create, apply, inspect and revert SQL migrations.
//!
//! Migrations are SQL files in `migrations/`, named after the sqlx
//! convention: a reversible pair `<UTC timestamp>_<description>.up.sql` +
//! `.down.sql` (the default), or a one-way `<UTC timestamp>_<description>.sql`
//! (what older projects have; `zapline db new` keeps writing those there
//! unless asked otherwise).
//!
//! None of the commands connect to the database themselves: they run the app
//! with `ZAPLINE_MIGRATE=<command>`, so `App::database` uses the app's own URL
//! and settings, does the work and exits before serving.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::build::{self, Captured};
use crate::naming;
use crate::project::Project;
use crate::ui;

/// Environment variable that makes `App::database` run one migration
/// command (`status`, `up`, `down:<n>`, `redo`, `reset`) and exit.
pub const ENV_MIGRATE: &str = "ZAPLINE_MIGRATE";

/// Must be `1` for a release build of the app to revert migrations.
pub const ENV_ALLOW_DESTRUCTIVE: &str = "ZAPLINE_ALLOW_DESTRUCTIVE";

/// Environment variable that makes the app migrate, run `App::seed` and exit.
pub const ENV_SEED: &str = "ZAPLINE_SEED";
/// Where migrations live, relative to the project root.
pub const MIGRATIONS_DIR: &str = "migrations";

/// The line the app prints when a migration command succeeded.
const DONE_MARKER: &str = "zapline-migrate: ok ";

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

/// Whether a new migration gets a down file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `<stem>.up.sql` + `<stem>.down.sql`: `zapline db rollback` can undo it.
    Reversible,
    /// `<stem>.sql`: applied once, never reverted.
    OneWay,
}

impl Style {
    /// The style a new migration in `dir` gets: one-way only in a project
    /// whose migrations are all one-way (made before reversible ones were
    /// the default), reversible otherwise, new projects included.
    pub fn detect(dir: &Path) -> Style {
        let names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".sql"))
                    .collect()
            })
            .unwrap_or_default();
        let one_way = |n: &String| !n.ends_with(".up.sql") && !n.ends_with(".down.sql");
        if !names.is_empty() && names.iter().all(one_way) {
            Style::OneWay
        } else {
            Style::Reversible
        }
    }
}

/// The file names of one migration, without the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationFiles {
    /// `<stem>.up.sql` or `<stem>.sql`.
    pub up: String,
    /// `<stem>.down.sql` for a reversible migration.
    pub down: Option<String>,
}

impl MigrationFiles {
    /// The files for `stem` (`<version>_<description>`) in `style`.
    pub fn new(stem: &str, style: Style) -> Self {
        match style {
            Style::Reversible => MigrationFiles {
                up: format!("{stem}.up.sql"),
                down: Some(format!("{stem}.down.sql")),
            },
            Style::OneWay => MigrationFiles {
                up: format!("{stem}.sql"),
                down: None,
            },
        }
    }

    /// The migration in `root`'s migrations directory whose description is
    /// `description` (e.g. `create_posts`), in the style it was written in.
    pub fn existing(root: &Path, description: &str) -> Option<Self> {
        let dir = root.join(MIGRATIONS_DIR);
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // `<digits>_<description>`, the description matched whole.
        let named = |stem: &str| {
            stem.split_once('_').is_some_and(|(version, rest)| {
                rest == description && version.bytes().all(|b| b.is_ascii_digit())
            })
        };
        names.iter().find_map(|n| {
            if let Some(stem) = n.strip_suffix(".up.sql") {
                named(stem).then(|| Self::new(stem, Style::Reversible))
            } else if n.ends_with(".down.sql") {
                None
            } else {
                let stem = n.strip_suffix(".sql")?;
                named(stem).then(|| Self::new(stem, Style::OneWay))
            }
        })
    }

    /// The files for a new migration called `name` in `root`: an existing
    /// one with that description (so `--force` regenerates it in place), or
    /// a new timestamped one in the project's [`Style`].
    pub fn plan(root: &Path, name: &str) -> Result<Self> {
        let slug = naming::snake_case(name);
        if let Some(existing) = Self::existing(root, &slug) {
            return Ok(existing);
        }
        Ok(Self::new(
            &new_migration_stem(root, name)?,
            Style::detect(&root.join(MIGRATIONS_DIR)),
        ))
    }
}

/// `<timestamp>_<snake name>` for a migration called `name` created at `secs`.
pub fn migration_stem(name: &str, secs: u64) -> Result<String> {
    let slug = naming::snake_case(name);
    if slug.is_empty() {
        bail!("`{name}` is not a usable migration name; try something like `create_posts`");
    }
    Ok(format!("{}_{slug}", timestamp(secs)))
}

/// The files for a new migration `name` created at `secs`.
pub fn migration_file_names(name: &str, secs: u64, style: Style) -> Result<MigrationFiles> {
    Ok(MigrationFiles::new(&migration_stem(name, secs)?, style))
}

/// A stem for a new migration `name` in `root`'s migrations directory:
/// timestamped now, or later if that version is taken, since two migrations
/// created in the same second (by back-to-back generators) must still get
/// distinct versions that sort in creation order.
pub fn new_migration_stem(root: &Path, name: &str) -> Result<String> {
    let dir = root.join(MIGRATIONS_DIR);
    let mut secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    while same_version_exists(&dir, secs) {
        secs += 1;
    }
    migration_stem(name, secs)
}

/// Creates a new migration `name` under `root`: `up` in its up file and,
/// when `down` is given, a reversible pair. Returns the paths written.
pub fn create_migration(
    root: &Path,
    name: &str,
    up: &str,
    down: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let dir = root.join(MIGRATIONS_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let style = if down.is_some() {
        Style::Reversible
    } else {
        Style::OneWay
    };
    let files = MigrationFiles::new(&new_migration_stem(root, name)?, style);
    let mut written = Vec::new();
    for (file, sql) in [(Some(&files.up), Some(up)), (files.down.as_ref(), down)] {
        if let (Some(file), Some(sql)) = (file, sql) {
            let path = dir.join(file);
            std::fs::write(&path, sql)
                .with_context(|| format!("cannot write {}", path.display()))?;
            written.push(path);
        }
    }
    Ok(written)
}

fn same_version_exists(dir: &Path, secs: u64) -> bool {
    let prefix = format!("{}_", timestamp(secs));
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
    })
}

/// The comments a new empty migration starts with.
pub fn blank_sql(name: &str, style: Style) -> (String, Option<String>) {
    match style {
        Style::Reversible => (
            format!(
                "-- {name}\n-- Applied once, in version order, by `zapline db migrate` or at app startup.\n\n"
            ),
            Some(format!(
                "-- Reverts {name}: undo what the .up.sql file does, in reverse order.\n\
                 -- Run by `zapline db rollback`, `redo` and `reset`.\n\n"
            )),
        ),
        Style::OneWay => (
            format!(
                "-- {name}\n-- Applied once, in file-name order, by `zapline db migrate` or at app startup.\n\n"
            ),
            None,
        ),
    }
}

/// `zapline db new <name>`: `style` overrides the project's [`Style`].
pub fn new(root: &Path, name: &str, style: Option<Style>) -> Result<()> {
    let style = style.unwrap_or_else(|| Style::detect(&root.join(MIGRATIONS_DIR)));
    let (up, down) = blank_sql(name, style);
    for path in create_migration(root, name, &up, down.as_deref())? {
        let shown = path.strip_prefix(root).unwrap_or(&path);
        ui::status("Created", shown.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Running migration commands in the app

/// Refuses `command` (one that reverts migrations: rollback, redo, reset)
/// against a release build or the production profile unless `force` is
/// set. Pure, so the rules are unit-tested.
pub fn check_destructive(
    command: &str,
    release: bool,
    production: bool,
    force: bool,
) -> Result<()> {
    if force || (!release && !production) {
        return Ok(());
    }
    let why = if release {
        "a release build"
    } else {
        "the production profile (APP_ENV/ZAPLINE_ENV)"
    };
    bail!(
        "refusing to run `zapline db {command}` against {why}: it reverts migrations and can drop data\n\
         hint: pass --force if you really mean it"
    )
}

/// Whether `APP_ENV` / `ZAPLINE_ENV` names the production profile.
pub fn production_profile(env: impl Fn(&str) -> Option<String>) -> bool {
    let profile = env("APP_ENV")
        .filter(|v| !v.is_empty())
        .or_else(|| env("ZAPLINE_ENV"));
    profile.is_some_and(|p| {
        let p = p.trim().to_ascii_lowercase();
        p == "production" || p == "prod"
    })
}

/// Runs the app with `ZAPLINE_MIGRATE=<command>` (plus `extra`) and checks
/// that it did the work.
fn run_command(
    project: &Project,
    bin: Option<&str>,
    command: &str,
    release: bool,
    extra: &[(&str, &OsStr)],
) -> Result<Captured> {
    let mut envs: Vec<(&str, &OsStr)> = vec![(ENV_MIGRATE, OsStr::new(command))];
    envs.extend_from_slice(extra);
    let captured = build::run_app_in(project, bin, &envs, RUN_TIMEOUT, release)?;
    if !captured.status.success() {
        // The app's own error (connection refused, bad SQL) is on stderr.
        return Err(captured.failure(&format!("while running `{command}`")));
    }
    if !captured.stdout.lines().any(|l| l.starts_with(DONE_MARKER)) {
        bail!(
            "the app exited without running the migration command\n\
             hint: register the database with `App::database(Database::from_env())` \
             (needs zapline's `postgres` or `sqlite` feature) and call `App::run` in `main`; \
             an app built with an older zapline only understands `zapline db migrate`"
        );
    }
    Ok(captured)
}

/// Prints the lines the app wrote about each migration (`applied ...`,
/// `reverted ...`) as status lines.
fn report_changes(stdout: &str) -> usize {
    let mut n = 0;
    for line in stdout.lines() {
        for (prefix, verb) in [
            ("applied ", "Applied"),
            ("reverted ", "Reverted"),
            ("redone ", "Redone"),
        ] {
            if let Some(rest) = line.strip_prefix(prefix) {
                ui::status(verb, rest);
                n += 1;
            }
        }
    }
    n
}

/// `zapline db migrate`.
pub fn migrate(project: &Project, bin: Option<&str>) -> Result<String> {
    let out = run_command(project, bin, "up", false, &[])?;
    let n = report_changes(&out.stdout);
    let summary = if n == 0 {
        "nothing to apply: the database is up to date".to_string()
    } else {
        format!("applied {n} migration(s): the database is up to date")
    };
    ui::status("Migrated", &summary);
    Ok(summary)
}

/// One row of `zapline db status`, parsed from the app's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    /// The migration's version.
    pub version: String,
    /// `applied`, `pending`, `checksum-mismatch`, `failed` or `missing`.
    pub state: String,
    /// Whether it has a down file.
    pub reversible: bool,
    /// When it was applied, if it was.
    pub applied_at: Option<String>,
    /// Its description.
    pub name: String,
}

/// The `zapline-migration` lines the app prints for `status`.
pub fn parse_status(stdout: &str) -> Vec<StatusRow> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.strip_prefix("zapline-migration\t")?.splitn(5, '\t');
            let version = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let reversible = parts.next()? == "reversible";
            let applied_at = Some(parts.next()?)
                .filter(|a| *a != "-")
                .map(str::to_string);
            let name = parts.next()?.to_string();
            Some(StatusRow {
                version,
                state,
                reversible,
                applied_at,
                name,
            })
        })
        .collect()
}

/// Renders `zapline db status`: version, name, applied at, state, and
/// whether the migration can be rolled back.
pub fn render_status(rows: &[StatusRow], color: bool) -> String {
    if rows.is_empty() {
        return "No migrations in migrations/.\n".to_string();
    }
    let state_label = |s: &str| match s {
        "checksum-mismatch" => "CHANGED since applied".to_string(),
        "missing" => "applied, file missing".to_string(),
        other => other.to_string(),
    };
    let w_version = rows
        .iter()
        .map(|r| r.version.len())
        .max()
        .unwrap_or(0)
        .max(7);
    let w_name = rows.iter().map(|r| r.name.len()).max().unwrap_or(0).max(4);
    let w_state = rows
        .iter()
        .map(|r| state_label(&r.state).len())
        .max()
        .unwrap_or(0)
        .max(5);
    let w_at = 19;
    let mut out = ui::bold(
        color,
        format!(
            "{:<w_version$}  {:<w_name$}  {:<w_at$}  {:<w_state$}  DOWN",
            "VERSION", "NAME", "APPLIED AT", "STATE"
        ),
    );
    out.push('\n');
    for r in rows {
        let state = format!("{:<w_state$}", state_label(&r.state));
        out.push_str(&format!(
            "{:<w_version$}  {:<w_name$}  {:<w_at$}  {}  {}\n",
            r.version,
            r.name,
            r.applied_at.as_deref().unwrap_or("-"),
            ui::paint(color, ui::migration_state_color(&r.state), state),
            if r.reversible { "yes" } else { "no" },
        ));
    }
    let applied = rows.iter().filter(|r| r.state != "pending").count();
    let pending = rows.len() - applied;
    out.push_str(&ui::dim(
        color,
        format!("\n{applied} applied, {pending} pending"),
    ));
    out.push('\n');
    let mismatched = rows
        .iter()
        .filter(|r| r.state == "checksum-mismatch")
        .count();
    if mismatched > 0 {
        out.push_str(&format!(
            "{} migration file(s) changed after being applied: restore them, or rebuild the \
             database with `zapline db reset` (development only)\n",
            mismatched
        ));
    }
    out
}

/// `zapline db status`: every migration and where it stands.
pub fn status(project: &Project, bin: Option<&str>) -> Result<Vec<StatusRow>> {
    let out = run_command(project, bin, "status", false, &[])?;
    Ok(parse_status(&out.stdout))
}

/// What the destructive commands are run against.
#[derive(Debug, Clone, Copy, Default)]
pub struct Target<'a> {
    /// Binary to run.
    pub bin: Option<&'a str>,
    /// Build and run the release profile.
    pub release: bool,
    /// Go ahead even against a release build or the production profile.
    pub force: bool,
}

fn destructive_env(
    target: Target<'_>,
    command: &str,
) -> Result<Vec<(&'static str, &'static OsStr)>> {
    let production = production_profile(|k| std::env::var(k).ok());
    check_destructive(command, target.release, production, target.force)?;
    // A release build of the app refuses on its own without this.
    Ok(if target.force {
        vec![(ENV_ALLOW_DESTRUCTIVE, OsStr::new("1"))]
    } else {
        Vec::new()
    })
}

/// `zapline db rollback [--steps N]`.
pub fn rollback(project: &Project, steps: u32, target: Target<'_>) -> Result<String> {
    if steps == 0 {
        bail!("--steps must be at least 1");
    }
    let env = destructive_env(target, "rollback")?;
    let out = run_command(
        project,
        target.bin,
        &format!("down:{steps}"),
        target.release,
        &env,
    )?;
    let n = report_changes(&out.stdout);
    let summary = if n == 0 {
        "nothing to roll back: no migration is applied".to_string()
    } else {
        format!("rolled back {n} migration(s)")
    };
    ui::status("Done", &summary);
    Ok(summary)
}

/// `zapline db redo`: roll back the last migration and apply it again.
pub fn redo(project: &Project, target: Target<'_>) -> Result<String> {
    let env = destructive_env(target, "redo")?;
    let out = run_command(project, target.bin, "redo", target.release, &env)?;
    let summary = match out.stdout.lines().find_map(|l| l.strip_prefix("redone ")) {
        Some(m) => {
            ui::status("Redone", m);
            format!("redone {m}")
        }
        None => {
            let s = "nothing to redo: no migration is applied".to_string();
            ui::status("Done", &s);
            s
        }
    };
    Ok(summary)
}

/// `zapline db reset`: roll everything back, migrate, and insert the seed
/// data when the app has some.
pub fn reset(project: &Project, target: Target<'_>) -> Result<String> {
    let mut env = destructive_env(target, "reset")?;
    env.push((ENV_SEED, OsStr::new("1")));
    let out = run_command(project, target.bin, "reset", target.release, &env)?;
    let seeded = out.stdout.contains("seed data inserted");
    let summary = format!(
        "the database was rebuilt from its migrations{}",
        if seeded { " and seeded" } else { "" }
    );
    ui::status("Reset", &summary);
    Ok(summary)
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
    fn file_names_are_snake_case_pairs_or_one_way() {
        let pair = migration_file_names("Create Posts", 0, Style::Reversible).unwrap();
        assert_eq!(pair.up, "19700101000000_create_posts.up.sql");
        assert_eq!(
            pair.down.as_deref(),
            Some("19700101000000_create_posts.down.sql")
        );
        let one = migration_file_names("add-email-to-users", 0, Style::OneWay).unwrap();
        assert_eq!(one.up, "19700101000000_add_email_to_users.sql");
        assert_eq!(one.down, None);
        assert!(migration_stem("!!!", 0).is_err());
    }

    #[test]
    fn new_projects_are_reversible_and_old_ones_stay_one_way() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(MIGRATIONS_DIR);
        assert_eq!(Style::detect(&dir), Style::Reversible, "no directory yet");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Style::detect(&dir), Style::Reversible, "empty");
        std::fs::write(dir.join("1_init.sql"), "").unwrap();
        assert_eq!(Style::detect(&dir), Style::OneWay);
        std::fs::write(dir.join("2_posts.up.sql"), "").unwrap();
        std::fs::write(dir.join("2_posts.down.sql"), "").unwrap();
        assert_eq!(Style::detect(&dir), Style::Reversible, "mixed");

        new(tmp.path(), "add_tags", None).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("add_tags"))
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("_add_tags.down.sql")));
    }

    #[test]
    fn existing_migrations_are_found_in_either_style() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(MIGRATIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("5_create_users.sql"), "").unwrap();
        std::fs::write(dir.join("6_create_posts.up.sql"), "").unwrap();
        std::fs::write(dir.join("6_create_posts.down.sql"), "").unwrap();
        assert_eq!(
            MigrationFiles::existing(tmp.path(), "create_users"),
            Some(MigrationFiles::new("5_create_users", Style::OneWay))
        );
        assert_eq!(
            MigrationFiles::plan(tmp.path(), "create_posts").unwrap(),
            MigrationFiles::new("6_create_posts", Style::Reversible)
        );
        assert_eq!(MigrationFiles::existing(tmp.path(), "posts"), None);
    }

    #[test]
    fn migrations_created_in_the_same_second_keep_their_order() {
        let tmp = tempfile::tempdir().unwrap();
        let a = create_migration(tmp.path(), "first", "", Some("")).unwrap();
        let b = create_migration(tmp.path(), "second", "", None).unwrap();
        assert_eq!((a.len(), b.len()), (2, 1));
        let name = |p: &PathBuf| p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name(&a[0]) < name(&b[0]),
            "{} !< {}",
            name(&a[0]),
            name(&b[0])
        );
    }

    #[test]
    fn release_builds_and_production_refuse_destructive_commands_without_force() {
        assert!(check_destructive("rollback", false, false, false).is_ok());
        let err = check_destructive("reset", true, false, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("release build"), "{msg}");
        assert!(msg.contains("--force"), "{msg}");
        assert!(check_destructive("reset", true, false, true).is_ok());
        let err = check_destructive("redo", false, true, false).unwrap_err();
        assert!(format!("{err:#}").contains("production"));
        assert!(check_destructive("redo", false, true, true).is_ok());
    }

    #[test]
    fn the_production_profile_is_read_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(!production_profile(env(&[])));
        assert!(production_profile(env(&[("APP_ENV", "production")])));
        assert!(production_profile(env(&[("ZAPLINE_ENV", "Prod")])));
        assert!(!production_profile(env(&[
            ("APP_ENV", "development"),
            ("ZAPLINE_ENV", "production")
        ])));
    }

    #[test]
    fn status_lines_parse_into_a_table() {
        let stdout = "zapline-migration\t20260101000000\tapplied\treversible\t2026-01-01 10:00:00\tcreate todos\n\
                      noise\n\
                      zapline-migration\t20260102000000\tchecksum-mismatch\tone-way\t2026-01-02 10:00:00\tadd tags\n\
                      zapline-migration\t20260103000000\tpending\treversible\t-\tcreate users\n\
                      zapline-migrate: ok status\n";
        let rows = parse_status(stdout);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "create todos");
        assert!(rows[0].reversible && !rows[1].reversible);
        assert_eq!(rows[2].applied_at, None);

        let table = render_status(&rows, false);
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[0].starts_with("VERSION         NAME"), "{table}");
        assert!(lines[2].contains("CHANGED since applied"), "{table}");
        assert!(
            lines[3].contains("pending") && lines[3].ends_with("yes"),
            "{table}"
        );
        assert!(table.contains("2 applied, 1 pending"), "{table}");
        assert!(table.contains("changed after being applied"), "{table}");
        assert_eq!(render_status(&[], false), "No migrations in migrations/.\n");
    }
}
