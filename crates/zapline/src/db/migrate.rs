//! Day-two migration operations: status, rollback, redo and reset, on top of
//! sqlx's [`Migrator`].
//!
//! A migration is reversible when it has a `.down.sql` file next to its up
//! file: either a `<version>_<name>.up.sql` + `<version>_<name>.down.sql`
//! pair (what `zapline db new` writes), or a one-way `<version>_<name>.sql`
//! with a `<version>_<name>.down.sql` added later. Rolling back a migration
//! without one is refused before anything runs.
//!
//! The CLI drives these through [`ENV_MIGRATE`]; tests and scripts can call
//! [`Migrations`] directly on a pool.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use sqlx::migrate::{Migrate, Migration, Migrator};
use sqlx::Pool;

use crate::{Error, Result};

/// Run one migration command at startup and exit instead of serving:
/// `status`, `up`, `down:<n>`, `redo` or `reset` (see [`MigrateCommand`]).
/// Read by [`App::database`](crate::App::database); `zapline db status`,
/// `migrate`, `rollback`, `redo` and `reset` set it.
pub const ENV_MIGRATE: &str = "ZAPLINE_MIGRATE";

/// Must be `1` for a release build to run a destructive command (`down`,
/// `redo`, `reset`), so a production binary can't drop tables by accident.
pub const ENV_ALLOW_DESTRUCTIVE: &str = "ZAPLINE_ALLOW_DESTRUCTIVE";

/// What [`ENV_MIGRATE`] asks for.
///
/// ```
/// # #[cfg(feature = "sqlite")] {
/// use zapline::db::MigrateCommand;
///
/// assert_eq!("down:2".parse::<MigrateCommand>().unwrap(), MigrateCommand::Down(2));
/// assert!(MigrateCommand::Reset.is_destructive());
/// # }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrateCommand {
    /// Print every migration and whether it is applied, then exit.
    Status,
    /// Apply the pending migrations, then exit.
    Up,
    /// Revert the last `n` applied migrations, then exit.
    Down(u32),
    /// Revert the last applied migration and apply it again, then exit.
    Redo,
    /// Revert every migration, apply them all again (and run the seed when
    /// `ZAPLINE_SEED` is set), then exit.
    Reset,
}

impl MigrateCommand {
    /// Whether the command can lose data (`down`, `redo`, `reset`).
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// assert!(!zapline::db::MigrateCommand::Status.is_destructive());
    /// # }
    /// ```
    pub fn is_destructive(self) -> bool {
        matches!(self, Self::Down(_) | Self::Redo | Self::Reset)
    }

    /// The command named by [`ENV_MIGRATE`], or `up` when only the older
    /// `ZAPLINE_MIGRATE_ONLY` is set; `None` when neither is.
    ///
    /// # Errors
    ///
    /// When the variable holds something other than a command.
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// // Unset in a normal run: the app serves.
    /// assert_eq!(zapline::db::MigrateCommand::from_env().unwrap(), None);
    /// # }
    /// ```
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var(ENV_MIGRATE) {
            Ok(v) if !v.trim().is_empty() => v.parse().map(Some),
            _ if std::env::var_os(super::ENV_MIGRATE_ONLY).is_some() => Ok(Some(Self::Up)),
            _ => Ok(None),
        }
    }
}

impl FromStr for MigrateCommand {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let command = match s {
            "status" => Self::Status,
            "up" | "1" => Self::Up,
            "redo" => Self::Redo,
            "reset" => Self::Reset,
            "down" => Self::Down(1),
            _ => match s.strip_prefix("down:").map(str::parse::<u32>) {
                Some(Ok(n)) if n > 0 => Self::Down(n),
                _ => {
                    return Err(Error::internal(format!(
                        "{ENV_MIGRATE}={s:?} is not a migration command; use status, up, \
                         down:<n>, redo or reset"
                    )))
                }
            },
        };
        Ok(command)
    }
}

impl fmt::Display for MigrateCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status => f.write_str("status"),
            Self::Up => f.write_str("up"),
            Self::Down(n) => write!(f, "down:{n}"),
            Self::Redo => f.write_str("redo"),
            Self::Reset => f.write_str("reset"),
        }
    }
}

