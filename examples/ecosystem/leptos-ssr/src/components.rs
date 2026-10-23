//! Leptos components. They take plain data as props and render to HTML on
//! the server; every interaction is a regular `<form>` post, so the pages
//! work in any browser, with or without JavaScript.

use leptos::prelude::*;

/// A note on the board.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct Note {
    pub id: i64,
    pub text: String,
    pub done: bool,
}

const STYLE: &str = "
body { font: 16px/1.5 system-ui, sans-serif; max-width: 40rem; margin: 2rem auto; padding: 0 1rem; }
.notes { list-style: none; padding: 0; }
.notes li { display: flex; gap: .5rem; align-items: center; padding: .25rem 0; }
.notes li span { flex: 1; }
.notes li.done span { text-decoration: line-through; color: #888; }
.new { display: flex; gap: .5rem; flex-wrap: wrap; }
.new input { flex: 1; }
.error { color: #b00020; width: 100%; margin: 0; }
";

/// The HTML document around every page.
#[component]
pub fn Page(title: &'static str, children: Children) -> impl IntoView {
    view! {
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <title>{title}</title>
                <style inner_html=STYLE></style>
            </head>
            <body>
                <main>{children()}</main>
            </body>
        </html>
    }
}

/// The whole board: a summary, the form, and the notes.
#[component]
pub fn NotesBoard(
    notes: Vec<Note>,
    /// A message to show under the form (for example a validation error).
    error: Option<String>,
    /// What to put back in the text box after an error.
    draft: String,
) -> impl IntoView {
    let open = notes.iter().filter(|n| !n.done).count();
    let done = notes.len() - open;
    let empty = notes.is_empty();
    view! {
        <h1>"Notes"</h1>
        <p class="summary">{format!("{open} open, {done} done")}</p>
        <NoteForm error=error draft=draft/>
        {empty.then(|| view! { <p class="empty">"Nothing yet."</p> })}
        <ul class="notes">
            {notes.into_iter().map(|note| view! { <NoteItem note=note/> }).collect_view()}
        </ul>
    }
}

/// The form that adds a note.
#[component]
fn NoteForm(error: Option<String>, draft: String) -> impl IntoView {
    view! {
        <form method="post" action="/notes" class="new">
            <input
                name="text"
                value=draft
                placeholder="What needs doing?"
                maxlength="200"
                required=true
                aria-label="New note"
            />
            <button type="submit">"Add"</button>
            {error.map(|e| view! { <p class="error" role="alert">{e}</p> })}
        </form>
    }
}

/// One note with its buttons.
#[component]
fn NoteItem(note: Note) -> impl IntoView {
    let toggle = format!("/notes/{}/toggle", note.id);
    let delete = format!("/notes/{}/delete", note.id);
    let label = if note.done { "Reopen" } else { "Done" };
    view! {
        <li class:done=note.done>
            <span>{note.text}</span>
            <form method="post" action=toggle>
                <button type="submit">{label}</button>
            </form>
            <form method="post" action=delete>
                <button type="submit">"Delete"</button>
            </form>
        </li>
    }
}
