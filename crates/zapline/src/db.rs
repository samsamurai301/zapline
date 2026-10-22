//! SQL databases with [sqlx]: a connection pool, migrations at startup, a
//! `Db` extractor, a health check and throwaway databases for tests.
//!
//! Enable the `postgres` or `sqlite` feature (or both), then register a
//! [`Database`] on the app. The pool is created by a startup hook, pending
//! migrations from `./migrations` are applied, and handlers take [`Db`]:
//!
//! ```no_run
//! # #[cfg(feature = "sqlite")] {
//! use zapline::db::{Database, Db};
//! use zapline::prelude::*;
//!
//! #[get("/users/count")]
//! async fn count(db: Db) -> Result<String> {
//!     let (n,): (i64,) = zapline::db::sqlx::query_as("SELECT COUNT(*) FROM users")
//!         .fetch_one(&*db)
//!         .await?;
//!     Ok(n.to_string())
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .database(Database::from_env()) // DATABASE_URL, e.g. sqlite://app.db or postgres://...
//!         .mount(count)
//!         .run("0.0.0.0:8080")
//! }
//! # }
//! ```
//!
//! [`Db`] dereferences to the sqlx pool, so every sqlx query API works on
//! `&*db`. `sqlx::Error` converts into [`Error`]: `RowNotFound` becomes a 404
//! and unique-constraint violations a 409, everything else a 500 that doesn't
//! leak the database message.
//!
//! # Tests
//!
//! [`Database::sqlite_memory`] gives every app its own empty in-memory
//! database with the migrations applied, so tests are isolated and need no
//! server:
//!
//! ```
//! # #[cfg(feature = "sqlite")] {
//! # use zapline::prelude::*;
//! # use zapline::db::Database;
//! let client = App::new().database(Database::sqlite_memory()).test();
//! # }
//! ```
//!
//! # Migrations
//!
//! Plain SQL files in `migrations/`, named `<version>_<description>.sql`
//! (`zapline db new <name>` creates one). They run in order at startup,
//! each at most once; turn that off with [`Database::migrate`]`(false)`, or
//! point elsewhere with [`Database::migrations`]. `zapline db migrate` applies
//! them without starting the server: it runs the app with
//! [`ENV_MIGRATE_ONLY`] set.
//!
//! [sqlx]: https://docs.rs/sqlx

use std::borrow::Cow;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::PathBuf;
use std::time::Duration;

pub use sqlx;
use sqlx::pool::PoolOptions;
use sqlx::{Connection, Pool};

use crate::extract::FromRequestParts;
use crate::health::HealthCheck;
use crate::{App, Error, Request, Resources};

/// The backend `Db` and `Database` use when no type parameter is given:
/// Postgres when the `postgres` feature is on, otherwise SQLite.
#[cfg(feature = "postgres")]
pub type DefaultDb = sqlx::Postgres;
/// The backend `Db` and `Database` use when no type parameter is given:
/// Postgres when the `postgres` feature is on, otherwise SQLite.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub type DefaultDb = sqlx::Sqlite;

/// When this environment variable is set, the app applies its migrations
/// during startup and exits instead of serving. Used by `zapline db migrate`.
pub const ENV_MIGRATE_ONLY: &str = "ZAPLINE_MIGRATE_ONLY";

/// Directory migrations are read from unless [`Database::migrations`] says
/// otherwise. Relative to the working directory (the project root under
/// `cargo run` and `zapline dev`).
pub const DEFAULT_MIGRATIONS_DIR: &str = "migrations";

// ---------------------------------------------------------------------------
// Database: pool configuration, registered with App::database

/// How to connect: URL, pool limits and migrations. Register it with
/// [`App::database`].
#[must_use]
pub struct Database<DB: sqlx::Database = DefaultDb> {
    url: Source,
    max_connections: Option<u32>,
    min_connections: u32,
    acquire_timeout: Duration,
    idle_timeout: Option<Duration>,
    migrate: bool,
    migrations: PathBuf,
    _db: PhantomData<fn() -> DB>,
}

enum Source {
    Url(String),
    Env(&'static str),
}

impl<DB: sqlx::Database> Database<DB> {
    fn with_source(url: Source) -> Self {
        Database {
            url,
            max_connections: None,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(5),
            idle_timeout: Some(Duration::from_secs(600)),
            migrate: true,
            migrations: PathBuf::from(DEFAULT_MIGRATIONS_DIR),
            _db: PhantomData,
        }
    }

