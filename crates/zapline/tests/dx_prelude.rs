//! A typical app written against `use zapline::prelude::*` alone (plus
//! serde's derives and std): if the prelude loses a name, or gains one that
//! clashes with serde or std, this stops compiling.
#![cfg(all(
    feature = "macros",
    feature = "json",
    feature = "query",
    feature = "openapi",
    feature = "validate",
    feature = "tracing"
))]

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::stream;
use serde::{Deserialize, Serialize};
use zapline::prelude::*;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Validate)]
struct NewTodo {
    #[validate(length(min = 1, max = 100))]
    title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
struct Todo {
    id: u64,
    title: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListQuery {
    limit: Option<usize>,
}

/// A repository service, injected with `Dep`.
#[derive(Default)]
struct Todos {
    items: Mutex<Vec<Todo>>,
}

impl Todos {
    fn add(&self, title: String) -> Todo {
        let mut items = self.items.lock().unwrap();
        let todo = Todo {
            id: items.len() as u64 + 1,
            title,
        };
        items.push(todo.clone());
        todo
    }
}

/// An authenticated user with roles.
#[derive(Clone)]
struct Me {
    name: String,
    roles: Vec<String>,
}

impl HasRoles for Me {
    fn roles(&self) -> &[String] {
        &self.roles
    }
}

impl fmt::Display for Me {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?}", self.name, self.roles)
    }
}

/// List todos.
#[get("/todos")]
async fn list(todos: Dep<Todos>, Query(q): Query<ListQuery>) -> Json<Vec<Todo>> {
    let items = todos.items.lock().unwrap();
    Json(items.iter().take(q.limit.unwrap_or(10)).cloned().collect())
}

/// Create a todo.
#[post("/todos")]
async fn create(
    todos: Dep<Todos>,
    tasks: BackgroundTasks,
    _id: RequestId,
    Valid(Json(new)): Valid<Json<NewTodo>>,
) -> Result<(StatusCode, Json<Todo>)> {
    let todo = todos.add(new.title);
    tasks.spawn(async {});
    Ok((StatusCode::CREATED, Json(todo)))
}

/// Fetch one todo.
#[get("/todos/{id}")]
async fn get_one(todos: Dep<Todos>, Path(id): Path<u64>) -> Option<Json<Todo>> {
    let items = todos.items.lock().unwrap();
    items.iter().find(|t| t.id == id).cloned().map(Json)
}

async fn whoami(CurrentUser(me): CurrentUser<Me>) -> String {
    format!("{me} admin={}", me.has_role("admin"))
}

async fn token(token: BearerToken) -> String {
    token.token().to_owned()
}

async fn ticks() -> Sse<impl futures_util::Stream<Item = zapline::sse::Event>> {
    let events = (1..=2).map(|i| zapline::sse::Event::default().data(i.to_string()));
    Sse::new(stream::iter(events)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn upload(mut body: BodyStream) -> Result<String> {
    let mut n = 0;
    while let Some(chunk) = body.chunk().await? {
        n += chunk.len();
    }
    Ok(n.to_string())
}

async fn download() -> StreamBody<impl futures_util::Stream<Item = Result<&'static str, fmt::Error>>>
{
    StreamBody::new(stream::iter([Ok("a"), Ok("b")]))
}

/// std's two-parameter `Result` keeps working through the prelude's alias.
fn parse(s: &str) -> Result<u32, std::num::ParseIntError> {
    s.parse()
}

fn app() -> App {
    let auth = Authenticate::new(|req: &Request| {
        let user = req
            .headers()
            .get("x-user")
            .and_then(|v| v.to_str().ok())
            .map(|name| Me {
                name: name.to_owned(),
                roles: vec!["admin".into()],
            });
        async move { Ok(user) }
    })
    .required();

    App::new()
        .provide(Todos::default())
        .mount((list, create, get_one))
        .get("/token", token)
        .get("/ticks", ticks)
        .post("/upload", upload)
        .get("/download", download)
        .nest("/me", Routes::new().get("/", whoami).wrap(auth))
        .schedule(
            Schedule::every(Duration::from_secs(3600)),
            |_: Resources| async { Ok::<_, Error>(()) },
        )
        .openapi(OpenApi::new("Todos", "1.0.0").docs(DocsUi::Scalar, "/reference"))
        .plugin(Health::new())
        .plugin(Metrics::new())
        .plugin(Trace::new())
}

#[tokio::test]
async fn typical_app_runs() {
    let c = app().test();
    assert_eq!(parse("7"), Ok(7));

    let r = c
        .post("/todos")
        .json(&NewTodo {
            title: "write docs".into(),
        })
        .send()
        .await;
    r.assert_status(StatusCode::CREATED);
    assert_eq!(
        r.json::<Todo>(),
        Todo {
            id: 1,
            title: "write docs".into()
        }
    );
    let r = c
        .post("/todos")
        .json(&HashMap::from([("title", "")]))
        .send()
        .await;
    r.assert_status(422);
    assert!(r.text().contains("validation_error"), "{}", r.text());

    c.get("/todos/1").send().await.assert_status(200);
    c.get("/todos/9").send().await.assert_status(404);
    let all: Vec<Todo> = c.get("/todos?limit=5").send().await.json();
    assert_eq!(all.len(), 1);

    let r = c.get("/token").bearer("t0k").send().await;
    assert_eq!(r.text(), "t0k");
    let r = c.get("/me").header("x-user", "ada").send().await;
    assert_eq!(r.text(), "ada [\"admin\"] admin=true");
    c.get("/me").send().await.assert_status(401);

    let r = c.get("/ticks").send().await;
    assert_eq!(r.text(), "data: 1\n\ndata: 2\n\n");
    assert_eq!(c.post("/upload").body("12345").send().await.text(), "5");
    assert_eq!(c.get("/download").send().await.text(), "ab");

    let spec = c.get("/openapi.json").send().await;
    spec.assert_status(200);
    assert!(spec.text().contains("/todos/{id}"));
    c.get("/reference").send().await.assert_status(200);
    c.get("/health").send().await.assert_status(200);
    c.get("/metrics").send().await.assert_status(200);
}

/// Glob-importing serde next to the prelude is unambiguous too.
mod with_serde_glob {
    use serde::*;
    use zapline::prelude::*;

    #[derive(Serialize, Deserialize)]
    struct Point {
        x: i32,
    }

    async fn flip(Json(p): Json<Point>) -> Result<Json<Point>> {
        Ok(Json(Point { x: -p.x }))
    }

    fn _bounds<'de, D: Deserializer<'de>, S: Serializer>() {}

    pub fn app() -> App {
        App::new().post("/flip", flip)
    }
}

/// Explicit imports of the same names from their source crates still work.
mod with_explicit_imports {
    use schemars::JsonSchema;
    use validator::Validate;
    use zapline::prelude::*;
    use zapline::telemetry::{self, Trace};

    #[derive(serde::Deserialize, JsonSchema, Validate)]
    struct Signup {
        #[validate(email)]
        email: String,
    }

    pub fn app() -> App {
        let _ = telemetry::init;
        App::new()
            .post(
                "/signup",
                |Valid(Json(s)): Valid<Json<Signup>>| async move { s.email },
            )
            .plugin(Trace::new())
    }
}

#[tokio::test]
async fn prelude_coexists_with_other_imports() {
    let c = with_serde_glob::app().test();
    let r = c
        .post("/flip")
        .json(&serde_json::json!({ "x": 3 }))
        .send()
        .await;
    assert_eq!(r.text(), r#"{"x":-3}"#);

    let c = with_explicit_imports::app().test();
    let r = c
        .post("/signup")
        .json(&serde_json::json!({ "email": "ada@example.com" }))
        .send()
        .await;
    assert_eq!(r.text(), "ada@example.com");
    let r = c
        .post("/signup")
        .json(&serde_json::json!({ "email": "nope" }))
        .send()
        .await;
    r.assert_status(422);
}

#[cfg(all(
    feature = "jwt",
    feature = "cookies",
    feature = "multipart",
    feature = "ws",
    feature = "fs"
))]
mod full {
    use zapline::prelude::*;

