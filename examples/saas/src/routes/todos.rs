//! Each user's todos: an htmx page at `/` and a JSON API under `/api/todos`.
//!
//! Every query is scoped to the logged-in user (`CurrentUser`), so one user
//! can never see or change another's todos: a foreign id is simply "not
//! found". Each htmx action answers with the re-rendered `list` block of
//! `templates/todos/index.html`, which htmx swaps into the page.

use zapline::db::{sqlx, Db};
use zapline::prelude::*;
use zapline::view::{context, HxRequest, Views};

use crate::routes::auth::CurrentUser;

/// A todo item.
#[model]
#[derive(sqlx::FromRow)]
pub struct Todo {
    pub id: i64,
    pub title: String,
    pub done: bool,
}

/// Body of a create request (form or JSON).
#[model]
pub struct NewTodo {
    #[validate(length(min = 1, max = 200))]
    pub title: String,
}

/// Adds the todo routes.
pub fn mount(app: App) -> App {
    app.mount((page, create_form, toggle, remove, list_json, create_json))
}

// ---- queries -----------------------------------------------------------------

async fn all(db: &Db, user_id: i64) -> Result<Vec<Todo>> {
    let todos = sqlx::query_as::<_, Todo>(
        "SELECT id, title, done FROM todos WHERE user_id = $1 ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(&**db)
    .await?;
    Ok(todos)
}

async fn insert(db: &Db, user_id: i64, title: &str) -> Result<Todo> {
    let todo = sqlx::query_as::<_, Todo>(
        "INSERT INTO todos (user_id, title, done) VALUES ($1, $2, FALSE) RETURNING id, title, done",
    )
    .bind(user_id)
    .bind(title)
    .fetch_one(&**db)
    .await?;
    Ok(todo)
}

/// Runs an UPDATE or DELETE on one of the user's todos; 404 if it isn't theirs.
async fn change(db: &Db, sql: &str, id: i64, user_id: i64) -> Result<()> {
    let done = sqlx::query(sql)
        .bind(id)
        .bind(user_id)
        .execute(&**db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(Error::not_found());
    }
    Ok(())
}

// ---- HTML (htmx) -------------------------------------------------------------

async fn render_list(
    views: &Views,
    db: &Db,
    hx: &HxRequest,
    user: &CurrentUser,
) -> Result<Html<String>> {
    let todos = all(db, user.0.id).await?;
    let email = &user.0.email;
    views.page_or_block(hx, "todos/index.html", "list", context! { todos, email })
}

/// The todo page (or just its list, for htmx). Visitors go to the log-in page.
#[get("/", tag = "pages")]
async fn page(views: Views, db: Db, hx: HxRequest, user: Option<CurrentUser>) -> Result<Response> {
    match user {
        Some(user) => Ok(render_list(&views, &db, &hx, &user).await?.into_response()),
        None => Ok(Redirect::to("/login").into_response()),
    }
}

/// Add a todo from the page's form.
#[post("/todos", tag = "pages")]
async fn create_form(
    views: Views,
    db: Db,
    hx: HxRequest,
    user: CurrentUser,
    Valid(Form(new)): Valid<Form<NewTodo>>,
) -> Result<Html<String>> {
    insert(&db, user.0.id, new.title.trim()).await?;
    render_list(&views, &db, &hx, &user).await
}

/// Flip a todo between done and not done.
#[post("/todos/{id}/toggle", tag = "pages")]
async fn toggle(
    views: Views,
    db: Db,
    hx: HxRequest,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Html<String>> {
    let sql = "UPDATE todos SET done = NOT done WHERE id = $1 AND user_id = $2";
    change(&db, sql, id, user.0.id).await?;
    render_list(&views, &db, &hx, &user).await
}

/// Delete a todo.
#[delete("/todos/{id}", tag = "pages")]
async fn remove(
    views: Views,
    db: Db,
    hx: HxRequest,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Html<String>> {
    change(
        &db,
        "DELETE FROM todos WHERE id = $1 AND user_id = $2",
        id,
        user.0.id,
    )
    .await?;
    render_list(&views, &db, &hx, &user).await
}

// ---- JSON API ------------------------------------------------------------------

/// List your todos.
#[get("/api/todos", tag = "todos")]
async fn list_json(db: Db, CurrentUser(user): CurrentUser) -> Result<Json<Vec<Todo>>> {
    Ok(Json(all(&db, user.id).await?))
}

/// Create a todo.
#[post("/api/todos", tag = "todos", status = 201)]
async fn create_json(
    db: Db,
    CurrentUser(user): CurrentUser,
    Valid(Json(new)): Valid<Json<NewTodo>>,
) -> Result<(StatusCode, Json<Todo>)> {
    let todo = insert(&db, user.id, new.title.trim()).await?;
    Ok((StatusCode::CREATED, Json(todo)))
}
