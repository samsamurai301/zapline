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

mod app;
mod body;
mod error;
pub mod extract;
mod handler;
pub mod middleware;
mod request;
mod response;
mod router;
mod server;
pub mod test;

pub use app::{App, ServerConfig, DEFAULT_BODY_LIMIT};
pub use body::{Body, BoxError};
pub use error::{Error, Result};
pub use handler::{Endpoint, Handler};
pub use request::Request;
pub use response::{Html, IntoResponse, Redirect, Response};
pub use router::{IntoRoutes, Routes};

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
    pub use crate::extract::{Extension, Path, State};
    pub use crate::middleware::Next;
    #[cfg(feature = "macros")]
    pub use crate::{delete, get, patch, post, put, route};
    pub use crate::{
        App, Bytes, Error, HeaderMap, HeaderValue, Html, IntoResponse, Method, Redirect, Request,
        Response, Result, Routes, StatusCode, Uri,
    };
}

#[cfg(feature = "json")]
pub use extract::Json;
