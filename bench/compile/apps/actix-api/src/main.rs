use actix_web::{App, HttpServer};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let addr = std::env::var("ADDR").unwrap_or_else(|_| {
        let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
        format!("0.0.0.0:{port}")
    });
    let store = actix_api::routes::users::UserStore::default();
    HttpServer::new(move || {
        App::new()
            .app_data(actix_web::web::Data::new(store.clone()))
            .wrap(actix_api::security_headers())
            .configure(actix_api::configure)
    })
    .bind(addr)?
    .run()
    .await
}
