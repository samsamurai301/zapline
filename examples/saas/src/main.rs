//! Binary entry point. Everything else lives in the library crate, so tests
//! can build the very same `App` in-process.

use zapline::db::Database;

fn main() -> std::io::Result<()> {
    // Override the bind address with e.g. `ADDR=127.0.0.1:3000 cargo run`,
    // or just the port with `PORT` (set by hosts such as Railway and Render).
    let addr = std::env::var("ADDR").unwrap_or_else(|_| {
        let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
        format!("0.0.0.0:{port}")
    });
    // DATABASE_URL (e.g. postgres://... with the `postgres` feature), or a
    // local SQLite file so the app runs with zero setup.
    let db = Database::from_env_or("sqlite://app.db?mode=rwc");
    saas::app(db).run(addr)
}
