fn main() -> std::io::Result<()> {
    zapline::telemetry::init();
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://blog.db?mode=rwc".into());
    seaorm_blog::app(seaorm_blog::connect_options(url)).run("0.0.0.0:8080")
}
