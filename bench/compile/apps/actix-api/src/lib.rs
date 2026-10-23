//! The actix-web equivalent of `zapline new --template api`.

pub mod models;
pub mod routes;

use actix_web::middleware::DefaultHeaders;
use actix_web::{web, HttpResponse};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

#[derive(OpenApi)]
#[openapi(
    paths(
        routes::health::health,
        routes::users::list_users,
        routes::users::get_user,
        routes::users::create_user,
        routes::users::replace_user,
        routes::users::delete_user,
    ),
    components(schemas(models::User, models::NewUser, routes::health::HealthStatus))
)]
struct ApiDoc;

/// Browser hardening headers (nosniff, no framing, referrer policy).
pub fn security_headers() -> DefaultHeaders {
    DefaultHeaders::new()
        .add(("x-content-type-options", "nosniff"))
        .add(("x-frame-options", "DENY"))
        .add(("referrer-policy", "strict-origin-when-cross-origin"))
}

/// Registers the OpenAPI docs and every route module.
pub fn configure(cfg: &mut web::ServiceConfig) {
    let spec = ApiDoc::openapi();
    cfg.route(
        "/openapi.json",
        web::get().to(move || {
            let spec = spec.clone();
            async move { HttpResponse::Ok().json(spec) }
        }),
    )
    .service(Scalar::with_url("/docs", ApiDoc::openapi()));
    routes::install(cfg);
}
