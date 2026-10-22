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
    EnvOr(&'static str, String),
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
    ///
    /// With Postgres on the thread-per-core runtime (the default on Linux)
    /// each worker thread gets its own pool of `n / workers` connections
    /// (rounded up), so a request's database I/O never leaves its core.
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
            Source::EnvOr(var, fallback) => {
                Ok(std::env::var(var).unwrap_or_else(|_| fallback.clone()))
            }
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

    /// `DATABASE_URL` if set, otherwise `fallback`: zero-setup local
    /// development, e.g. `Database::from_env_or("sqlite://app.db?mode=rwc")`.
    pub fn from_env_or(fallback: impl Into<String>) -> Self {
        Self::with_source(Source::EnvOr("DATABASE_URL", fallback.into()))
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
            res.insert(WorkerPools {
                shared: pool.clone(),
                key: (DB::NAME != "SQLite").then(crate::server::local::key),
            });
            res.insert(pool);
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Per-worker pools

/// The pool created at startup, plus one pool per thread-per-core worker.
///
/// Connections are registered with the runtime that opened them. Shared by
/// per-core workers, a connection opened on one worker (or on the startup
/// runtime) is woken by that runtime's I/O driver, so every query would hop
/// threads. Each worker instead lazily opens its own pool with the same
/// options and a share of `max_connections`.
struct WorkerPools<DB: sqlx::Database> {
    shared: Pool<DB>,
    /// `None`: always use the shared pool. SQLite runs queries on its own
    /// threads, so splitting its pool would only shrink it.
    key: Option<u64>,
}

impl<DB: sqlx::Database> Clone for WorkerPools<DB> {
    fn clone(&self) -> Self {
        WorkerPools {
            shared: self.shared.clone(),
            key: self.key,
        }
    }
}

impl<DB: sqlx::Database> WorkerPools<DB> {
    /// This worker's pool, or the shared one outside per-core workers.
    #[inline]
    fn get(&self) -> Pool<DB> {
        let Some(key) = self.key else {
            return self.shared.clone();
        };
        crate::server::local::get_or_init(key, |workers| {
            let opts = self.shared.options();
            let max = opts.get_max_connections().div_ceil(workers as u32).max(1);
            let min = opts.get_min_connections().div_ceil(workers as u32).min(max);
            opts.clone()
                .max_connections(max)
                .min_connections(min)
                .connect_lazy_with((*self.shared.connect_options()).clone())
        })
        .unwrap_or_else(|| self.shared.clone())
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
        match req.state::<WorkerPools<DB>>() {
            Some(pools) => Ok(Db(pools.get())),
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

// ---------------------------------------------------------------------------
// Sessions in the database

#[cfg(feature = "cookies")]
pub use self::sessions::{SessionBackend, SqlSessionStore};

#[cfg(feature = "cookies")]
mod sessions {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde_json::{Map, Value};
    use sqlx::Pool;

    use super::DefaultDb;
    use crate::session::{SessionLayer, SessionRecord, SessionStore};
    use crate::{App, Error, Resources, Result};

    /// Created at startup if missing; works on SQLite and Postgres.
    const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS zapline_sessions (\
        id TEXT PRIMARY KEY, data TEXT NOT NULL, expires_at BIGINT NOT NULL)";

    /// Expired rows are swept on every this-many saves.
    const SWEEP_EVERY: u32 = 256;

    /// A [`SessionStore`] in the app's database (table `zapline_sessions`,
    /// created at startup), so sessions survive restarts and are shared by
    /// every instance. Install it with [`App::database_sessions`].
    pub struct SqlSessionStore<DB: sqlx::Database = DefaultDb> {
        pool: Arc<OnceLock<Pool<DB>>>,
        saves: Arc<AtomicU32>,
    }

    impl<DB: sqlx::Database> Clone for SqlSessionStore<DB> {
        fn clone(&self) -> Self {
            SqlSessionStore {
                pool: Arc::clone(&self.pool),
                saves: Arc::clone(&self.saves),
            }
        }
    }

    impl<DB: sqlx::Database> std::fmt::Debug for SqlSessionStore<DB> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SqlSessionStore")
                .field("connected", &self.pool.get().is_some())
                .finish()
        }
    }

    impl<DB: sqlx::Database> SqlSessionStore<DB> {
        /// A store on `pool` (normally [`App::database_sessions`] does this).
        pub fn new(pool: Pool<DB>) -> Self {
            let store = Self::unbound();
            let _ = store.pool.set(pool);
            store
        }

        fn unbound() -> Self {
            SqlSessionStore {
                pool: Arc::new(OnceLock::new()),
                saves: Arc::new(AtomicU32::new(0)),
            }
        }

        fn pool(&self) -> Result<&Pool<DB>> {
            self.pool.get().ok_or_else(|| {
                Error::internal("the session store is not connected: the app has not started")
            })
        }
    }

    fn unix(t: SystemTime) -> i64 {
        t.duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }

    fn record(data: &str, expires_at: i64) -> Result<SessionRecord> {
        let data: Map<String, Value> = serde_json::from_str(data)
            .map_err(|e| Error::internal("corrupt session data").with_source(e))?;
        let secs = u64::try_from(expires_at).unwrap_or(0);
        Ok(SessionRecord {
            data,
            expires_at: UNIX_EPOCH + Duration::from_secs(secs),
        })
    }

    mod sealed {
        pub trait Sealed {}
    }

    /// A database that can hold sessions: SQLite and Postgres.
    pub trait SessionBackend: sqlx::Database + sealed::Sealed
    where
        SqlSessionStore<Self>: SessionStore,
    {
        /// Create the sessions table if it doesn't exist.
        #[doc(hidden)]
        fn create_table(pool: &Pool<Self>) -> impl std::future::Future<Output = Result<()>> + Send;
    }

    macro_rules! store_for {
        ($db:ty) => {
            impl sealed::Sealed for $db {}

            impl SessionBackend for $db {
                async fn create_table(pool: &Pool<Self>) -> Result<()> {
                    sqlx::query(CREATE_TABLE).execute(pool).await?;
                    Ok(())
                }
            }

            impl SessionStore for SqlSessionStore<$db> {
                async fn load(&self, id: &str) -> Result<Option<SessionRecord>> {
                    let row: Option<(String, i64)> = sqlx::query_as(
                        "SELECT data, expires_at FROM zapline_sessions WHERE id = $1 AND expires_at > $2",
                    )
                    .bind(id)
                    .bind(unix(SystemTime::now()))
                    .fetch_optional(self.pool()?)
                    .await?;
                    row.map(|(data, exp)| record(&data, exp)).transpose()
                }

                async fn save(&self, id: &str, record: &SessionRecord) -> Result<()> {
                    let pool = self.pool()?;
                    let data = serde_json::to_string(&record.data)
                        .map_err(|e| Error::internal("cannot encode the session").with_source(e))?;
                    sqlx::query(
                        "INSERT INTO zapline_sessions (id, data, expires_at) VALUES ($1, $2, $3) \
                         ON CONFLICT (id) DO UPDATE SET data = excluded.data, expires_at = excluded.expires_at",
                    )
                    .bind(id)
                    .bind(data)
                    .bind(unix(record.expires_at))
                    .execute(pool)
                    .await?;
                    if self.saves.fetch_add(1, Ordering::Relaxed) % SWEEP_EVERY == SWEEP_EVERY - 1 {
                        sqlx::query("DELETE FROM zapline_sessions WHERE expires_at <= $1")
                            .bind(unix(SystemTime::now()))
                            .execute(pool)
                            .await?;
                    }
                    Ok(())
                }

                async fn delete(&self, id: &str) -> Result<()> {
                    sqlx::query("DELETE FROM zapline_sessions WHERE id = $1")
                        .bind(id)
                        .execute(self.pool()?)
                        .await?;
                    Ok(())
                }
            }
        };
    }

    #[cfg(feature = "sqlite")]
    store_for!(sqlx::Sqlite);
    #[cfg(feature = "postgres")]
    store_for!(sqlx::Postgres);

    impl App {
        /// Server-side sessions stored in the database registered with
        /// [`App::database`] (call that first): table `zapline_sessions`,
        /// created at startup. `configure` adjusts the [`SessionLayer`]
        /// (cookie name, lifetime, `secure`...). Handlers take
        /// [`Session`](crate::session::Session) as usual.
        ///
        /// A cookie key must be registered too, e.g.
        /// [`key_from_env`](crate::cookies::key_from_env).
        ///
        /// ```no_run
        /// # #[cfg(feature = "sqlite")] {
        /// use zapline::cookies::key_from_env;
        /// use zapline::db::Database;
        /// use zapline::prelude::*;
        ///
        /// # fn main() -> zapline::Result<()> {
        /// let app = App::new()
        ///     .database(Database::from_env())
        ///     .cookie_key(key_from_env("SESSION_SECRET")?)
        ///     .database_sessions(|layer| layer.secure(!cfg!(debug_assertions)));
        /// # Ok(()) }
        /// # }
        /// ```
        pub fn database_sessions(
            self,
            configure: impl FnOnce(SessionLayer) -> SessionLayer,
        ) -> Self {
            self.database_sessions_in::<DefaultDb>(configure)
        }

        /// [`database_sessions`](Self::database_sessions) on a database
        /// other than [`DefaultDb`] (for apps with both backends enabled).
        pub fn database_sessions_in<DB: SessionBackend>(
            self,
            configure: impl FnOnce(SessionLayer) -> SessionLayer,
        ) -> Self
        where
            SqlSessionStore<DB>: SessionStore,
        {
            let store = SqlSessionStore::<DB>::unbound();
            let bind = store.clone();
            self.on_startup(move |res: Resources| async move {
                let pool: Pool<DB> = res.get().ok_or_else(|| {
                    Error::internal(
                        "database_sessions needs a database: call `.database(..)` before `.database_sessions(..)`",
                    )
                })?;
                DB::create_table(&pool).await?;
                let _ = bind.pool.set(pool);
                Ok(())
            })
            .wrap(configure(SessionLayer::new(store)))
        }
    }
}
