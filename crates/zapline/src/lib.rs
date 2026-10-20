//! # zapline
//!
//! A fast, low-boilerplate web framework for Rust.
//!
//! ```no_run
//! use zapline::prelude::*;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Serialize, Deserialize)]
//! struct User { id: u64, name: String }
//!
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
//!     App::new().mount((get_user, create_user)).run("0.0.0.0:8080")
//! }
//! ```

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
pub use handler::{Endpoint, Handler};
pub use lifecycle::Resources;
pub use request::{ConnInfo, Request};
pub use response::{Html, IntoResponse, Redirect, Response};
pub use router::{IntoRoutes, RouteInfo, Routes};
pub use server::ServerConfig;

pub use bytes::{self, Bytes};
pub use http::{self, header, HeaderMap, HeaderValue, Method, StatusCode, Uri};

#[cfg(feature = "macros")]
pub use zapline_macros::{delete, get, head, options, patch, post, put, route};

/// Everything you need for a typical app: `use zapline::prelude::*;`
pub mod prelude {
    #[cfg(feature = "form")]
    pub use crate::extract::Form;
    #[cfg(feature = "json")]
    pub use crate::extract::Json;
    #[cfg(feature = "query")]
    pub use crate::extract::Query;
    pub use crate::extract::{Extension, MatchedPath, Path, State};
    pub use crate::middleware::Next;
    #[cfg(feature = "macros")]
    pub use crate::{delete, get, patch, post, put, route};
    pub use crate::{
        App, Bytes, ConnInfo, Error, HeaderMap, HeaderValue, Html, IntoResponse, Method, Plugin,
        Redirect, Request, Resources, Response, Result, Routes, StatusCode, Uri,
    };
    // WP prelude additions are applied during integration.
}

#[cfg(feature = "json")]
pub use extract::Json;
