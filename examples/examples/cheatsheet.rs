//! The zapline cheatsheet: one file, the canonical way to do each common
//! task. It is included verbatim in the guide (`docs/src/cheatsheet.md`) and
//! in `llms.txt`, so it doubles as compact context for AI assistants.
//!
//!     cargo run -p zapline-examples --example cheatsheet
//!     cargo test -p zapline-examples --example cheatsheet

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use zapline::middleware::{Cors, SetRequestId};
use zapline::prelude::*;

// ---- Models: `#[model]` derives serde, JsonSchema (OpenAPI) and, with `#[validate]` rules, Validate.
// Doc comments become OpenAPI descriptions.

/// A note.
#[model]
pub struct Note {
    pub id: u64,
    pub text: String,
}

/// Body of `POST /notes`.
#[model]
pub struct NewNote {
    #[validate(length(min = 1, max = 500))]
    pub text: String,
}

/// Query string of `GET /notes`.
#[model]
pub struct Page {
    pub limit: Option<usize>,
}

// ---- Services: a trait behind `Dep<dyn Trait>` so tests can swap it.

pub trait Notes: Send + Sync {
    fn list(&self, limit: usize) -> Vec<Note>;
    fn get(&self, id: u64) -> Option<Note>;
    fn add(&self, text: String) -> Note;
}

#[derive(Default)]
pub struct MemNotes(RwLock<BTreeMap<u64, Note>>);

impl Notes for MemNotes {
    fn list(&self, limit: usize) -> Vec<Note> {
        self.0
            .read()
            .unwrap()
            .values()
            .take(limit)
            .cloned()
            .collect()
    }
    fn get(&self, id: u64) -> Option<Note> {
        self.0.read().unwrap().get(&id).cloned()
    }
    fn add(&self, text: String) -> Note {
        let mut map = self.0.write().unwrap();
        let note = Note {
            id: map.len() as u64 + 1,
            text,
        };
        map.insert(note.id, note.clone());
        note
    }
}

// ---- Handlers: route macro + async fn; arguments are extractors.

/// List notes.
#[get("/notes", tag = "notes")]
async fn list_notes(notes: Dep<dyn Notes>, Query(page): Query<Page>) -> Json<Vec<Note>> {
    Json(notes.list(page.limit.unwrap_or(50).min(200)))
}

/// Fetch one note. Unknown ids are a 404 with a stable error code.
#[get("/notes/{id}", tag = "notes")]
async fn get_note(notes: Dep<dyn Notes>, Path(id): Path<u64>) -> Result<Json<Note>> {
    let note = notes
        .get(id)
        .ok_or_else(|| Error::not_found().with_code("note_not_found"))?;
    Ok(Json(note))
}

/// Create a note. Invalid bodies get a 422 with per-field details.
#[post("/notes", tag = "notes", status = 201)]
async fn create_note(
    notes: Dep<dyn Notes>,
    Valid(Json(new)): Valid<Json<NewNote>>,
) -> (StatusCode, Json<Note>) {
    (StatusCode::CREATED, Json(notes.add(new.text)))
}

// ---- Middleware: a plain async fn.

async fn powered_by(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert("x-powered-by", HeaderValue::from_static("zapline"));
    res
}

// ---- The app: one function that tests and `main` share.

pub fn app(notes: Arc<dyn Notes>) -> App {
    App::new()
        .provide_dyn::<dyn Notes>(notes)
        .wrap(SetRequestId::new())
        .wrap(Cors::permissive())
        .wrap(powered_by)
        .nest(
            "/api",
            Routes::new().mount((list_notes, get_note, create_note)),
        )
        .plugin(Health::new()) // /health, /health/live, /health/ready
        .openapi(OpenApi::new("Notes", "1.0.0")) // /openapi.json + /docs
}

fn main() -> std::io::Result<()> {
    app(Arc::new(MemNotes::default())).run("0.0.0.0:8080")
}

// ---- Tests: in-process, no sockets.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn create_then_fetch() {
        let client = app(Arc::new(MemNotes::default())).test();

        let res = client
            .post("/api/notes")
            .json(&json!({"text": "hi"}))
            .send()
            .await;
        assert_eq!(res.status(), 201);
        let note: Note = res.json();

        let res = client.get(&format!("/api/notes/{}", note.id)).send().await;
        assert_eq!(res.status(), 200);
        assert_eq!(res.header("x-powered-by"), Some("zapline"));
    }

    #[tokio::test]
    async fn validation_and_not_found() {
        let client = app(Arc::new(MemNotes::default())).test();
        let res = client
            .post("/api/notes")
            .json(&json!({"text": ""}))
            .send()
            .await;
        assert_eq!(res.status(), 422);
        let res = client.get("/api/notes/99").send().await;
        assert_eq!(res.status(), 404);
    }
}
