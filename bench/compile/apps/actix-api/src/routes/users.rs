//! `/users` CRUD endpoints backed by an in-memory store.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use actix_web::dev::Payload;
use actix_web::http::StatusCode;
use actix_web::{
    delete, get, post, put, web, FromRequest, HttpRequest, HttpResponse, ResponseError,
};
use serde::de::DeserializeOwned;
use serde_json::json;
use validator::Validate;

use crate::models::{NewUser, Pagination, User};

/// Largest page `GET /users` returns.
const MAX_PAGE: usize = 200;

/// Errors a handler can return, rendered as JSON.
#[derive(Debug)]
pub enum ApiError {
    NotFound,
    EmailTaken,
    BadJson(String),
    Invalid(validator::ValidationErrors),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::NotFound => f.write_str("not found"),
            ApiError::EmailTaken => f.write_str("email already registered"),
            ApiError::BadJson(e) => f.write_str(e),
            ApiError::Invalid(e) => write!(f, "{e}"),
        }
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::EmailTaken => StatusCode::CONFLICT,
            ApiError::BadJson(_) => StatusCode::BAD_REQUEST,
            ApiError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }

    fn error_response(&self) -> HttpResponse {
        let body = match self {
            ApiError::NotFound => json!({"code": "not_found", "message": "not found"}),
            ApiError::EmailTaken => {
                json!({"code": "email_taken", "message": "email already registered"})
            }
            ApiError::BadJson(e) => json!({"code": "bad_request", "message": e}),
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
                json!({"code": "validation_failed", "errors": fields})
            }
        };
        HttpResponse::build(self.status_code()).json(body)
    }
}

/// `web::Json<T>` that also runs `T`'s validation rules.
pub struct ValidJson<T>(pub T);

impl<T: DeserializeOwned + Validate + 'static> FromRequest for ValidJson<T> {
    type Error = ApiError;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self, ApiError>>>>;

    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let json = web::Json::<T>::from_request(req, payload);
        Box::pin(async move {
            let web::Json(value) = json.await.map_err(|e| ApiError::BadJson(e.to_string()))?;
            value.validate().map_err(ApiError::Invalid)?;
            Ok(ValidJson(value))
        })
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
#[utoipa::path(params(Pagination), responses((status = 200, body = Vec<User>)))]
#[get("/users")]
pub async fn list_users(
    store: web::Data<UserStore>,
    page: web::Query<Pagination>,
) -> web::Json<Vec<User>> {
    let limit = page.limit.unwrap_or(50).min(MAX_PAGE);
    web::Json(store.list(page.offset.unwrap_or(0), limit))
}

/// Fetch one user by id.
#[utoipa::path(params(("id" = u64, Path)), responses((status = 200, body = User), (status = 404)))]
#[get("/users/{id}")]
pub async fn get_user(
    store: web::Data<UserStore>,
    id: web::Path<u64>,
) -> Result<web::Json<User>, ApiError> {
    Ok(web::Json(store.get(*id)?))
}

/// Create a user. Responds 409 if the email is already registered.
#[utoipa::path(request_body = NewUser, responses((status = 201, body = User), (status = 409), (status = 422)))]
#[post("/users")]
pub async fn create_user(
    store: web::Data<UserStore>,
    ValidJson(new): ValidJson<NewUser>,
) -> Result<HttpResponse, ApiError> {
    Ok(HttpResponse::Created().json(store.create(new)?))
}

/// Replace a user.
#[utoipa::path(params(("id" = u64, Path)), request_body = NewUser, responses((status = 200, body = User), (status = 404), (status = 409), (status = 422)))]
#[put("/users/{id}")]
pub async fn replace_user(
    store: web::Data<UserStore>,
    id: web::Path<u64>,
    ValidJson(new): ValidJson<NewUser>,
) -> Result<web::Json<User>, ApiError> {
    Ok(web::Json(store.replace(*id, new)?))
}

/// Delete a user.
#[utoipa::path(params(("id" = u64, Path)), responses((status = 204), (status = 404)))]
#[delete("/users/{id}")]
pub async fn delete_user(
    store: web::Data<UserStore>,
    id: web::Path<u64>,
) -> Result<HttpResponse, ApiError> {
    store.delete(*id)?;
    Ok(HttpResponse::NoContent().finish())
}

/// Adds the `/users` routes. The store is shared app data (see `main.rs`),
/// so every worker sees the same users.
pub fn mount(cfg: &mut web::ServiceConfig) {
    cfg.service(list_users)
        .service(get_user)
        .service(create_user)
        .service(replace_user)
        .service(delete_user);
}
