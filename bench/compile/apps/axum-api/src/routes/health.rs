//! Liveness endpoint for load balancers and orchestrators.

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

/// Body of `GET /health`.
#[derive(Serialize, ToSchema)]
pub struct HealthStatus {
    pub status: &'static str,
}

/// Liveness probe: answers 200 while the process is up.
#[utoipa::path(get, path = "/health", responses((status = 200, body = HealthStatus)))]
pub async fn health() -> Json<HealthStatus> {
    Json(HealthStatus { status: "ok" })
}

/// Adds the health route.
pub fn mount(router: Router) -> Router {
    router.route("/health", get(health))
}
