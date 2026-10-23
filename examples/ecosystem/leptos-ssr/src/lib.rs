//! leptos-ssr: Leptos components rendered on the server by zapline.
//!
//! Handlers load data with zapline's database integration, then render a
//! Leptos view to HTML with [`render`]. Forms post to zapline routes, which
//! answer with a redirect (Post/Redirect/Get) or, for invalid input, the
//! page again with the error and the user's text. No JavaScript, no build
//! step, no WASM toolchain.

pub mod components;
pub mod render;

use leptos::prelude::view;
use zapline::db::{sqlx, Database, Db, DbCheck};
use zapline::prelude::*;

use components::{Note, NotesBoard, Page};
pub use render::render;

/// Builds the app on `db`.
pub fn app(db: Database) -> App {
    App::new()
        .database(db)
        .plugin(Health::new().check("database", DbCheck::new()))
        .mount((home, add_note, toggle_note, delete_note))
}

/// Body of the "add" form.
#[model]
pub struct NewNote {
    #[validate(length(min = 1, max = 200, message = "A note has 1 to 200 characters."))]
    pub text: String,
}

#[get("/")]
async fn home(db: Db) -> Result<Html<String>> {
    let notes = all_notes(&db).await?;
    Ok(board(notes, None, String::new()))
}

#[post("/notes")]
async fn add_note(db: Db, Form(form): Form<NewNote>) -> Result<Response> {
    let new = NewNote {
        text: form.text.trim().to_owned(),
    };
    if let Err(errors) = new.validate() {
        // Show the page again with the message and the user's text, so
        // nothing they typed is lost.
        let message = errors
            .field_errors()
            .get("text")
            .and_then(|e| e.first())
            .and_then(|e| e.message.as_ref())
            .map_or_else(|| "Invalid note.".to_owned(), |m| m.to_string());
        let notes = all_notes(&db).await?;
        let page = board(notes, Some(message), form.text);
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    sqlx::query("INSERT INTO notes (text) VALUES ($1)")
        .bind(&new.text)
        .execute(&*db)
        .await?;
    Ok(Redirect::to("/").into_response())
}

#[post("/notes/{id}/toggle")]
async fn toggle_note(db: Db, Path(id): Path<i64>) -> Result<Redirect> {
    let updated = sqlx::query("UPDATE notes SET done = NOT done WHERE id = $1")
        .bind(id)
        .execute(&*db)
        .await?;
    if updated.rows_affected() == 0 {
        return Err(Error::not_found());
    }
    Ok(Redirect::to("/"))
}

#[post("/notes/{id}/delete")]
async fn delete_note(db: Db, Path(id): Path<i64>) -> Result<Redirect> {
    let deleted = sqlx::query("DELETE FROM notes WHERE id = $1")
        .bind(id)
        .execute(&*db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(Error::not_found());
    }
    Ok(Redirect::to("/"))
}

async fn all_notes(db: &Db) -> Result<Vec<Note>> {
    let notes = sqlx::query_as::<_, Note>("SELECT id, text, done FROM notes ORDER BY id")
        .fetch_all(&**db)
        .await?;
    Ok(notes)
}

fn board(notes: Vec<Note>, error: Option<String>, draft: String) -> Html<String> {
    render(move || {
        view! {
            <Page title="Notes">
                <NotesBoard notes=notes error=error draft=draft/>
            </Page>
        }
    })
}
