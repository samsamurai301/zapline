//! Binary entry point. Everything else lives in the library crate, so tests
//! can build the very same `App` in-process.

use zapline::db::Database;

fn main() -> std::io::Result<()> {
    // Override the bind address with e.g. `ADDR=127.0.0.1:3000 cargo run`.
    let addr = std::env::var("ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    // DATABASE_URL (e.g. postgres://... with the `postgres` feature), or a
    // local SQLite file so the app runs with zero setup.
    let db = Database::from_env_or("sqlite://app.db?mode=rwc");
    saas::app(db).run(addr)
}