/// Refuses a destructive `command` in a release build unless
/// [`ENV_ALLOW_DESTRUCTIVE`] is `1`. `debug` is `cfg!(debug_assertions)` and
/// `allow` the variable's value, passed in so the rule is testable.
pub(crate) fn guard_destructive(
    command: MigrateCommand,
    debug: bool,
    allow: Option<&str>,
) -> Result<()> {
    if !command.is_destructive() || debug || allow.map(str::trim) == Some("1") {
        return Ok(());
    }
    Err(Error::internal(format!(
        "refusing to run `{command}` in a release build: it reverts migrations and can drop \
         data. Set {ENV_ALLOW_DESTRUCTIVE}=1 if you really mean it"
    )))
}

/// Where a migration stands, from [`Migrations::status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationState {
    /// The file exists and has not run.
    Pending,
    /// Applied, and the file still matches what ran.
    Applied,
    /// Applied, but the file changed since: `up` refuses to run until the
    /// file is restored (or the database rebuilt).
    ChecksumMismatch,
    /// Started but did not finish (a failed non-transactional migration):
    /// fix the database by hand, then delete its row from
    /// `_sqlx_migrations`.
    Failed,
    /// Applied, but its file is gone.
    Missing,
}

impl MigrationState {
    /// A short lowercase name: `pending`, `applied`, `checksum-mismatch`,
    /// `failed` or `missing`.
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// assert_eq!(zapline::db::MigrationState::ChecksumMismatch.as_str(), "checksum-mismatch");
    /// # }
    /// ```
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::ChecksumMismatch => "checksum-mismatch",
            Self::Failed => "failed",
            Self::Missing => "missing",
        }
    }
}

/// One migration as [`Migrations::status`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationInfo {
    /// The number before the first `_` in the file name (a UTC timestamp
    /// for files made by `zapline db new`).
    pub version: i64,
    /// The rest of the file name, with spaces for underscores.
    pub name: String,
    /// When it was applied (UTC, `YYYY-MM-DD HH:MM:SS`), if it was.
    pub applied_at: Option<String>,
    /// Whether a `.down.sql` file can revert it.
    pub reversible: bool,
    /// Where it stands.
    pub state: MigrationState,
}

/// A migration as recorded in `_sqlx_migrations`.
#[doc(hidden)]
#[derive(Debug)]
pub struct AppliedRow {
    version: i64,
    description: String,
    installed_on: String,
    success: bool,
    checksum: Vec<u8>,
}

mod sealed {
    pub trait Sealed {}
}

/// A database whose migrations zapline can inspect and revert: SQLite and
/// Postgres. It appears in the bounds of [`App::database`](crate::App::database)
/// and [`Migrations`]; there is nothing to implement.
pub trait MigrateBackend: sqlx::Database + sealed::Sealed {
    /// The rows of `_sqlx_migrations`, oldest first; empty when the table
    /// doesn't exist yet.
    #[doc(hidden)]
    fn applied(pool: &Pool<Self>) -> impl Future<Output = Result<Vec<AppliedRow>>> + Send;
}

macro_rules! migrate_backend {
    ($db:ty, $exists:literal, $installed_on:literal) => {
        impl sealed::Sealed for $db {}

        impl MigrateBackend for $db {
            async fn applied(pool: &Pool<Self>) -> Result<Vec<AppliedRow>> {
                let (exists,): (bool,) = sqlx::query_as($exists).fetch_one(pool).await?;
                if !exists {
                    return Ok(Vec::new());
                }
                let rows: Vec<(i64, String, String, bool, Vec<u8>)> = sqlx::query_as(concat!(
                    "SELECT version, description, ",
                    $installed_on,
                    ", success, checksum FROM _sqlx_migrations ORDER BY version"
                ))
                .fetch_all(pool)
                .await?;
                Ok(rows
                    .into_iter()
                    .map(
                        |(version, description, installed_on, success, checksum)| AppliedRow {
                            version,
                            description,
                            installed_on,
                            success,
                            checksum,
                        },
                    )
                    .collect())
            }
        }
    };
}

#[cfg(feature = "sqlite")]
migrate_backend!(
    sqlx::Sqlite,
    "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations')",
    "strftime('%Y-%m-%d %H:%M:%S', installed_on)"
);
#[cfg(feature = "postgres")]
migrate_backend!(
    sqlx::Postgres,
    "SELECT to_regclass('_sqlx_migrations') IS NOT NULL",
    "to_char(installed_on AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')"
);