    /// Maximum pool size. Defaults to 4 × the number of CPUs, at least 10.
    pub fn max_connections(mut self, n: u32) -> Self {
        self.max_connections = Some(n);
        self
    }

    /// Connections kept open even when idle (default 0).
    pub fn min_connections(mut self, n: u32) -> Self {
        self.min_connections = n;
        self
    }

    /// How long a request waits for a free connection before failing with
    /// 503 (default 5 s).
    pub fn acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = timeout;
        self
    }

    /// Run pending migrations at startup (default `true`).
    pub fn migrate(mut self, enabled: bool) -> Self {
        self.migrate = enabled;
        self
    }

    /// Read migrations from `dir` instead of `./migrations`. A missing
    /// directory means "no migrations".
    pub fn migrations(mut self, dir: impl Into<PathBuf>) -> Self {
        self.migrations = dir.into();
        self
    }

    fn resolve_url(&self) -> crate::Result<String> {
        match &self.url {
            Source::Url(url) => Ok(url.clone()),
            Source::Env(var) => std::env::var(var).map_err(|_| {
                Error::internal(format!(
                    "the {var} environment variable is not set; set it to your database URL \
                     (for example sqlite://app.db?mode=rwc or postgres://user:pass@localhost/app)"
                ))
            }),
        }
    }

    async fn connect(self) -> crate::Result<Pool<DB>>
    where
        DB::Connection: sqlx::migrate::Migrate,
    {
        let url = self.resolve_url()?;
        let max = self.max_connections.unwrap_or_else(|| {
            let cpus = std::thread::available_parallelism().map_or(4, |n| n.get() as u32);
            (cpus * 4).max(10)
        });
        let pool = PoolOptions::<DB>::new()
            .max_connections(max)
            .min_connections(self.min_connections)
            .acquire_timeout(self.acquire_timeout)
            .idle_timeout(self.idle_timeout)
            .connect(&url)
            .await
            .map_err(|e| Error::internal(format!("cannot connect to the database: {e}")))?;
        if self.migrate {
            run_migrations(&pool, &self.migrations).await?;
        }
        Ok(pool)
    }
}

// Constructors live on the default backend so `Database::from_env()` needs
// no type annotation; `Database::sqlite(..)` / `Database::postgres(..)` pick a
// backend explicitly when both features are on.
impl Database {
    /// Read the URL from `DATABASE_URL` at startup (the sqlx convention).
    pub fn from_env() -> Self {
        Self::from_env_var("DATABASE_URL")
    }

    /// Read the URL from the `var` environment variable at startup. Startup
    /// fails with a clear message when it is missing.
    pub fn from_env_var(var: &'static str) -> Self {
        Self::with_source(Source::Env(var))
    }

    /// Connect to `url` (`postgres://user:pass@host/db`,
    /// `sqlite://app.db?mode=rwc`).
    pub fn url(url: impl Into<String>) -> Self {
        Self::with_source(Source::Url(url.into()))
    }
}

#[cfg(feature = "sqlite")]
impl Database<sqlx::Sqlite> {
    /// A private in-memory SQLite database, with migrations applied: each
    /// call (each app, each test) gets its own empty database that lives as
    /// long as the app.
    pub fn sqlite_memory() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("zapline-{}-{n}", std::process::id());
        let mut db = Self::with_source(Source::Url(format!(
            "sqlite:file:{name}?mode=memory&cache=shared"
        )));
        // An in-memory database disappears with its last connection: keep
        // one open for the lifetime of the pool.
        db.min_connections = 1;
        db.idle_timeout = None;
        db
    }

    /// A SQLite database at `url` (`sqlite://app.db?mode=rwc`).
    pub fn sqlite(url: impl Into<String>) -> Self {
        Self::with_source(Source::Url(url.into()))
    }
}

#[cfg(feature = "postgres")]
impl Database<sqlx::Postgres> {
    /// A Postgres database at `url` (`postgres://user:pass@host/db`).
    pub fn postgres(url: impl Into<String>) -> Self {
        Self::with_source(Source::Url(url.into()))
    }
}

async fn run_migrations<DB: sqlx::Database>(
    pool: &Pool<DB>,
    dir: &std::path::Path,
) -> crate::Result<()>
where
    DB::Connection: sqlx::migrate::Migrate,
{
    if !dir.is_dir() {
        return Ok(());
    }
    let migrator = sqlx::migrate::Migrator::new(dir.to_path_buf())
        .await
        .map_err(|e| {
            Error::internal(format!("cannot read migrations in {}: {e}", dir.display()))
        })?;
    migrator
        .run(pool)
        .await
        .map_err(|e| Error::internal(format!("migration failed: {e}")))?;
    Ok(())
}

