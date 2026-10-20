//! A Spring-style layered application: typed configuration -> repository
//! (a trait, swappable in tests) -> service -> thin controllers, plus a
//! scheduled job, a background worker and per-request background tasks.
//!
//!     cargo run -p zapline-examples --example enterprise_layered
//!     APP__SERVER__PORT=9000 APP__TODOS__MAX_TITLE=20 cargo run -p zapline-examples --example enterprise_layered
//!
//!     curl -X POST localhost:8080/todos -H 'x-user: ada' -H 'content-type: application/json' -d '{"title":"ship it"}'
//!     curl localhost:8080/todos -H 'x-user: ada'
//!     curl -X POST localhost:8080/todos/1/done -H 'x-user: ada'

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zapline::config::{Config, Secret};
use zapline::di::{Cached, Dep};
use zapline::extract::FromRequestParts;
use zapline::prelude::*;
use zapline::tasks::{BackgroundTasks, Schedule, Shutdown};

// ---------------------------------------------------------------------------
// Configuration

/// Built-in defaults; `config/*.toml`, `.env` and `APP__*` env vars override.
const DEFAULTS: &str = r#"
[server]
host = "0.0.0.0"
port = 8080

[todos]
max_title = 80
purge_every_secs = 30

[notifications]
webhook_url = "https://hooks.example.com/todos"
api_key = "dev-key"
"#;

#[derive(Clone, Deserialize)]
struct Settings {
    server: ServerSettings,
    todos: TodoSettings,
    notifications: NotificationSettings,
}

#[derive(Clone, Deserialize)]
struct ServerSettings {
    host: String,
    port: u16,
}

#[derive(Clone, Deserialize)]
struct TodoSettings {
    max_title: usize,
    purge_every_secs: u64,
}

#[derive(Clone, Deserialize)]
struct NotificationSettings {
    webhook_url: String,
    api_key: Secret<String>, // never printed by Debug/Display
}

// ---------------------------------------------------------------------------
// Repository layer

#[derive(Clone, Serialize)]
struct Todo {
    id: u64,
    owner: String,
    title: String,
    done: bool,
}

trait TodoRepository: Send + Sync {
    fn insert(&self, owner: &str, title: &str) -> Todo;
    fn list(&self, owner: &str) -> Vec<Todo>;
    fn mark_done(&self, owner: &str, id: u64) -> Option<Todo>;
    fn purge_done(&self) -> usize;
}

#[derive(Default)]
struct InMemoryTodos {
    next_id: AtomicU64,
    rows: RwLock<BTreeMap<u64, Todo>>,
}

impl TodoRepository for InMemoryTodos {
    fn insert(&self, owner: &str, title: &str) -> Todo {
        let todo = Todo {
            id: self.next_id.fetch_add(1, Ordering::Relaxed) + 1,
            owner: owner.to_owned(),
            title: title.to_owned(),
            done: false,
        };
        self.rows.write().unwrap().insert(todo.id, todo.clone());
        todo
    }

    fn list(&self, owner: &str) -> Vec<Todo> {
        let rows = self.rows.read().unwrap();
        rows.values()
            .filter(|t| t.owner == owner)
            .cloned()
            .collect()
    }

    fn mark_done(&self, owner: &str, id: u64) -> Option<Todo> {
        let mut rows = self.rows.write().unwrap();
        let todo = rows.get_mut(&id).filter(|t| t.owner == owner)?;
        todo.done = true;
        Some(todo.clone())
    }

    fn purge_done(&self) -> usize {
        let mut rows = self.rows.write().unwrap();
        let before = rows.len();
        rows.retain(|_, t| !t.done);
        before - rows.len()
    }
}

// ---------------------------------------------------------------------------
// Service layer: business rules, no HTTP types except `Error`.

struct TodoService {
    repo: Arc<dyn TodoRepository>,
    max_title: usize,
}

impl TodoService {
    fn create(&self, owner: &str, title: &str) -> Result<Todo> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > self.max_title {
            return Err(Error::unprocessable(format!(
                "title must be 1..={} characters",
                self.max_title
            ))
            .with_code("invalid_title"));
        }
        Ok(self.repo.insert(owner, title))
    }

    fn list(&self, owner: &str) -> Vec<Todo> {
        self.repo.list(owner)
    }

    fn complete(&self, owner: &str, id: u64) -> Result<Todo> {
        self.repo
            .mark_done(owner, id)
            .ok_or_else(|| Error::not_found().with_code("todo_not_found"))
    }
}

