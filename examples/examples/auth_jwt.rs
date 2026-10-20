//! JWT authentication with role- and scope-based authorization.
//!
//!     cargo run -p zapline-examples --example auth_jwt
//!
//!     # get a token (admin / admin-pass, or ada / ada-pass)
//!     TOKEN=$(curl -s localhost:8080/login -H 'content-type: application/json' \
//!         -d '{"username":"admin","password":"admin-pass"}' | sed 's/.*"token":"\([^"]*\)".*/\1/')
//!     curl localhost:8080/api/me -H "authorization: Bearer $TOKEN"
//!     curl -X DELETE localhost:8080/api/users/7 -H "authorization: Bearer $TOKEN"   # admins only
//!     curl localhost:8080/api/reports -H "authorization: Bearer $TOKEN"            # needs scope
//!     curl localhost:8080/api/me                                                   # 401

use std::time::Duration;

use serde::{Deserialize, Serialize};
use zapline::auth::{constant_time_eq, Authenticate, Claims, CurrentUser, JwtConfig};
use zapline::prelude::*;

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct TokenResponse {
    token: String,
    token_type: &'static str,
    expires_in: u64,
}

const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// Demo user database: (name, password, roles, scopes).
const USERS: &[(&str, &str, &[&str], &[&str])] = &[
    ("admin", "admin-pass", &["admin", "user"], &["reports:read"]),
    ("ada", "ada-pass", &["user"], &[]),
];

async fn login(
    State(jwt): State<JwtConfig>,
    Json(creds): Json<Credentials>,
) -> Result<Json<TokenResponse>> {
    // Compare every candidate in constant time; real apps verify a password
    // hash (argon2, bcrypt) instead.
    let user = USERS.iter().find(|(name, pass, ..)| {
        constant_time_eq(name, &creds.username) & constant_time_eq(pass, &creds.password)
    });
    let Some((name, _, roles, scopes)) = user else {
        return Err(Error::unauthorized().with_code("invalid_credentials"));
    };
    let claims = Claims::new(*name)
        .roles(roles.iter().copied())
        .scopes(scopes.iter().copied());
    Ok(Json(TokenResponse {
        token: jwt.encode(&claims)?,
        token_type: "Bearer",
        expires_in: TOKEN_TTL.as_secs(),
    }))
}

async fn me(CurrentUser(claims): CurrentUser<Claims>) -> Json<Claims> {
    Json(claims)
}

async fn delete_user(Path(id): Path<u64>, CurrentUser(admin): CurrentUser<Claims>) -> String {
    format!("user {id} deleted by {}", admin.sub)
}

async fn reports() -> &'static str {
    "quarterly numbers"
}

pub fn app() -> App {
    let secret = std::env::var("JWT_SECRET")
        .unwrap_or_else(|_| "dev-only-secret-change-me-in-production".into());

    let api = Routes::new()
        .get("/me", me)
        .delete("/users/{id}", delete_user)
        .require_roles(["admin"])
        .get("/reports", reports)
        .require_scopes(["reports:read"])
        // Every /api route needs a valid token; role/scope policies are enforced too.
        .wrap(Authenticate::jwt());

    App::new()
        .jwt(
            JwtConfig::hs256(secret)
                .issuer("zapline-example")
                .ttl(TOKEN_TTL),
        )
        .post("/login", login)
        .nest("/api", api)
}

fn main() -> std::io::Result<()> {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    println!("listening on http://localhost:{port}");
    app().run(format!("0.0.0.0:{port}"))
}
