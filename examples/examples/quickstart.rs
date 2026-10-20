//! The README's opening example, kept compiling.
//!
//!     cargo run -p zapline-examples --example quickstart

use serde::{Deserialize, Serialize};
use zapline::prelude::*;

#[derive(Serialize, Deserialize, JsonSchema, Validate)]
struct NewUser {
    #[validate(length(min = 1, max = 64))]
    name: String,
    #[validate(email)]
    email: String,
}

/// Create a user.
#[post("/users", tag = "users", status = 201)]
async fn create_user(Valid(Json(user)): Valid<Json<NewUser>>) -> (StatusCode, Json<NewUser>) {
    (StatusCode::CREATED, Json(user))
}

fn main() -> std::io::Result<()> {
    App::new()
        .mount(create_user)
        .openapi(OpenApi::new("Users API", "1.0.0")) // /openapi.json + Swagger UI at /docs
        .run("0.0.0.0:8080")
}
