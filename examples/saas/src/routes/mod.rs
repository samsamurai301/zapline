//! HTTP routes, one module per resource.
//!
//! `zapline generate resource <name>` creates a new module and registers it
//! between the markers below. Each module exposes `mount(app) -> App`.

use zapline::prelude::*;

pub mod auth;
pub mod todos;

/// Registers every route module on the app.
pub fn install(mut app: App) -> App {
    // zapline:routes:start
    app = app.plugin(todos::mount);
    app = app.plugin(auth::mount);
    // zapline:routes:end
    app
}
