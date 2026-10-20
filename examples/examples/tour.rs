//! The README's "A tour" section, kept compiling.
//!
//!     cargo run -p zapline-examples --example tour

use std::sync::Arc;
use std::time::Duration;
use zapline::auth::{Authenticate, Claims, CurrentUser, JwtConfig};
use zapline::health::Health;
use zapline::metrics::Metrics;
use zapline::middleware::{Cors, RateLimit, SecurityHeaders, SetRequestId};
use zapline::prelude::*;
use zapline::tasks::Schedule;

trait UserRepo: Send + Sync {
    fn name(&self, id: u64) -> Option<String>;
}

/// Look up a user by id.
#[get("/users/{id}", tag = "users")]
async fn get_user(repo: Dep<dyn UserRepo>, Path(id): Path<u64>) -> Result<String> {
    repo.name(id)
        .ok_or_else(|| Error::not_found().with_code("user_not_found"))
}

#[get("/me")]
async fn me(CurrentUser(claims): CurrentUser<Claims>) -> String {
    claims.sub
}

fn app(repo: Arc<dyn UserRepo>, jwt_secret: &str) -> App {
    let api = Routes::new()
        .mount((get_user, me))
        .delete("/users/{id}", || async { StatusCode::NO_CONTENT })
        .require_roles(["admin"]) // guard: fails closed
        .wrap(Authenticate::jwt()); // JWT for the whole group

    App::new()
        .provide_dyn::<dyn UserRepo>(repo) // dependency injection
        .jwt(JwtConfig::hs256(jwt_secret))
        .wrap(SetRequestId::new())
        .wrap(SecurityHeaders::default())
        .wrap(Cors::permissive())
        .wrap(RateLimit::per_ip(100, Duration::from_secs(60)))
        .nest("/api", api)
        .schedule(
            Schedule::every(Duration::from_secs(300)),
            |_res| async { /* cleanup */ },
        )
        .plugin(Health::new()) // /health/live, /health/ready
        .plugin(Metrics::new()) // /metrics
        .openapi(OpenApi::new("Tour", "1.0.0").bearer_auth("bearerAuth"))
}

#[derive(Default)]
struct MemRepo;

impl UserRepo for MemRepo {
    fn name(&self, id: u64) -> Option<String> {
        (id == 1).then(|| "ada".to_owned())
    }
}

fn main() -> std::io::Result<()> {
    app(Arc::new(MemRepo), "change-me").run("0.0.0.0:8080")
}
