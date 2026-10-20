//! A small CRUD API showing state, extractors, errors, route groups and
//! middleware.
//!
//!     cargo run --release --example rest_api
//!     curl -X POST localhost:8080/api/todos -H 'content-type: application/json' -d '{"title":"ship it"}'
//!     curl localhost:8080/api/todos?done=false

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use zapline::middleware::{Cors, Logger};
use zapline::prelude::*;

#[derive(Clone, Serialize)]
struct Todo {
    id: u64,
    title: String,
    done: bool,
}

#[derive(Deserialize)]
struct NewTodo {
    title: String,
}

#[derive(Deserialize)]
struct UpdateTodo {
    title: Option<String>,
    done: Option<bool>,
}

#[derive(Deserialize)]
struct Filter {
    done: Option<bool>,
}

#[derive(Clone, Default)]
struct Db(Arc<RwLock<(u64, BTreeMap<u64, Todo>)>>);

#[get("/todos")]
async fn list(State(db): State<Db>, Query(f): Query<Filter>) -> Json<Vec<Todo>> {
    let db = db.0.read().unwrap();
    let todos =
        db.1.values()
            .filter(|t| f.done.is_none_or(|d| t.done == d))
            .cloned();
    Json(todos.collect())
}

#[get("/todos/{id}")]
async fn get_one(State(db): State<Db>, Path(id): Path<u64>) -> Option<Json<Todo>> {
    db.0.read().unwrap().1.get(&id).cloned().map(Json)
}

#[post("/todos")]
async fn create(
    State(db): State<Db>,
    Json(new): Json<NewTodo>,
) -> Result<(StatusCode, Json<Todo>)> {
    if new.title.trim().is_empty() {
        return Err(Error::unprocessable("title must not be empty"));
    }
    let mut db = db.0.write().unwrap();
    db.0 += 1;
    let todo = Todo {
        id: db.0,
        title: new.title,
        done: false,
    };
    db.1.insert(todo.id, todo.clone());
    Ok((StatusCode::CREATED, Json(todo)))
}

#[patch("/todos/{id}")]
async fn update(
    State(db): State<Db>,
    Path(id): Path<u64>,
    Json(patch): Json<UpdateTodo>,
) -> Result<Json<Todo>> {
    let mut db = db.0.write().unwrap();
    let todo = db.1.get_mut(&id).ok_or_else(Error::not_found)?;
    if let Some(t) = patch.title {
        todo.title = t;
    }
    if let Some(d) = patch.done {
        todo.done = d;
    }
    Ok(Json(todo.clone()))
}

#[delete("/todos/{id}")]
async fn remove(State(db): State<Db>, Path(id): Path<u64>) -> Result<StatusCode> {
    match db.0.write().unwrap().1.remove(&id) {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(Error::not_found()),
    }
}

/// Group middleware: only applies to `/api/*`.
async fn require_json_accept(req: Request, next: Next) -> Response {
    // Real apps would check auth here and insert the user via
    // `req.extensions_mut().insert(user)`, read back with `Extension<User>`.
    next.run(req).await
}

pub fn app() -> App {
    let api = Routes::new()
        .mount((list, get_one, create, update, remove))
        .wrap(require_json_accept);

    App::new()
        .state(Db::default())
        .wrap(Logger)
        .wrap(Cors::permissive())
        .get("/health", || async { "ok" })
        .nest("/api", api)
}

fn main() -> std::io::Result<()> {
    app().run("0.0.0.0:8080")
}
