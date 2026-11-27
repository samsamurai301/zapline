use std::collections::HashMap;
use std::sync::Mutex;

use actix_web::http::StatusCode;
use actix_web::{delete, get, post, web, App, HttpResponse, HttpServer, ResponseError};
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

#[derive(Default)]
struct Store(Mutex<HashMap<u64, Note>>);

#[derive(Debug)]
enum ApiError {
    NotFound,
    Invalid(validator::ValidationErrors),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NotFound => write!(f, "not found"),
            ApiError::Invalid(_) => write!(f, "validation failed"),
        }
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }
    fn error_response(&self) -> HttpResponse {
        let details = match self {
            ApiError::Invalid(e) => json!(e),
            ApiError::NotFound => json!(null),
        };
        HttpResponse::build(self.status_code())
            .json(json!({"error": self.to_string(), "details": details}))
    }
}

/// List notes.
#[utoipa::path(responses((status = 200, body = [Note])))]
#[get("/notes")]
async fn list(store: web::Data<Store>) -> web::Json<Vec<Note>> {
    web::Json(store.0.lock().unwrap().values().cloned().collect())
}

/// Create a note.
#[utoipa::path(request_body = NewNote, responses((status = 201, body = Note), (status = 422, description = "Invalid input")))]
#[post("/notes")]
async fn create(
    store: web::Data<Store>,
    new: web::Json<NewNote>,
) -> Result<HttpResponse, ApiError> {
    new.validate().map_err(ApiError::Invalid)?;
    let new = new.into_inner();
    let mut notes = store.0.lock().unwrap();
    let note = Note {
        id: notes.len() as u64 + 1,
        title: new.title,
        body: new.body,
    };
    notes.insert(note.id, note.clone());
    Ok(HttpResponse::Created().json(note))
}

/// Fetch a note.
#[utoipa::path(params(("id" = u64, Path)), responses((status = 200, body = Note), (status = 404, description = "Not found")))]
#[get("/notes/{id}")]
async fn get_note(
    store: web::Data<Store>,
    id: web::Path<u64>,
) -> Result<web::Json<Note>, ApiError> {
    let note = store.0.lock().unwrap().get(&id).cloned();
    note.map(web::Json).ok_or(ApiError::NotFound)
}

/// Delete a note.
#[utoipa::path(params(("id" = u64, Path)), responses((status = 204), (status = 404, description = "Not found")))]
#[delete("/notes/{id}")]
async fn remove(store: web::Data<Store>, id: web::Path<u64>) -> Result<HttpResponse, ApiError> {
    match store.0.lock().unwrap().remove(&id) {
        Some(_) => Ok(HttpResponse::NoContent().finish()),
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

fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(list)
        .service(create)
        .service(get_note)
        .service(remove)
        .route(
            "/openapi.json",
            web::get().to(|| async { HttpResponse::Ok().json(ApiDoc::openapi()) }),
        )
        .service(Scalar::with_url("/docs", ApiDoc::openapi()));
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let store = web::Data::new(Store::default());
    HttpServer::new(move || App::new().app_data(store.clone()).configure(configure))
        .bind("0.0.0.0:8080")?
        .run()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test;

    #[actix_web::test]
    async fn create_fetch_and_validate() {
        let store = web::Data::new(Store::default());
        let app = test::init_service(App::new().app_data(store).configure(configure)).await;
        let req = test::TestRequest::post()
            .uri("/notes")
            .set_json(json!({"title": "hi", "body": "there"}))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 201);
        let req = test::TestRequest::get().uri("/notes/1").to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 200);
        let req = test::TestRequest::get().uri("/notes/9").to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 404);
        let req = test::TestRequest::post()
            .uri("/notes")
            .set_json(json!({"title": "", "body": ""}))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 422);
    }
}
