//! # zapline
//!
//! A fast, batteries-included web framework for Rust: FastAPI ergonomics,
//! enterprise structure and thread-per-core performance.
//!
//! Handlers are plain `async fn`s whose arguments are typed *extractors* and
//! whose return value is anything that implements [`IntoResponse`]. Route
//! macros document themselves into an OpenAPI spec, errors are one
//! [`Error`] type that `?` works with, and everything else (auth, sessions,
//! validation, DI, background jobs, metrics, tracing...) plugs in with one
//! line and costs nothing when unused.
//!
//! ```no_run
//! use serde::{Deserialize, Serialize};
//! use zapline::prelude::*;
//!
//! #[derive(Serialize, Deserialize, JsonSchema)]
//! struct User {
//!     id: u64,
//!     name: String,
//! }
//!
//! /// Fetch one user.
//! #[get("/users/{id}")]
//! async fn get_user(Path(id): Path<u64>) -> Json<User> {
//!     Json(User { id, name: "ada".into() })
//! }
//!
//! #[post("/users")]
//! async fn create_user(Json(user): Json<User>) -> (StatusCode, Json<User>) {
//!     (StatusCode::CREATED, Json(user))
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .mount((get_user, create_user))
//!         .openapi(OpenApi::new("Users", "1.0.0")) // GET /openapi.json and /docs
//!         .plugin(Health::new()) // GET /health, /health/live, /health/ready
//!         .run("0.0.0.0:8080")
//! }
//! ```
//!
//! `use zapline::prelude::*` brings in the app builder, extractors,
//! responses, route macros and the types most apps touch (see [`prelude`]).
//! Deriving [`JsonSchema`] or `Validate` expands to paths in `schemars` /
//! `validator`, so add those crates to your `Cargo.toml` when you derive them.
//!
//! # Where to start
//!
//! | Topic | Start here |
//! |---|---|
//! | Building and running an app | [`App`], [`Routes`], [`ServerConfig`] |
//! | Reading requests | [`extract`]: `Path`, `Query`, `Json`, `Form`, `State`, ...; [`stream::BodyStream`] |
//! | Writing responses | [`IntoResponse`], [`Html`], [`Redirect`], [`sse`], [`stream::StreamBody`] |
//! | Errors | [`Error`] (`{"error", "code", "details"}` JSON), [`App::error_handler`] |
//! | Middleware | [`middleware`]: CORS, timeouts, rate limiting, request IDs, compression, ... |
//! | API docs and validation | [`openapi`], [`validate`] |
//! | Security | [`auth`] (bearer, basic, API keys, JWT, guards, CSRF), [`cookies`], [`session`] |
//! | App structure | [`di`] (dependency injection), [`config`], [`tasks`], [`lifecycle`], [`Plugin`] |
//! | Realtime and content | [`ws`], [`sse`], [`stream`], [`multipart`], [`fs`] |
//! | Observability | [`telemetry`], [`metrics`], [`health`] |
//! | Testing | [`test`](mod@test): `app.test()` runs the whole pipeline in-process |
//!
//! # Cargo features
//!
//! | Feature | Default | Enables |
//! |---|:---:|---|
//! | `macros` | yes | Route attribute macros: [`get`], [`post`], [`route`], ... |
//! | `json` | yes | [`Json`], NDJSON streams, structured error `details` |
//! | `query` | yes | [`extract::Query`] |
//! | `form` | yes | [`Form`] |
//! | `openapi` | yes | [`openapi`]: OpenAPI 3.1 spec and Swagger UI / Scalar / Redoc pages |
//! | `validate` | yes | [`validate`]: `Valid<T>` with the `validator` crate |
//! | `config` | yes | [`config`]: layered TOML, `.env` and environment configuration |
//! | `tracing` | yes | [`telemetry`]: logging setup and per-request spans with `tracing` |
//! | `http2` | | HTTP/2 in the server |
//! | `tls` | | [`tls`]: HTTPS with rustls |
//! | `ws` | | [`ws`]: WebSockets |
//! | `fs` | | [`fs`]: static files with ranges, ETags and precompressed variants |
//! | `multipart` | | [`multipart`]: `multipart/form-data` uploads |
//! | `compression` | | [`middleware::Compression`]: gzip, brotli, zstd and deflate |
//! | `jwt` | | JSON Web Tokens: [`auth::Jwt`], [`auth::JwtConfig`], [`auth::Claims`] |
//! | `cookies` | | [`cookies`], [`session`] and [`auth::Csrf`] |
//! | `full` | | All of the above |
//!
//! Always available: routing, extractors, middleware, [`auth`] (bearer,
//! basic, API keys, guards), [`di`], [`tasks`], [`sse`], [`stream`],
//! [`health`], [`metrics`] and the [`test`](mod@test) client.

// ---- core -----------------------------------------------------------------
mod app;
mod body;
mod error;
pub mod extract;
mod handler;
pub mod lifecycle;
pub mod middleware;
mod request;
mod response;
mod router;
pub mod server;
pub mod test;

