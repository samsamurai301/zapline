//! Data types shared by the routes.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use validator::Validate;

/// A user as stored and returned by the API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct User {
    pub id: u64,
    pub name: String,
    pub email: String,
}

/// Payload for creating or replacing a user.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, Validate)]
pub struct NewUser {
    #[validate(length(min = 1, max = 100))]
    pub name: String,
    #[validate(email)]
    pub email: String,
}

/// Query parameters for `GET /users`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct Pagination {
    /// Maximum number of users to return (default 50, at most 200).
    pub limit: Option<usize>,
    /// Number of users to skip (default 0).
    pub offset: Option<usize>,
}
