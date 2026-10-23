// The representative app the generated-client tests snapshot: every kind of
// input and output the TypeScript and JavaScript generators handle.
// Included by `tests/client.rs`.

use std::collections::HashMap;

use futures_util::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use zapline::multipart::Multipart;
use zapline::prelude::*;
use zapline::sse::Event;

/// Where a task is in its life.
#[derive(Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Not started.
    Todo,
    /// Someone is on it.
    Doing,
    /// Finished.
    Done,
}

/// A user.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct User {
    pub id: u64,
    pub name: String,
}

/// A task.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    pub id: u64,
    /// Short title shown in lists.
    pub title: String,
    pub status: Status,
    /// 1 (highest) to 5.
    pub priority: Option<u8>,
    pub tags: Vec<String>,
    pub assignee: Option<User>,
    /// Free-form labels.
    pub meta: HashMap<String, String>,
}

/// A new task.
#[derive(Deserialize, JsonSchema, Validate)]
pub struct NewTask {
    #[validate(length(min = 1, max = 80))]
    pub title: String,
    #[validate(range(min = 1, max = 5))]
    pub priority: Option<u8>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Fields to change; absent ones stay as they are.
#[derive(Deserialize, JsonSchema)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub status: Option<Status>,
    /// `null` clears the assignee.
    #[serde(default, deserialize_with = "serde_with_null::deserialize")]
    #[schemars(with = "Option<u64>")]
    pub assignee: Option<Option<u64>>,
}

/// Generic page of results.
#[derive(Serialize, JsonSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u64,
    /// Cursor of the next page, if any.
    pub next: Option<String>,
}

/// List filters.
#[derive(Deserialize, JsonSchema)]
pub struct TaskFilter {
    /// Only tasks in this state.
    pub status: Option<Status>,
    /// Search in titles.
    pub q: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    20
}

/// A shape, tagged by `kind`.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Shape {
    Circle { radius: f64 },
    Rect { width: f64, height: f64 },
}

/// Credentials posted by the login form.
#[derive(Deserialize, JsonSchema)]
pub struct Login {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub remember: bool,
}

/// Uploaded files.
#[derive(Serialize, JsonSchema)]
pub struct Upload {
    pub files: Vec<String>,
    pub bytes: usize,
}

fn sample(id: u64) -> Task {
    Task {
        id,
        title: format!("task {id}"),
        status: Status::Todo,
        priority: Some(3),
        tags: vec!["a".into()],
        assignee: None,
        meta: HashMap::new(),
    }
}

/// List tasks.
#[get("/tasks", tag = "tasks")]
pub async fn list_tasks(Query(f): Query<TaskFilter>) -> Json<Page<Task>> {
    let items: Vec<Task> = (1..=3)
        .map(sample)
        .filter(|t| f.status.is_none_or(|s| s == t.status))
        .filter(|t| f.q.as_ref().is_none_or(|q| t.title.contains(q.as_str())))
        .take(f.limit as usize)
        .collect();
    Json(Page {
        total: items.len() as u64,
        items,
        next: None,
    })
}

/// Create a task.
#[post("/tasks", tag = "tasks", status = 201)]
pub async fn create_task(Valid(Json(new)): Valid<Json<NewTask>>) -> (StatusCode, Json<Task>) {
    let mut task = sample(42);
    task.title = new.title;
    task.priority = new.priority;
    task.tags = new.tags;
    (StatusCode::CREATED, Json(task))
}

/// One task.
///
/// Returns 404 when there is no such task.
#[get("/tasks/{id}", tag = "tasks")]
pub async fn get_task(Path(id): Path<u64>) -> Option<Json<Task>> {
    (id < 100).then(|| Json(sample(id)))
}

/// Change a task.
#[patch("/tasks/{id}", tag = "tasks")]
pub async fn update_task(Path(id): Path<u64>, Json(patch): Json<TaskPatch>) -> Json<Task> {
    let mut task = sample(id);
    if let Some(t) = patch.title {
        task.title = t;
    }
    if let Some(s) = patch.status {
        task.status = s;
    }
    if let Some(assignee) = patch.assignee {
        task.assignee = assignee.map(|id| User {
            id,
            name: format!("user {id}"),
        });
    }
    Json(task)
}

/// Delete a task.
#[delete("/tasks/{id}", tag = "tasks", status = 204)]
pub async fn delete_task(Path(_id): Path<u64>) -> StatusCode {
    StatusCode::NO_CONTENT
}

/// A task by project and number.
#[get("/projects/{project}/tasks/{number}", tag = "tasks")]
pub async fn project_task(Path((project, number)): Path<(String, u32)>) -> Json<Task> {
    let mut task = sample(number.into());
    task.title = format!("{project}#{number}");
    Json(task)
}

/// Attach files to a task.
#[post("/tasks/{id}/files", tag = "tasks")]
pub async fn upload(Path(_id): Path<u64>, mut form: Multipart) -> Result<Json<Upload>> {
    let mut files = Vec::new();
    let mut bytes = 0;
    while let Some(field) = form.next_field().await? {
        files.push(field.file_name().unwrap_or("-").to_owned());
        bytes += field.bytes().await?.len();
    }
    Ok(Json(Upload { files, bytes }))
}

/// Sign in.
#[post("/login")]
pub async fn login(Form(login): Form<Login>) -> Result<Json<User>> {
    if login.password != "secret" {
        return Err(Error::unauthorized().with_code("bad_credentials"));
    }
    Ok(Json(User {
        id: if login.remember { 2 } else { 1 },
        name: login.email,
    }))
}

/// Area of a shape.
#[post("/shapes/area")]
pub async fn area(Json(shape): Json<Shape>) -> Json<f64> {
    Json(match shape {
        Shape::Circle { radius } => std::f64::consts::PI * radius * radius,
        Shape::Rect { width, height } => width * height,
    })
}

/// A file from the docs tree.
#[get("/docs-files/{*path}")]
pub async fn doc_file(Path(path): Path<String>) -> String {
    format!("file {path}")
}

/// Import tasks, one title per line.
#[post("/import", tag = "tasks")]
pub async fn import(body: String) -> Json<u64> {
    Json(body.lines().filter(|l| !l.is_empty()).count() as u64)
}

/// Raw export.
#[get("/export", tag = "tasks")]
pub async fn export() -> Bytes {
    Bytes::from_static(b"\x00\x01")
}

/// Task updates as server-sent events.
#[get("/events", tag = "tasks")]
pub async fn events() -> Sse<impl Stream<Item = Event>> {
    Sse::new(stream::iter(vec![Event::default().data("hello")]))
}

/// The HTML home page (not part of the JSON client).
#[get("/")]
pub async fn home() -> Html<&'static str> {
    Html("<h1>tasks</h1>")
}

/// The representative app.
pub fn app() -> App {
    App::new().mount((
        list_tasks,
        create_task,
        get_task,
        update_task,
        delete_task,
        project_task,
        upload,
        login,
        area,
        doc_file,
        import,
        export,
        events,
        home,
    ))
}

/// The spec settings the tests use.
pub fn api() -> OpenApi {
    OpenApi::new("Tasks API", "1.0.0").tag("tasks", "Manage tasks")
}

/// `Option<Option<T>>` with `null` kept as `Some(None)`.
mod serde_with_null {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }
}
