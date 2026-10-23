use actix_web::{App, HttpServer};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let addr = std::env::var("ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    HttpServer::new(|| App::new().configure(actix_minimal::app))
        .bind(addr)?
        .run()
        .await
}
