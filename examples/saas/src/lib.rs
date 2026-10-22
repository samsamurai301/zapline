//! saas: a multi-user todo app showing a complete zapline stack.
//!
//! Accounts with password reset (`routes/auth.rs`), per-user data
//! (`routes/todos.rs`), a welcome email sent by a durable job
//! (`jobs/send_welcome.rs`), a scheduled cleanup, server-rendered HTML with
//! htmx, and a JSON API with OpenAPI docs at `/docs`.

pub mod jobs;
pub mod routes;

use std::time::Duration;

use zapline::db::{sqlx, Database, DbCheck, DefaultDb};
use zapline::openapi::OpenApi;
use zapline::prelude::*;
use zapline::tasks::Schedule;

/// Builds the application on top of `db`. `main` passes the real database;
/// tests pass `Database::sqlite_memory()`.
pub fn app(db: Database) -> App {
    App::new()
        // Routes, recent requests, jobs and mail at /_dev (debug builds only).
        .dev_console()
        .database(db)
        .jobs(jobs::register)
        .views("templates")
        .static_files("/static", ServeDir::new("static"))
        .plugin(Health::new().check("database", DbCheck::new()))
        .openapi(OpenApi::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
        .plugin(routes::install)
        .schedule(
            Schedule::every(Duration::from_secs(3600)).name("purge-expired-reset-links"),
            purge_expired_reset_links,
        )
}

/// Deletes password-reset links that can no longer be used.
async fn purge_expired_reset_links(res: Resources) -> Result<()> {
    let db: sqlx::Pool<DefaultDb> = res.require()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    sqlx::query("DELETE FROM password_resets WHERE expires_at <= $1")
        .bind(now)
        .execute(&db)
        .await?;
    Ok(())
}
