use zapline::db::Database;

fn main() -> std::io::Result<()> {
    zapline::telemetry::init();
    // DATABASE_URL, or a file next to the crate. Migrations in `migrations/`
    // are applied at startup.
    sqlx_transactions::app(Database::from_env_or("sqlite://ledger.db?mode=rwc")).run("0.0.0.0:8080")
}
