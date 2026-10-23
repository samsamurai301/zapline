//! HTTP routes, one module per resource.

use actix_web::web;

pub mod health;
pub mod users;

/// Registers every route module.
pub fn install(cfg: &mut web::ServiceConfig) {
    health::mount(cfg);
    users::mount(cfg);
}