/// The migrations in a directory, and what can be done with them on a pool:
/// [`status`](Self::status), [`up`](Self::up), [`down`](Self::down),
/// [`redo`](Self::redo) and [`reset`](Self::reset).
///
/// [`App::database`](crate::App::database) uses it for `ZAPLINE_MIGRATE`;
/// call it yourself in tests or one-off tools.
///
/// ```
/// # #[cfg(feature = "sqlite")] {
/// use zapline::db::{sqlx, Migrations};
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
/// let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;
/// let migrations = Migrations::open("migrations").await?; // a missing directory is empty
/// migrations.up(&pool).await?;
/// for m in migrations.status(&pool).await? {
///     println!("{} {} {}", m.version, m.state.as_str(), m.name);
/// }
/// # Ok(()) }
/// # }
/// ```
#[derive(Debug)]
pub struct Migrations {
    /// `None` when the directory doesn't exist.
    migrator: Option<Migrator>,
    dir: PathBuf,
}

impl Migrations {
    /// Read the migrations in `dir`. A missing directory has none.
    ///
    /// # Errors
    ///
    /// When a file name has no numeric version or a file can't be read.
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// let none = zapline::db::Migrations::open("no/such/dir").await?;
    /// # Ok(()) }
    /// # }
    /// ```
    pub async fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let migrator = if dir.is_dir() {
            Some(Migrator::new(dir.clone()).await.map_err(|e| {
                Error::internal(format!("cannot read migrations in {}: {e}", dir.display()))
            })?)
        } else {
            None
        };
        Ok(Migrations { migrator, dir })
    }

    fn all(&self) -> std::slice::Iter<'_, Migration> {
        self.migrator.as_ref().map_or([].iter(), |m| m.iter())
    }

    fn ups(&self) -> impl Iterator<Item = &Migration> {
        self.all().filter(|m| !m.migration_type.is_down_migration())
    }

    fn down_of(&self, version: i64) -> Option<&Migration> {
        self.all()
            .find(|m| m.version == version && m.migration_type.is_down_migration())
    }

    /// Every migration, in version order: the files in the directory plus
    /// any applied migration whose file is gone.
    ///
    /// # Errors
    ///
    /// When the database can't be queried.
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// # use zapline::db::{sqlx, Migrations, MigrationState};
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// # let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;
    /// let status = Migrations::open("migrations").await?.status(&pool).await?;
    /// let pending = status.iter().filter(|m| m.state == MigrationState::Pending).count();
    /// # Ok(()) }
    /// # }
    /// ```
    pub async fn status<DB>(&self, pool: &Pool<DB>) -> Result<Vec<MigrationInfo>>
    where
        DB: MigrateBackend,
        DB::Connection: Migrate,
    {
        let applied = DB::applied(pool).await?;
        let mut out: Vec<MigrationInfo> = self
            .ups()
            .map(|m| {
                let row = applied.iter().find(|a| a.version == m.version);
                let state = match row {
                    None => MigrationState::Pending,
                    Some(a) if !a.success => MigrationState::Failed,
                    Some(a) if a.checksum.as_slice() != &*m.checksum => {
                        MigrationState::ChecksumMismatch
                    }
                    Some(_) => MigrationState::Applied,
                };
                MigrationInfo {
                    version: m.version,
                    name: m.description.to_string(),
                    applied_at: row.map(|a| a.installed_on.clone()),
                    reversible: self.down_of(m.version).is_some(),
                    state,
                }
            })
            .collect();
        for a in &applied {
            if !out.iter().any(|m| m.version == a.version) {
                out.push(MigrationInfo {
                    version: a.version,
                    name: a.description.clone(),
                    applied_at: Some(a.installed_on.clone()),
                    reversible: false,
                    state: if a.success {
                        MigrationState::Missing
                    } else {
                        MigrationState::Failed
                    },
                });
            }
        }
        out.sort_by_key(|m| m.version);
        Ok(out)
    }

    /// Apply the pending migrations and return them.
    ///
    /// # Errors
    ///
    /// When a migration fails (it is rolled back unless it opted out of the
    /// transaction), or an applied migration's file changed.
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// # use zapline::db::{sqlx, Migrations};
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// # let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;
    /// for m in Migrations::open("migrations").await?.up(&pool).await? {
    ///     println!("applied {} {}", m.version, m.name);
    /// }
    /// # Ok(()) }
    /// # }
    /// ```
    pub async fn up<DB>(&self, pool: &Pool<DB>) -> Result<Vec<MigrationInfo>>
    where
        DB: MigrateBackend,
        DB::Connection: Migrate,
    {
        let Some(migrator) = &self.migrator else {
            return Ok(Vec::new());
        };
        let before = self.status(pool).await?;
        migrator
            .run(pool)
            .await
            .map_err(|e| Error::internal(format!("migration failed: {e}")))?;
        let after = self.status(pool).await?;
        Ok(after
            .into_iter()
            .filter(|m| {
                m.state == MigrationState::Applied
                    && before
                        .iter()
                        .any(|b| b.version == m.version && b.state == MigrationState::Pending)
            })
            .collect())
    }

    /// Revert the last `steps` applied migrations, newest first, and return
    /// them. Nothing runs unless every one of them has a `.down.sql` file.
    ///
    /// # Errors
    ///
    /// When one of them can't be reverted (no down file, file gone, failed
    /// earlier) or its down SQL fails.
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// # use zapline::db::{sqlx, Migrations};
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// # let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;
    /// let reverted = Migrations::open("migrations").await?.down(&pool, 1).await?;
    /// # Ok(()) }
    /// # }
    /// ```
    pub async fn down<DB>(&self, pool: &Pool<DB>, steps: u32) -> Result<Vec<MigrationInfo>>
    where
        DB: MigrateBackend,
        DB::Connection: Migrate,
    {
        let status = self.status(pool).await?;
        let applied: Vec<&MigrationInfo> = status
            .iter()
            .rev()
            .filter(|m| m.state != MigrationState::Pending)
            .collect();
        let steps = usize::try_from(steps).unwrap_or(usize::MAX);
        let chosen: Vec<&MigrationInfo> = applied.iter().copied().take(steps).collect();
        for m in &chosen {
            match m.state {
                MigrationState::Failed => {
                    return Err(Error::internal(format!(
                        "migration {} ({}) did not finish; repair the database by hand, then \
                         delete its row from _sqlx_migrations",
                        m.version, m.name
                    )))
                }
                MigrationState::Missing => {
                    return Err(Error::internal(format!(
                        "migration {} ({}) is applied but its file is no longer in {}",
                        m.version,
                        m.name,
                        self.dir.display()
                    )))
                }
                _ if !m.reversible => {
                    return Err(Error::internal(format!(
                        "migration {} ({}) has no down file, so it can't be reverted; add \
                         {}/{}_{}.down.sql with the SQL that undoes it",
                        m.version,
                        m.name,
                        self.dir.display(),
                        m.version,
                        m.name.replace(' ', "_")
                    )))
                }
                _ => {}
            }
        }
        let (Some(last), Some(migrator)) = (chosen.last(), &self.migrator) else {
            return Ok(Vec::new());
        };
        // Everything above the next-older applied migration is reverted.
        let target = applied
            .iter()
            .find(|m| m.version < last.version)
            .map_or(0, |m| m.version);
        migrator
            .undo(pool, target)
            .await
            .map_err(|e| Error::internal(format!("rollback failed: {e}")))?;
        Ok(chosen
            .into_iter()
            .map(|m| MigrationInfo {
                applied_at: None,
                state: MigrationState::Pending,
                ..m.clone()
            })
            .collect())
    }

    /// Revert the last applied migration and apply it again (handy while
    /// writing it). Returns it, or `None` when nothing is applied.
    ///
    /// # Errors
    ///
    /// As for [`down`](Self::down), or when the up SQL fails.
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// # use zapline::db::{sqlx, Migrations};
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// # let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;
    /// Migrations::open("migrations").await?.redo(&pool).await?;
    /// # Ok(()) }
    /// # }
    /// ```
    pub async fn redo<DB>(&self, pool: &Pool<DB>) -> Result<Option<MigrationInfo>>
    where
        DB: MigrateBackend,
        DB::Connection: Migrate,
    {
        let Some(reverted) = self.down(pool, 1).await?.into_iter().next() else {
            return Ok(None);
        };
        let Some(up) = self.ups().find(|m| m.version == reverted.version) else {
            return Ok(None);
        };
        // Only this migration: newer pending ones stay pending.
        let mut conn = pool.acquire().await?;
        conn.apply(up)
            .await
            .map_err(|e| Error::internal(format!("migration failed: {e}")))?;
        drop(conn);
        Ok(self
            .status(pool)
            .await?
            .into_iter()
            .find(|m| m.version == reverted.version))
    }

    /// Revert every applied migration, then apply them all: an empty schema
    /// at the latest version. Returns the migrations applied.
    ///
    /// # Errors
    ///
    /// As for [`down`](Self::down) and [`up`](Self::up).
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// # use zapline::db::{sqlx, Migrations};
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// # let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;
    /// Migrations::open("migrations").await?.reset(&pool).await?;
    /// # Ok(()) }
    /// # }
    /// ```
    pub async fn reset<DB>(&self, pool: &Pool<DB>) -> Result<Vec<MigrationInfo>>
    where
        DB: MigrateBackend,
        DB::Connection: Migrate,
    {
        self.down(pool, u32::MAX).await?;
        self.up(pool).await
    }
}

