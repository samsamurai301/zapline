//! Liveness endpoint for load balancers and orchestrators.

use actix_web::{get, web};
use serde::Serialize;
use utoipa::ToSchema;

/// Body of `GET /health`.
#[derive(Serialize, ToSchema)]
pub struct HealthStatus {
    pub status: &'static str,
}

/// Liveness probe: answers 200 while the process is up.
#[utoipa::path(responses((status = 200, body = HealthStatus)))]
#[get("/health")]
pub async fn health() -> web::Json<HealthStatus> {
    web::Json(HealthStatus { status: "ok" })
}

/// Adds the health route.
pub fn mount(cfg: &mut web::ServiceConfig) {
    cfg.service(health);
}