/// Sends notifications; a separate provider so tests can replace it.
struct Notifier {
    webhook_url: String,
    api_key: Secret<String>,
}

impl Notifier {
    async fn todo_created(&self, todo: &Todo) -> Result<()> {
        // A real app would POST to the webhook with the API key.
        let _key = self.api_key.expose();
        eprintln!(
            "notify {}: {} created #{} {:?}",
            self.webhook_url, todo.owner, todo.id, todo.title
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Request scope: the caller, resolved once per request.

#[derive(Clone)]
struct CurrentUser(String);

impl FromRequestParts for CurrentUser {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.headers()
            .get("x-user")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(|v| CurrentUser(v.to_owned()))
            .ok_or_else(|| Error::unauthorized().with_code("missing_user"))
    }
}

/// Route middleware: rejects anonymous requests. The handler's
/// `Cached<CurrentUser>` reuses this lookup instead of repeating it.
async fn require_user(mut req: Request, next: Next) -> Response {
    match Cached::<CurrentUser>::resolve(&mut req).await {
        Ok(_) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Controllers

#[derive(Deserialize)]
struct NewTodo {
    title: String,
}

#[post("/todos")]
async fn create_todo(
    todos: Dep<TodoService>,
    notifier: Dep<Notifier>,
    Cached(user): Cached<CurrentUser>,
    tasks: BackgroundTasks,
    Json(new): Json<NewTodo>,
) -> Result<(StatusCode, Json<Todo>)> {
    let todo = todos.create(&user.0, &new.title)?;
    let created = todo.clone();
    tasks.spawn(async move { notifier.todo_created(&created).await });
    Ok((StatusCode::CREATED, Json(todo)))
}

#[get("/todos")]
async fn list_todos(todos: Dep<TodoService>, Cached(user): Cached<CurrentUser>) -> Json<Vec<Todo>> {
    Json(todos.list(&user.0))
}

#[post("/todos/{id}/done")]
async fn complete_todo(
    todos: Dep<TodoService>,
    Cached(user): Cached<CurrentUser>,
    Path(id): Path<u64>,
) -> Result<Json<Todo>> {
    todos.complete(&user.0, id).map(Json)
}

// ---------------------------------------------------------------------------
// Composition root

fn app(settings: Settings) -> App {
    let purge_every = Duration::from_secs(settings.todos.purge_every_secs.max(1));
    let api = Routes::new()
        .mount((create_todo, list_todos, complete_todo))
        .wrap(require_user);

    App::new()
        .config(settings) // State<Settings> + Dep<Settings>
        .provide_dyn::<dyn TodoRepository>(Arc::new(InMemoryTodos::default()))
        .provide_with(|res: Resources| async move {
            let settings = res.dep::<Settings>()?;
            Ok(TodoService {
                repo: res.dep::<dyn TodoRepository>()?,
                max_title: settings.todos.max_title,
            })
        })
        .provide_with(|res: Resources| async move {
            let n = &res.dep::<Settings>()?.notifications;
            Ok(Notifier {
                webhook_url: n.webhook_url.clone(),
                api_key: n.api_key.clone(),
            })
        })
        .schedule(
            Schedule::every(purge_every)
                .delay_start(purge_every)
                .name("purge-done-todos"),
            |res: Resources| async move {
                let purged = res.dep::<dyn TodoRepository>()?.purge_done();
                if purged > 0 {
                    eprintln!("purged {purged} completed todo(s)");
                }
                Ok::<_, Error>(())
            },
        )
        .background(|_res: Resources, shutdown: Shutdown| async move {
            let mut beats = 0u64;
            loop {
                tokio::select! {
                    _ = shutdown.wait() => break,
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {
                        beats += 1;
                        eprintln!("heartbeat #{beats}");
                    }
                }
            }
        })
        .tasks_grace_period(Duration::from_secs(5))
        .get("/health", || async { "ok" })
        .nest("/", api)
}

fn main() -> std::io::Result<()> {
    let config = Config::builder().toml_str(DEFAULTS).standard().build()?;
    let settings: Settings = config.extract()?;
    eprintln!(
        "profile: {}, notifications api key: {}",
        config.profile(),
        settings.notifications.api_key // prints [REDACTED]
    );
    let addr = (settings.server.host.clone(), settings.server.port);
    app(settings).run(addr)
}
