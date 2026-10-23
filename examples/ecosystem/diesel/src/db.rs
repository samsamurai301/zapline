//! Running blocking Diesel code from async handlers.
//!
//! Diesel's connections are synchronous. Calling them directly in a handler
//! would block a server thread for the length of the query, so every query
//! goes through [`DbPool::run`]: it borrows a connection from an r2d2 pool
//! on tokio's blocking thread pool and hands the closure's result back to
//! the handler. The server threads keep serving other requests meanwhile.

use std::time::Duration;

use diesel::connection::SimpleConnection;
use diesel::r2d2::{ConnectionManager, CustomizeConnection, Pool};
use diesel::result::{DatabaseErrorKind, Error as DieselError};
use diesel::SqliteConnection;
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use zapline::prelude::*;

/// The SQL files in `migrations/`, compiled into the binary.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// A pool of SQLite connections. Cheap to clone.
#[derive(Clone)]
pub struct DbPool(Pool<ConnectionManager<SqliteConnection>>);

impl DbPool {
    /// Opens a pool on a database file (`shop.db`) or `:memory:`, and applies
    /// pending migrations.
    ///
    /// An in-memory database belongs to one connection, so `:memory:` gets a
    /// pool of exactly one connection that is never recycled.
    pub async fn open(url: &str, max_connections: u32) -> Result<DbPool> {
        let memory = url == ":memory:";
        let mut builder = Pool::builder()
            .max_size(if memory { 1 } else { max_connections })
            .connection_timeout(Duration::from_secs(5))
            .connection_customizer(Box::new(Pragmas));
        if memory {
            builder = builder
                .min_idle(Some(1))
                .idle_timeout(None)
                .max_lifetime(None);
        }
        let manager = ConnectionManager::<SqliteConnection>::new(url);
        let url = url.to_owned();
        let pool = tokio::task::spawn_blocking(move || builder.build(manager))
            .await
            .map_err(|_| Error::internal("opening the database panicked"))?
            .map_err(|e| Error::internal(format!("cannot open {url}: {e}")))?;
        let pool = DbPool(pool);
        pool.run(|conn| {
            conn.run_pending_migrations(MIGRATIONS)
                .map(drop)
                .map_err(|e| DbError::Other(format!("migration failed: {e}")))
        })
        .await?;
        Ok(pool)
    }

    /// Runs `f` with a connection, on the blocking thread pool.
    ///
    /// No free connection within 5 seconds is a `503`; a panic inside `f` is a
    /// `500` (the server keeps running).
    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut SqliteConnection) -> Result<T, DbError> + Send + 'static,
    {
        let pool = self.0.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|_| DbError::PoolTimeout)?;
            f(&mut conn)
        })
        .await
        .map_err(|_| Error::internal("database task panicked"))?;
        Ok(result?)
    }
}

/// Settings applied to every new connection.
#[derive(Debug)]
struct Pragmas;

impl CustomizeConnection<SqliteConnection, diesel::r2d2::Error> for Pragmas {
    fn on_acquire(&self, conn: &mut SqliteConnection) -> Result<(), diesel::r2d2::Error> {
        // WAL lets readers run while one writer commits; writers wait up to
        // 5 s for each other instead of failing with "database is locked".
        conn.batch_execute(
            "PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;",
        )
        .map_err(diesel::r2d2::Error::QueryError)
    }
}

/// Errors from code that runs inside [`DbPool::run`]. Diesel errors convert
/// with `?`, and every variant converts into the right HTTP error.
#[derive(Debug)]
pub enum DbError {
    /// A Diesel error: unique violation, no row, ...
    Diesel(DieselError),
    /// A domain rule rejected the change; the transaction rolls back.
    Rejected(Error),
    /// No connection became free in time.
    PoolTimeout,
    /// Anything else; logged, not shown to the client.
    Other(String),
}

impl From<DieselError> for DbError {
    fn from(e: DieselError) -> Self {
        DbError::Diesel(e)
    }
}

impl From<Error> for DbError {
    fn from(e: Error) -> Self {
        DbError::Rejected(e)
    }
}

/// The same statuses and codes as zapline's sqlx integration.
impl From<DbError> for Error {
    fn from(e: DbError) -> Self {
        match e {
            DbError::Rejected(e) => e,
            DbError::PoolTimeout => {
                Error::service_unavailable("database busy").with_code("db_pool_timeout")
            }
            DbError::Other(message) => Error::internal(message),
            DbError::Diesel(DieselError::NotFound) => Error::not_found(),
            DbError::Diesel(DieselError::DatabaseError(kind, info)) => match kind {
                DatabaseErrorKind::UniqueViolation => {
                    Error::conflict("already exists").with_code("unique_violation")
                }
                DatabaseErrorKind::ForeignKeyViolation => {
                    Error::conflict("referenced row does not exist or is still in use")
                        .with_code("foreign_key_violation")
                }
                DatabaseErrorKind::NotNullViolation | DatabaseErrorKind::CheckViolation => {
                    Error::unprocessable("a value breaks a database constraint")
                        .with_code("constraint_violation")
                }
                _ => Error::internal(format!("database error: {}", info.message())),
            },
            DbError::Diesel(other) => Error::internal(format!("database error: {other}")),
        }
    }
}
