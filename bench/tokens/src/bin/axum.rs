use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{OpenApi, ToSchema};
use utoipa_scalar::{Scalar, Servable};
use validator::Validate;

#[derive(Clone, Serialize, Deserialize, ToSchema)]
struct Note {
    id: u64,
    title: String,
    body: String,
}

#[derive(Deserialize, ToSchema, Validate)]
struct NewNote {
    #[validate(length(min = 1, max = 200))]
    title: String,
    body: String,
}

#[derive(Clone, Default)]
struct Store(Arc<Mutex<HashMap<u64, Note>>>);

enum ApiError {
    NotFound,
    Invalid(validator::ValidationErrors),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => {
                (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
            }
            ApiError::Invalid(e) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": "validation failed", "details": e})),
            )
                .into_response(),
        }
    }
}

/// List notes.
#[utoipa::path(get, path = "/notes", responses((status = 200, body = [Note])))]
async fn list(State(store): State<Store>) -> Json<Vec<Note>> {
    Json(store.0.lock().unwrap().values().cloned().collect())
}

/// Create a note.
#[utoipa::path(post, path = "/notes", request_body = NewNote,
    responses((status = 201, body = Note), (status = 422, description = "Invalid input")))]
async fn create(
    State(store): State<Store>,
    Json(new): Json<NewNote>,
) -> Result<(StatusCode, Json<Note>), ApiError> {
    new.validate().map_err(ApiError::Invalid)?;
    let mut notes = store.0.lock().unwrap();
    let note = Note {
        id: notes.len() as u64 + 1,
        title: new.title,
        body: new.body,
    };
    notes.insert(note.id, note.clone());
    Ok((StatusCode::CREATED, Json(note)))
}

/// Fetch a note.
#[utoipa::path(get, path = "/notes/{id}", params(("id" = u64, Path)),
    responses((status = 200, body = Note), (status = 404, description = "Not found")))]
async fn get_note(State(store): State<Store>, Path(id): Path<u64>) -> Result<Json<Note>, ApiError> {
    let note = store.0.lock().unwrap().get(&id).cloned();
    note.map(Json).ok_or(ApiError::NotFound)
}

/// Delete a note.
#[utoipa::path(delete, path = "/notes/{id}", params(("id" = u64, Path)),
    responses((status = 204), (status = 404, description = "Not found")))]
async fn remove(State(store): State<Store>, Path(id): Path<u64>) -> Result<StatusCode, ApiError> {
    match store.0.lock().unwrap().remove(&id) {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(ApiError::NotFound),
    }
}

#[derive(OpenApi)]
#[openapi(
    info(title = "notes", version = "1.0.0"),
    paths(list, create, get_note, remove),
    components(schemas(Note, NewNote))
)]
struct ApiDoc;

fn app() -> Router {
    Router::new()
        .route("/notes", get(list).post(create))
        .route("/notes/{id}", get(get_note).delete(remove))
        .route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
        .merge(Scalar::with_url("/docs", ApiDoc::openapi()))
        .with_state(Store::default())
}

#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app()).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> StatusCode {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        let req = req
            .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn create_fetch_and_validate() {
        let app = app();
        assert_eq!(
            send(
                &app,
                "POST",
                "/notes",
                Some(json!({"title": "hi", "body": "there"}))
            )
            .await,
            201
        );
        assert_eq!(send(&app, "GET", "/notes/1", None).await, 200);
        assert_eq!(send(&app, "GET", "/notes/9", None).await, 404);
        assert_eq!(
            send(
                &app,
                "POST",
                "/notes",
                Some(json!({"title": "", "body": ""}))
            )
            .await,
            422
        );
    }
}