/// The line `status` prints for one migration; `zapline db status` parses
/// it: `zapline-migration <TAB> version <TAB> state <TAB> reversible
/// <TAB> applied_at or - <TAB> name`.
pub(crate) fn status_line(m: &MigrationInfo) -> String {
    format!(
        "zapline-migration\t{}\t{}\t{}\t{}\t{}",
        m.version,
        m.state.as_str(),
        if m.reversible {
            "reversible"
        } else {
            "one-way"
        },
        m.applied_at.as_deref().unwrap_or("-"),
        m.name
    )
}

/// Runs `command` on `pool` and prints what happened, one line per
/// migration, then a final `zapline-migrate: ok <command>` line the CLI
/// looks for.
pub(crate) async fn run_command<DB>(
    command: MigrateCommand,
    pool: &Pool<DB>,
    dir: &Path,
) -> Result<()>
where
    DB: MigrateBackend,
    DB::Connection: Migrate,
{
    guard_destructive(
        command,
        cfg!(debug_assertions),
        std::env::var(ENV_ALLOW_DESTRUCTIVE).ok().as_deref(),
    )?;
    let migrations = Migrations::open(dir).await?;
    match command {
        MigrateCommand::Status => {
            for m in migrations.status(pool).await? {
                println!("{}", status_line(&m));
            }
        }
        MigrateCommand::Up => {
            for m in migrations.up(pool).await? {
                println!("applied {} {}", m.version, m.name);
            }
            // Older CLIs look for this line.
            println!("migrations are up to date");
        }
        MigrateCommand::Down(n) => {
            for m in migrations.down(pool, n).await? {
                println!("reverted {} {}", m.version, m.name);
            }
        }
        MigrateCommand::Redo => match migrations.redo(pool).await? {
            Some(m) => println!("redone {} {}", m.version, m.name),
            None => println!("nothing to redo"),
        },
        MigrateCommand::Reset => {
            let applied = migrations.reset(pool).await?;
            println!(
                "reset: reverted everything and applied {} migration(s)",
                applied.len()
            );
        }
    }
    println!("zapline-migrate: ok {command}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_and_print() {
        for (text, command) in [
            ("status", MigrateCommand::Status),
            ("up", MigrateCommand::Up),
            ("1", MigrateCommand::Up),
            ("down", MigrateCommand::Down(1)),
            ("down:3", MigrateCommand::Down(3)),
            ("redo", MigrateCommand::Redo),
            (" reset ", MigrateCommand::Reset),
        ] {
            assert_eq!(text.parse::<MigrateCommand>().unwrap(), command, "{text}");
        }
        for bad in ["down:0", "down:x", "drop", ""] {
            assert!(bad.parse::<MigrateCommand>().is_err(), "{bad}");
        }
        assert_eq!(MigrateCommand::Down(2).to_string(), "down:2");
    }

    #[test]
    fn release_builds_refuse_destructive_commands_without_the_opt_in() {
        use MigrateCommand::*;
        for command in [Down(1), Redo, Reset] {
            let err = guard_destructive(command, false, None).unwrap_err();
            assert!(err.message().contains(ENV_ALLOW_DESTRUCTIVE), "{command}");
            assert!(guard_destructive(command, false, Some("0")).is_err());
            assert!(guard_destructive(command, false, Some("1")).is_ok());
            assert!(guard_destructive(command, true, None).is_ok(), "debug");
        }
        assert!(guard_destructive(Status, false, None).is_ok());
        assert!(guard_destructive(Up, false, None).is_ok());
    }
}
