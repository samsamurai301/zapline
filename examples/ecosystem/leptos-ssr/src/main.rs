use zapline::db::Database;

fn main() -> std::io::Result<()> {
    zapline::telemetry::init();
    leptos_ssr::app(Database::from_env_or("sqlite://notes.db?mode=rwc")).run("0.0.0.0:8080")
}
