//! The axum equivalent of `zapline new --template api`.

pub mod models;
pub mod routes;

use axum::http::{header, HeaderName, HeaderValue};
use axum::routing::get;
use axum::{Json, Router};
use tower_http::set_header::SetResponseHeaderLayer;
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

fn security_header(name: HeaderName, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value))
}

/// Builds the application: OpenAPI docs plus every route module.
pub fn app() -> Router {
    let spec = ApiDoc::openapi();
    routes::install(Router::new())
        .route("/openapi.json", get(move || async move { Json(spec) }))
        .merge(Scalar::with_url("/docs", ApiDoc::openapi()))
        .layer(security_header(header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
        .layer(security_header(header::X_FRAME_OPTIONS, "DENY"))
        .layer(security_header(
            header::REFERRER_POLICY,
            "strict-origin-when-cross-origin",
        ))
}
