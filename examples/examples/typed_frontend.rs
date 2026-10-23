//! A page without a build step that talks to the API through the typed
//! client zapline serves at `/_zapline/api.js`, showing validation errors
//! next to the inputs.
//!
//!     cargo run -p zapline-examples --example typed_frontend
//!     open http://localhost:8080
//!     curl localhost:8080/_zapline/api.js      # the generated ES module
//!
//! For a bundled frontend, `zapline dev` writes the same client as
//! TypeScript to `frontend/src/api.ts` after every build.

use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use validator::Validate;
use zapline::prelude::*;

/// A todo item.
#[derive(Clone, Serialize, JsonSchema)]
struct Todo {
    id: u64,
    title: String,
    done: bool,
}

/// A new todo item.
#[derive(Deserialize, JsonSchema, Validate)]
struct NewTodo {
    /// What to do.
    #[validate(length(min = 3, max = 60, message = "use 3 to 60 characters"))]
    title: String,
}

#[derive(Clone, Default)]
struct Todos(Arc<Mutex<Vec<Todo>>>);

/// All todo items.
#[get("/todos")]
async fn list_todos(State(todos): State<Todos>) -> Json<Vec<Todo>> {
    Json(todos.0.lock().map(|t| t.clone()).unwrap_or_default())
}

/// Add a todo item.
#[post("/todos", status = 201)]
async fn add_todo(
    State(todos): State<Todos>,
    Valid(Json(new)): Valid<Json<NewTodo>>,
) -> Result<(StatusCode, Json<Todo>)> {
    let mut all = todos
        .0
        .lock()
        .map_err(|_| Error::internal("store poisoned"))?;
    let todo = Todo {
        id: all.len() as u64 + 1,
        title: new.title,
        done: false,
    };
    all.push(todo.clone());
    Ok((StatusCode::CREATED, Json(todo)))
}

const PAGE: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Todos</title>
<form id="new">
  <input name="title" placeholder="Something to do" autofocus>
  <button>Add</button>
  <small id="error" style="color: crimson"></small>
</form>
<ul id="list"></ul>
<script type="module">
  // @ts-check
  import { api, isValidationError } from "/_zapline/api.js";

  const form = /** @type {HTMLFormElement} */ (document.querySelector("#new"));
  const input = /** @type {HTMLInputElement} */ (form.querySelector("[name=title]"));
  const error = /** @type {HTMLElement} */ (document.querySelector("#error"));
  const list = /** @type {HTMLElement} */ (document.querySelector("#list"));

  async function render() {
    list.replaceChildren(...(await api.listTodos()).map((t) => {
      const li = document.createElement("li");
      li.textContent = t.title;
      return li;
    }));
  }

  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    error.textContent = "";
    try {
      await api.addTodo({ title: input.value });
      input.value = "";
      await render();
    } catch (e) {
      // Typed: `fields` knows the body's field names.
      if (isValidationError(e, "addTodo")) error.textContent = e.messageFor("title") ?? e.message;
      else throw e;
    }
  });
  await render();
</script>
"##;

fn main() -> std::io::Result<()> {
    App::new()
        .state(Todos::default())
        .mount((list_todos, add_todo))
        .get("/", || async { Html(PAGE) })
        .hidden()
        .openapi(OpenApi::new("Todos", "1.0.0").client_js())
        .run("0.0.0.0:8080")
}