#[doc(hidden)]
pub mod __private;

// ---- API docs & validation ---------------------------------------------------
#[cfg(feature = "openapi")]
pub mod openapi;
#[cfg(feature = "validate")]
pub mod validate;

// ---- protocols & content -----------------------------------------------------
#[cfg(feature = "fs")]
pub mod fs;
#[cfg(feature = "multipart")]
pub mod multipart;
pub mod sse;
pub mod stream;
#[cfg(feature = "tls")]
pub mod tls;
#[cfg(feature = "ws")]
pub mod ws;

// ---- security ----------------------------------------------------------------
pub mod auth;
#[cfg(feature = "cookies")]
pub mod cookies;
#[cfg(feature = "cookies")]
pub mod session;

// ---- application structure ---------------------------------------------------
#[cfg(feature = "config")]
pub mod config;
pub mod di;
pub mod tasks;

// ---- observability -----------------------------------------------------------
pub mod health;
pub mod metrics;
#[cfg(feature = "tracing")]
pub mod telemetry;

pub use app::{App, Plugin, DEFAULT_BODY_LIMIT, ENV_PRINT_ROUTES};
pub use body::{Body, BoxError, ReqBody};
pub use error::{Error, ErrorReport, Result};
#[cfg(feature = "form")]
pub use extract::Form;
#[cfg(feature = "json")]
pub use extract::Json;
pub use handler::{BoxFuture, Endpoint, Handler};
pub use lifecycle::Resources;
#[cfg(feature = "openapi")]
pub use openapi::JsonSchema;
pub use request::{ConnInfo, Request};
pub use response::{Html, IntoResponse, Redirect, Response};
pub use router::{EndpointLayers, IntoRoutes, RouteInfo, Routes};
pub use server::{shutdown_signal, RuntimeMode, ServerConfig};
#[cfg(feature = "tls")]
pub use tls::TlsConfig;

pub use bytes::{self, Bytes};
pub use http::{self, header, HeaderMap, HeaderValue, Method, StatusCode, Uri};

#[cfg(feature = "macros")]
pub use zapline_macros::{delete, get, head, options, patch, post, put, route};

/// Everything a typical app needs: `use zapline::prelude::*;`
///
/// The core (app builder, extractors, responses, errors, route macros) plus
/// the most used type of each feature module, each behind its Cargo feature.
/// Names generic enough to clash with other crates (`sse::Event`,
/// `ws::Message`, `cookies::Key`, `config::Config`, `session::Session`...)
/// are left out: import them from their module.
///
/// It exports no crate names and nothing from serde, so it combines with
/// `use serde::{Deserialize, Serialize};` and std imports without conflicts.
/// Its `Result` is [`zapline::Result`](crate::Result), whose error type
/// defaults to [`Error`], so `Result<T, E>` keeps working.
pub mod prelude {
    // Core: app, routing, requests, responses, errors.
    #[cfg(feature = "form")]
    pub use crate::extract::Form;
    #[cfg(feature = "json")]
    pub use crate::extract::Json;
    #[cfg(feature = "query")]
    pub use crate::extract::Query;
    pub use crate::extract::{Extension, MatchedPath, Path, State};
    pub use crate::middleware::{Next, RequestId};
    #[cfg(feature = "macros")]
    pub use crate::{delete, get, head, options, patch, post, put, route};
    pub use crate::{
        header, App, Bytes, ConnInfo, Error, HeaderMap, HeaderValue, Html, IntoResponse, Method,
        Plugin, Redirect, Request, Resources, Response, Result, Routes, StatusCode, Uri,
    };

    // API docs and validation.
    #[cfg(feature = "openapi")]
    pub use crate::openapi::{DocsUi, JsonSchema, OpenApi};
    #[cfg(feature = "validate")]
    pub use crate::validate::{Valid, Validate};

    // Application structure.
    pub use crate::di::{Cached, Dep};
    pub use crate::tasks::{BackgroundTasks, Schedule};

    // Security.
    pub use crate::auth::{Authenticate, BearerToken, CurrentUser, HasRoles};
    #[cfg(feature = "jwt")]
    pub use crate::auth::{Claims, Jwt, JwtConfig};
    #[cfg(feature = "cookies")]
    pub use crate::cookies::CookieJar;

    // Realtime and content.
    #[cfg(feature = "fs")]
    pub use crate::fs::{ServeDir, ServeFile};
    #[cfg(feature = "multipart")]
    pub use crate::multipart::Multipart;
    pub use crate::sse::{KeepAlive, Sse};
    pub use crate::stream::{BodyStream, StreamBody};
    #[cfg(feature = "ws")]
    pub use crate::ws::{WebSocket, WebSocketUpgrade};

    // Observability.
    pub use crate::health::Health;
    pub use crate::metrics::Metrics;
    #[cfg(feature = "tracing")]
    pub use crate::telemetry::Trace;
}
