use actix_web::{error, web, App, HttpRequest, HttpResponse, HttpServer};
use bench_servers as tfb;
use serde::Serialize;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Serialize)]
struct Message {
    message: &'static str,
}

#[derive(Serialize)]
struct User {
    id: u64,
    name: &'static str,
}

async fn db(pool: web::Data<PgPool>) -> actix_web::Result<web::Json<tfb::World>> {
    let w = tfb::single_query(&pool)
        .await
        .map_err(error::ErrorInternalServerError)?;
    Ok(web::Json(w))
}

async fn queries(
    pool: web::Data<PgPool>,
    req: HttpRequest,
) -> actix_web::Result<web::Json<Vec<tfb::World>>> {
    let n = tfb::query_count(Some(req.query_string()));
    let w = tfb::multiple_queries(&pool, n)
        .await
        .map_err(error::ErrorInternalServerError)?;
    Ok(web::Json(w))
}

async fn updates(
    pool: web::Data<PgPool>,
    req: HttpRequest,
) -> actix_web::Result<web::Json<Vec<tfb::World>>> {
    let n = tfb::query_count(Some(req.query_string()));
    let w = tfb::updates(&pool, n)
        .await
        .map_err(error::ErrorInternalServerError)?;
    Ok(web::Json(w))
}

async fn fortunes(pool: web::Data<PgPool>) -> actix_web::Result<HttpResponse> {
    let html = tfb::fortunes(&pool)
        .await
        .map_err(error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(html))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // NO_DB=1 serves only the non-database endpoints (no Postgres needed).
    let pool = if std::env::var("NO_DB").is_ok_and(|v| v == "1") {
        None
    } else {
        Some(
            PgPoolOptions::new()
                .max_connections(tfb::pool_size())
                .connect(&tfb::database_url())
                .await
                .expect("connect to Postgres"),
        )
    };
    HttpServer::new(move || {
        let app = App::new()
            .route("/plaintext", web::get().to(|| async { "Hello, World!" }))
            .route(
                "/json",
                web::get().to(|| async {
                    web::Json(Message {
                        message: "Hello, World!",
                    })
                }),
            )
            .route(
                "/user/{id}",
                web::get().to(|id: web::Path<u64>| async move {
                    web::Json(User {
                        id: id.into_inner(),
                        name: "ada",
                    })
                }),
            );
        match &pool {
            Some(pool) => app
                .app_data(web::Data::new(pool.clone()))
                .route("/db", web::get().to(db))
                .route("/queries", web::get().to(queries))
                .route("/updates", web::get().to(updates))
                .route("/fortunes", web::get().to(fortunes)),
            None => app,
        }
    })
    .workers(tfb::workers())
    .backlog(4096)
    .bind("0.0.0.0:3003")?
    .run()
    .await
}