impl App {
    /// Connect to a database at startup, apply pending migrations, and make
    /// the pool available to handlers as [`Db`] (and as `State<Pool<DB>>`).
    ///
    /// Startup fails if the database can't be reached or a migration fails.
    pub fn database<DB>(self, database: Database<DB>) -> Self
    where
        DB: sqlx::Database,
        DB::Connection: sqlx::migrate::Migrate,
    {
        self.on_startup(move |res: Resources| async move {
            let pool = database.connect().await?;
            if std::env::var_os(ENV_MIGRATE_ONLY).is_some() {
                println!("migrations are up to date");
                std::process::exit(0);
            }
            res.insert(pool);
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Db extractor

/// The connection pool, as a handler argument. Dereferences to
/// [`sqlx::Pool`], so pass `&*db` wherever sqlx wants an executor, or call
/// `db.begin().await?` for a transaction.
pub struct Db<DB: sqlx::Database = DefaultDb>(pub Pool<DB>);

/// [`Db`] for SQLite, for apps (and tests) that enable both backends.
#[cfg(feature = "sqlite")]
pub type SqliteDb = Db<sqlx::Sqlite>;

/// [`Db`] for Postgres.
#[cfg(feature = "postgres")]
pub type PgDb = Db<sqlx::Postgres>;

impl<DB: sqlx::Database> Clone for Db<DB> {
    fn clone(&self) -> Self {
        Db(self.0.clone())
    }
}

impl<DB: sqlx::Database> Deref for Db<DB> {
    type Target = Pool<DB>;
    fn deref(&self) -> &Pool<DB> {
        &self.0
    }
}

impl<DB: sqlx::Database> FromRequestParts for Db<DB> {
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        match req.state::<Pool<DB>>() {
            Some(pool) => Ok(Db(pool.clone())),
            None => Err(Error::internal(format!(
                "no {} database registered: add `.database(Database::from_env())` to the app \
                 (with both the postgres and sqlite features on, `Db` means Postgres; \
                 use `SqliteDb` for SQLite)",
                DB::NAME
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Health check

/// Readiness check that pings the database:
/// `Health::new().check("database", DbCheck::new())`.
pub struct DbCheck<DB: sqlx::Database = DefaultDb>(PhantomData<fn() -> DB>);

impl DbCheck {
    /// A check for the default backend ([`DefaultDb`]). For another backend
    /// use `DbCheck::<sqlx::Sqlite>::default()`.
    pub fn new() -> Self {
        DbCheck(PhantomData)
    }
}

impl<DB: sqlx::Database> Default for DbCheck<DB> {
    fn default() -> Self {
        DbCheck(PhantomData)
    }
}

impl<DB: sqlx::Database> HealthCheck for DbCheck<DB> {
    async fn check(&self, res: Resources) -> crate::Result<()> {
        let pool = res
            .get::<Pool<DB>>()
            .ok_or_else(|| Error::service_unavailable("database not initialized"))?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|_| Error::service_unavailable("no database connection available"))?;
        conn.ping()
            .await
            .map_err(|_| Error::service_unavailable("database did not answer"))
    }
}

// ---------------------------------------------------------------------------
// Errors

/// How `?` turns a [`sqlx::Error`] into an HTTP error (called by the
/// blanket `From` impl on [`Error`]).
pub(crate) fn from_sqlx(err: sqlx::Error) -> Error {
    match &err {
        sqlx::Error::RowNotFound => Error::not_found(),
        sqlx::Error::PoolTimedOut => {
            Error::service_unavailable("database busy").with_code("db_pool_timeout")
        }
        sqlx::Error::Database(db) => match db.kind() {
            sqlx::error::ErrorKind::UniqueViolation => {
                Error::conflict("already exists").with_code("unique_violation")
            }
            sqlx::error::ErrorKind::ForeignKeyViolation => Error::conflict(Cow::Borrowed(
                "referenced row does not exist or is still in use",
            ))
            .with_code("foreign_key_violation"),
            sqlx::error::ErrorKind::NotNullViolation | sqlx::error::ErrorKind::CheckViolation => {
                Error::unprocessable("value violates a database constraint")
                    .with_code("constraint_violation")
            }
            _ => internal(err),
        },
        _ => internal(err),
    }
}

fn internal(err: sqlx::Error) -> Error {
    Error::internal("database error").with_source(err)
}
