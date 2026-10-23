//! HTTP routes, one module per resource.

use axum::Router;

pub mod health;
pub mod users;

/// Registers every route module on the router.
pub fn install(router: Router) -> Router {
    let router = health::mount(router);
    users::mount(router)
}
