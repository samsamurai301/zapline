fn main() -> std::io::Result<()> {
    zapline::telemetry::init();
    // A path to a SQLite file; created on first run.
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "shop.db".into());
    diesel_shop::app(url).run("0.0.0.0:8080")
}
