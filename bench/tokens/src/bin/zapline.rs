use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use zapline::openapi::OpenApi;
use zapline::prelude::*;

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct Note {
    id: u64,
    title: String,
    body: String,
}

#[derive(Deserialize, JsonSchema, Validate)]
struct NewNote {
    #[validate(length(min = 1, max = 200))]
    title: String,
    body: String,
}

#[derive(Clone, Default)]
struct Store(Arc<Mutex<HashMap<u64, Note>>>);

/// List notes.
#[get("/notes")]
async fn list(State(store): State<Store>) -> Json<Vec<Note>> {
    Json(store.0.lock().unwrap().values().cloned().collect())
}

/// Create a note.
#[post("/notes", status = 201)]
async fn create(
    State(store): State<Store>,
    Valid(Json(new)): Valid<Json<NewNote>>,
) -> (StatusCode, Json<Note>) {
    let mut notes = store.0.lock().unwrap();
    let note = Note {
        id: notes.len() as u64 + 1,
        title: new.title,
        body: new.body,
    };
    notes.insert(note.id, note.clone());
    (StatusCode::CREATED, Json(note))
}

/// Fetch a note.
#[get("/notes/{id}")]
async fn get_note(State(store): State<Store>, Path(id): Path<u64>) -> Result<Json<Note>> {
    let note = store.0.lock().unwrap().get(&id).cloned();
    note.map(Json).ok_or_else(Error::not_found)
}

/// Delete a note.
#[delete("/notes/{id}", status = 204)]
async fn remove(State(store): State<Store>, Path(id): Path<u64>) -> Result<StatusCode> {
    match store.0.lock().unwrap().remove(&id) {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(Error::not_found()),
    }
}

fn app() -> App {
    App::new()
        .state(Store::default())
        .openapi(OpenApi::new("notes", "1.0.0"))
        .mount((list, create, get_note, remove))
}

fn main() -> std::io::Result<()> {
    app().run("0.0.0.0:8080")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn create_fetch_and_validate() {
        let client = app().test();
        let res = client
            .post("/notes")
            .json(&json!({"title": "hi", "body": "there"}))
            .send()
            .await;
        res.assert_status(201);
        client.get("/notes/1").send().await.assert_status(200);
        client.get("/notes/9").send().await.assert_status(404);
        client
            .post("/notes")
            .json(&json!({"title": "", "body": ""}))
            .send()
            .await
            .assert_status(422);
    }
}
