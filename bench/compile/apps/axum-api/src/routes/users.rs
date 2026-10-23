//! `/users` CRUD endpoints backed by an in-memory store.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde_json::json;
use validator::Validate;

use crate::models::{NewUser, Pagination, User};

/// Largest page `GET /users` returns.
const MAX_PAGE: usize = 200;

/// Errors a handler can return, rendered as JSON.
pub enum ApiError {
    NotFound,
    EmailTaken,
    BadJson(JsonRejection),
    Invalid(validator::ValidationErrors),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({"code": "not_found", "message": "not found"})),
            )
                .into_response(),
            ApiError::EmailTaken => (
                StatusCode::CONFLICT,
                Json(json!({"code": "email_taken", "message": "email already registered"})),
            )
                .into_response(),
            ApiError::BadJson(e) => (
                e.status(),
                Json(json!({"code": "bad_request", "message": e.body_text()})),
            )
                .into_response(),
            ApiError::Invalid(errors) => {
                let fields: Vec<_> = errors
                    .field_errors()
                    .into_iter()
                    .flat_map(|(field, errs)| {
                        errs.iter().map(
                            move |e| json!({"field": field, "code": e.code, "message": e.message}),
                        )
                    })
                    .collect();
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"code": "validation_failed", "errors": fields})),
                )
                    .into_response()
            }
        }
    }
}

/// `Json<T>` that also runs `T`'s validation rules.
pub struct ValidJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Validate> FromRequest<S> for ValidJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state)
            .await
            .map_err(ApiError::BadJson)?;
        value.validate().map_err(ApiError::Invalid)?;
        Ok(ValidJson(value))
    }
}

#[derive(Default)]
struct Inner {
    last_id: u64,
    users: BTreeMap<u64, User>,
}

impl Inner {
    fn email_taken(&self, email: &str, except: Option<u64>) -> bool {
        let mut others = self.users.values().filter(|u| Some(u.id) != except);
        others.any(|u| u.email == email)
    }
}

/// Thread-safe in-memory user store. Clones share the same data.
#[derive(Clone, Default)]
pub struct UserStore(Arc<RwLock<Inner>>);

impl UserStore {
    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }

    fn list(&self, offset: usize, limit: usize) -> Vec<User> {
        let inner = self.read();
        let page = inner.users.values().skip(offset).take(limit);
        page.cloned().collect()
    }

    fn get(&self, id: u64) -> Result<User, ApiError> {
        let inner = self.read();
        inner.users.get(&id).cloned().ok_or(ApiError::NotFound)
    }

    fn create(&self, new: NewUser) -> Result<User, ApiError> {
        let mut inner = self.write();
        if inner.email_taken(&new.email, None) {
            return Err(ApiError::EmailTaken);
        }
        inner.last_id += 1;
        let user = User {
            id: inner.last_id,
            name: new.name,
            email: new.email,
        };
        inner.users.insert(user.id, user.clone());
        Ok(user)
    }

    fn replace(&self, id: u64, new: NewUser) -> Result<User, ApiError> {
        let mut inner = self.write();
        if !inner.users.contains_key(&id) {
            return Err(ApiError::NotFound);
        }
        if inner.email_taken(&new.email, Some(id)) {
            return Err(ApiError::EmailTaken);
        }
        let user = User {
            id,
            name: new.name,
            email: new.email,
        };
        inner.users.insert(id, user.clone());
        Ok(user)
    }

    fn delete(&self, id: u64) -> Result<(), ApiError> {
        let removed = self.write().users.remove(&id);
        removed.map(|_| ()).ok_or(ApiError::NotFound)
    }
}

/// List users, optionally paginated with `?limit=` and `?offset=`.
#[utoipa::path(get, path = "/users", params(Pagination), responses((status = 200, body = Vec<User>)))]
pub async fn list_users(
    State(store): State<UserStore>,
    Query(page): Query<Pagination>,
) -> Json<Vec<User>> {
    let limit = page.limit.unwrap_or(50).min(MAX_PAGE);
    Json(store.list(page.offset.unwrap_or(0), limit))
}

/// Fetch one user by id.
#[utoipa::path(get, path = "/users/{id}", params(("id" = u64, Path)), responses((status = 200, body = User), (status = 404)))]
pub async fn get_user(
    State(store): State<UserStore>,
    Path(id): Path<u64>,
) -> Result<Json<User>, ApiError> {
    Ok(Json(store.get(id)?))
}

/// Create a user. Responds 409 if the email is already registered.
#[utoipa::path(post, path = "/users", request_body = NewUser, responses((status = 201, body = User), (status = 409), (status = 422)))]
pub async fn create_user(
    State(store): State<UserStore>,
    ValidJson(new): ValidJson<NewUser>,
) -> Result<(StatusCode, Json<User>), ApiError> {
    Ok((StatusCode::CREATED, Json(store.create(new)?)))
}

/// Replace a user.
#[utoipa::path(put, path = "/users/{id}", params(("id" = u64, Path)), request_body = NewUser, responses((status = 200, body = User), (status = 404), (status = 409), (status = 422)))]
pub async fn replace_user(
    State(store): State<UserStore>,
    Path(id): Path<u64>,
    ValidJson(new): ValidJson<NewUser>,
) -> Result<Json<User>, ApiError> {
    Ok(Json(store.replace(id, new)?))
}

/// Delete a user.
#[utoipa::path(delete, path = "/users/{id}", params(("id" = u64, Path)), responses((status = 204), (status = 404)))]
pub async fn delete_user(
    State(store): State<UserStore>,
    Path(id): Path<u64>,
) -> Result<StatusCode, ApiError> {
    store.delete(id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Adds the `/users` routes and their store.
pub fn mount(router: Router) -> Router {
    let users = Router::new()
        .route("/users", get(list_users).post(create_user))
        .route(
            "/users/{id}",
            get(get_user).put(replace_user).delete(delete_user),
        )
        .with_state(UserStore::default());
    router.merge(users)
}