    const SECRET: &str = "an-example-secret-of-at-least-32-bytes";

    async fn me(Jwt(claims): Jwt<Claims>) -> String {
        claims.sub
    }

    async fn admin(CurrentUser(claims): CurrentUser<Claims>) -> String {
        format!("{} {}", claims.sub, claims.has_role("admin"))
    }

    async fn visit(jar: CookieJar) -> (CookieJar, String) {
        let n: u32 = jar
            .get("visits")
            .and_then(|c| c.value().parse().ok())
            .unwrap_or(0);
        let jar = jar.add(zapline::cookies::Cookie::new("visits", (n + 1).to_string()));
        (jar, n.to_string())
    }

    async fn fields(mut form: Multipart) -> Result<String> {
        let mut names = Vec::new();
        while let Some(field) = form.next_field().await? {
            names.push(field.name().unwrap_or("").to_owned());
        }
        Ok(names.join(","))
    }

    async fn socket(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|mut socket: WebSocket| async move {
            while let Some(Ok(msg)) = socket.recv().await {
                if socket.send(msg).await.is_err() {
                    break;
                }
            }
        })
    }

    #[tokio::test]
    async fn feature_types_are_in_the_prelude() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi").unwrap();
        let config = JwtConfig::hs256(SECRET);
        let token = config.encode(&Claims::new("ada").roles(["admin"])).unwrap();

        let c = App::new()
            .jwt(config)
            .get("/me", me)
            .nest(
                "/admin",
                Routes::new()
                    .get("/", admin)
                    .require_roles(["admin"])
                    .wrap(Authenticate::jwt()),
            )
            .get("/visit", visit)
            .post("/fields", fields)
            .get("/ws", socket)
            .get("/hello", ServeFile::new(dir.path().join("hello.txt")))
            .static_files("/static", ServeDir::new(dir.path()))
            .test()
            .with_cookies();

        assert_eq!(c.get("/me").bearer(&token).send().await.text(), "ada");
        assert_eq!(
            c.get("/admin").bearer(&token).send().await.text(),
            "ada true"
        );
        assert_eq!(c.get("/visit").send().await.text(), "0");
        assert_eq!(c.get("/visit").send().await.text(), "1");
        let form = zapline::test::MultipartForm::new()
            .text("a", "1")
            .text("b", "2");
        assert_eq!(c.post("/fields").multipart(form).send().await.text(), "a,b");
        assert_eq!(c.get("/hello").send().await.text(), "hi");
        assert_eq!(c.get("/static/hello.txt").send().await.text(), "hi");
        let r = c
            .get("/ws")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await;
        r.assert_status(StatusCode::SWITCHING_PROTOCOLS);
    }
}
