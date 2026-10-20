//! End to end: an app using JWT auth policies, dependency injection,
//! background tasks, multipart uploads, SSE, raw body streams, validation,
//! health checks, metrics and static files, and the spec it documents.
#![cfg(all(feature = "jwt", feature = "multipart", feature = "fs"))]

use std::collections::BTreeSet;
use std::path::Path as FsPath;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{self, Stream};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use validator::Validate;
use zapline::auth::{Authenticate, Claims, CurrentUser, JwtConfig};
use zapline::di::Dep;
use zapline::fs::ServeDir;
use zapline::health::Health;
use zapline::metrics::Metrics;
use zapline::multipart::Multipart;
use zapline::openapi::{spec_for, OpenApi};
use zapline::prelude::*;
use zapline::sse::{Event, Sse};
use zapline::stream::BodyStream;
use zapline::tasks::BackgroundTasks;
use zapline::validate::Valid;

const SECRET: &str = "openapi-integration-secret-of-32-bytes";
const BOUNDARY: &str = "X-ZAPLINE-BOUNDARY";

// ---------------------------------------------------------------------------
// The app

/// A note.
#[derive(Clone, Serialize, JsonSchema)]
struct Note {
    id: u64,
    title: String,
    author: String,
}

/// A new note.
#[derive(Deserialize, JsonSchema, Validate)]
struct NewNote {
    #[validate(length(min = 1, max = 100))]
    title: String,
}

#[derive(Deserialize, JsonSchema)]
struct Page {
    /// Notes to skip.
    #[serde(default)]
    offset: usize,
    /// Maximum number of notes.
    limit: Option<usize>,
}

/// Files attached to a note.
#[derive(Serialize, JsonSchema)]
struct Attachment {
    note: u64,
    files: Vec<String>,
    bytes: usize,
}

/// Note storage, injected as `Dep<dyn Notes>`.
trait Notes: Send + Sync {
    fn add(&self, title: String, author: String) -> Note;
    fn list(&self) -> Vec<Note>;
    fn clear(&self);
}

#[derive(Default)]
struct MemoryNotes {
    next: AtomicU64,
    notes: Mutex<Vec<Note>>,
}

impl Notes for MemoryNotes {
    fn add(&self, title: String, author: String) -> Note {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let note = Note { id, title, author };
        self.notes.lock().unwrap().push(note.clone());
        note
    }

    fn list(&self) -> Vec<Note> {
        self.notes.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.notes.lock().unwrap().clear();
    }
}

/// Written by background tasks.
#[derive(Clone, Default)]
struct Audit(Arc<Mutex<Vec<String>>>);

/// List notes.
#[get("/notes", tag = "notes")]
async fn list_notes(notes: Dep<dyn Notes>, Query(page): Query<Page>) -> Json<Vec<Note>> {
    let all = notes.list().into_iter().skip(page.offset);
    Json(all.take(page.limit.unwrap_or(usize::MAX)).collect())
}

/// Create a note.
///
/// The author is the authenticated user.
#[post("/notes", tag = "notes", status = 201)]
async fn create_note(
    CurrentUser(user): CurrentUser<Claims>,
    notes: Dep<dyn Notes>,
    tasks: BackgroundTasks,
    State(audit): State<Audit>,
    Valid(Json(new)): Valid<Json<NewNote>>,
) -> (StatusCode, Json<Note>) {
    let note = notes.add(new.title, user.sub);
    let line = format!("created {}", note.id);
    tasks.spawn(async move { audit.0.lock().unwrap().push(line) });
    (StatusCode::CREATED, Json(note))
}

/// Delete every note.
#[delete("/notes", tag = "notes", status = 204)]
async fn purge(notes: Dep<dyn Notes>) -> StatusCode {
    notes.clear();
    StatusCode::NO_CONTENT
}

/// Attach files to a note.
#[post("/notes/{id}/attachments", tag = "notes")]
async fn attach(Path(id): Path<u64>, mut form: Multipart) -> Result<Json<Attachment>> {
    let mut files = Vec::new();
    let mut bytes = 0;
    while let Some(field) = form.next_field().await? {
        files.push(field.file_name().unwrap_or("-").to_owned());
        bytes += field.bytes().await?.len();
    }
    Ok(Json(Attachment {
        note: id,
        files,
        bytes,
    }))
}

/// Import notes, one title per line.
#[post("/import", tag = "notes")]
async fn import(
    CurrentUser(user): CurrentUser<Claims>,
    notes: Dep<dyn Notes>,
    mut body: BodyStream,
) -> Result<String> {
    let mut raw = Vec::new();
    while let Some(chunk) = body.chunk().await? {
        raw.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(raw).map_err(|_| Error::bad_request("not UTF-8"))?;
    let titles: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    for title in &titles {
        notes.add((*title).to_owned(), user.sub.clone());
    }
    Ok(titles.len().to_string())
}

/// Note titles as server-sent events.
#[get("/events", tag = "notes")]
async fn events(notes: Dep<dyn Notes>) -> Sse<impl Stream<Item = Event>> {
    let events: Vec<Event> = notes
        .list()
        .into_iter()
        .map(|n| Event::default().event("note").data(n.title))
        .collect();
    Sse::new(stream::iter(events))
}

/// Service status.
#[get("/status")]
async fn status() -> &'static str {
    "ok"
}

fn app(assets: &FsPath, audit: Audit) -> App {
    let v1 = Routes::new()
        .mount((list_notes, create_note, attach, import, events))
        .mount(purge)
        .require_roles(["admin"])
        .mount(status)
        .allow_anonymous()
        .require_auth_all()
        .tag_all("v1");
    App::new()
        .jwt(JwtConfig::hs256(SECRET))
        .provide_dyn::<dyn Notes>(Arc::new(MemoryNotes::default()))
        .state(audit)
        .nest("/v1", v1)
        .static_files("/assets", ServeDir::new(assets))
        .plugin(Health::new().check("notes", |_res: Resources| async { Ok(()) }))
        .plugin(Metrics::new().track_unmatched(false))
        .wrap_route(Authenticate::jwt())
}

fn api() -> OpenApi {
    OpenApi::new("Notes API", "1.0.0")
        .tag("notes", "Manage notes")
        .bearer_auth("jwt")
}

fn assets() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Helpers

fn op<'a>(spec: &'a Value, path: &str, method: &str) -> &'a Value {
    let o = &spec["paths"][path][method];
    assert!(
        o.is_object(),
        "missing {method} {path} in {:#}",
        spec["paths"]
    );
    o
}

fn schema_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

fn error(description: &str) -> Value {
    json!({
        "description": description,
        "content": { "application/json": { "schema": schema_ref("Error") } }
    })
}

fn codes(o: &Value) -> Vec<&str> {
    o["responses"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn token(roles: &[&str]) -> String {
    let claims = Claims::new("ada")
        .roles(roles.iter().copied())
        .expires_in(Duration::from_secs(600));
    JwtConfig::hs256(SECRET).encode(&claims).unwrap()
}

fn multipart(files: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in files {
        out.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
                 filename=\"{name}\"\r\nContent-Type: text/plain\r\n\r\n{data}\r\n"
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x) {
                    ("$ref", Value::String(r)) => out.push(r.clone()),
                    _ => collect_refs(x, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// The spec

#[test]
fn documents_the_api_and_nothing_else() {
    let dir = assets();
    let spec = spec_for(app(dir.path(), Audit::default()), api());

    let operations: BTreeSet<String> = spec["paths"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(path, item)| {
            item.as_object()
                .unwrap()
                .keys()
                .map(move |m| format!("{m} {path}"))
        })
        .collect();
    let expected: BTreeSet<String> = [
        "get /v1/notes",
        "post /v1/notes",
        "delete /v1/notes",
        "post /v1/notes/{id}/attachments",
        "post /v1/import",
        "get /v1/events",
        "get /v1/status",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    // No docs, health, metrics or static file routes.
    assert_eq!(operations, expected);

    // The declared bearer scheme is used; no default is added.
    assert_eq!(
        spec["components"]["securitySchemes"],
        json!({ "jwt": { "type": "http", "scheme": "bearer", "bearerFormat": "JWT" } })
    );
    assert!(spec.get("security").is_none());
    assert_eq!(
        spec["tags"],
        json!([{ "name": "notes", "description": "Manage notes" }])
    );

    // Every reference resolves.
    let schemas = spec["components"]["schemas"].as_object().unwrap();
    let mut refs = Vec::new();
    collect_refs(&spec, &mut refs);
    for r in &refs {
        let name = r.strip_prefix("#/components/schemas/").unwrap();
        assert!(schemas.contains_key(name), "dangling {r}");
    }
    let mut names: Vec<&str> = schemas.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["Attachment", "Error", "NewNote", "Note"]);
}

#[test]
fn auth_policies_become_security() {
    let dir = assets();
    let spec = spec_for(app(dir.path(), Audit::default()), api());
    let jwt = json!([{ "jwt": [] }]);
    for (path, method) in [
        ("/v1/notes", "get"),
        ("/v1/notes", "post"),
        ("/v1/notes/{id}/attachments", "post"),
        ("/v1/import", "post"),
        ("/v1/events", "get"),
    ] {
        let o = op(&spec, path, method);
        assert_eq!(o["security"], jwt, "{method} {path}");
        assert_eq!(
            o["responses"]["401"],
            error("Unauthorized"),
            "{method} {path}"
        );
        assert!(o["responses"].get("403").is_none(), "{method} {path}");
        assert_eq!(o["tags"], json!(["v1", "notes"]), "{method} {path}");
    }

    let purge_op = op(&spec, "/v1/notes", "delete");
    assert_eq!(purge_op["security"], json!([{ "jwt": ["admin"] }]));
    assert_eq!(codes(purge_op), ["204", "401", "403"]);
    assert_eq!(purge_op["responses"]["403"], error("Forbidden"));

    let status_op = op(&spec, "/v1/status", "get");
    assert_eq!(status_op["security"], json!([]));
    assert_eq!(codes(status_op), ["200"]);
    assert_eq!(status_op["tags"], json!(["v1"]));
}

#[test]
fn operations_are_fully_described() {
    let dir = assets();
    let spec = spec_for(app(dir.path(), Audit::default()), api());

    // DI, background tasks and state add nothing; validation adds a 422.
    let create = op(&spec, "/v1/notes", "post");
    assert_eq!(create["operationId"], "create_note");
    assert_eq!(create["summary"], "Create a note.");
    assert_eq!(
        create["description"],
        "The author is the authenticated user."
    );
    assert!(create.get("parameters").is_none());
    assert_eq!(
        create["requestBody"],
        json!({
            "required": true,
            "content": { "application/json": { "schema": schema_ref("NewNote") } }
        })
    );
    assert_eq!(codes(create), ["201", "401", "422"]);
    assert_eq!(
        create["responses"]["201"]["content"]["application/json"]["schema"],
        schema_ref("Note")
    );
    assert_eq!(create["responses"]["422"], error("Validation Error"));

    let list = op(&spec, "/v1/notes", "get");
    let params: Vec<(&str, bool)> = list["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap(),
                p["required"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(params, [("limit", false), ("offset", false)]);
    assert_eq!(codes(list), ["200", "400", "401"]);
    assert_eq!(
        list["responses"]["400"],
        error("Invalid request parameters")
    );

    let attach_op = op(&spec, "/v1/notes/{id}/attachments", "post");
    assert_eq!(attach_op["parameters"][0]["name"], "id");
    assert_eq!(attach_op["parameters"][0]["schema"]["type"], "integer");
    assert_eq!(
        attach_op["requestBody"]["content"],
        json!({ "multipart/form-data": { "schema": { "type": "object" } } })
    );
    assert_eq!(codes(attach_op), ["200", "400", "401", "default"]);
    assert_eq!(
        attach_op["responses"]["200"]["content"]["application/json"]["schema"],
        schema_ref("Attachment")
    );

    let import_op = op(&spec, "/v1/import", "post");
    assert_eq!(
        import_op["requestBody"]["content"],
        json!({ "application/octet-stream": {} })
    );
    assert_eq!(codes(import_op), ["200", "401", "default"]);

    let events_op = op(&spec, "/v1/events", "get");
    assert_eq!(
        events_op["responses"]["200"]["content"],
        json!({ "text/event-stream": { "schema": { "type": "string" } } })
    );
}

// ---------------------------------------------------------------------------
// The running app

#[tokio::test]
async fn serves_the_spec_and_the_api() {
    let dir = assets();
    let audit = Audit::default();
    let c = app(dir.path(), audit.clone()).openapi(api()).test();
    let user = token(&[]);
    let admin = token(&["admin"]);

    // Docs and probes are public; the served spec is the generated one.
    let r = c.get("/openapi.json").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>(),
        spec_for(app(dir.path(), Audit::default()), api())
    );
    assert_eq!(c.get("/docs").send().await.status(), 200);
    assert_eq!(c.get("/health/ready").send().await.status(), 200);
    assert_eq!(c.get("/v1/status").send().await.text(), "ok");
    // Metrics, static files and the API need a token.
    for path in ["/metrics", "/assets/readme.txt", "/v1/notes"] {
        assert_eq!(c.get(path).send().await.status(), 401, "{path}");
    }
    assert_eq!(
        c.get("/assets/readme.txt")
            .bearer(&user)
            .send()
            .await
            .text(),
        "hello"
    );
    assert_eq!(c.get("/metrics").bearer(&user).send().await.status(), 200);

    // Validation, DI and background tasks.
    let r = c
        .post("/v1/notes")
        .bearer(&user)
        .json(&json!({ "title": "" }))
        .send()
        .await;
    assert_eq!(r.status(), 422);
    let r = c
        .post("/v1/notes")
        .bearer(&user)
        .json(&json!({ "title": "first" }))
        .send()
        .await;
    assert_eq!(r.status(), 201);
    assert_eq!(
        r.json::<Value>(),
        json!({ "id": 1, "title": "first", "author": "ada" })
    );
    for _ in 0..100 {
        if !audit.0.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(*audit.0.lock().unwrap(), ["created 1"]);

    // Raw body streams and multipart uploads.
    let r = c
        .post("/v1/import")
        .bearer(&user)
        .body("second\nthird\n")
        .send()
        .await;
    assert_eq!(r.text(), "2");
    let r = c
        .post("/v1/notes/1/attachments")
        .bearer(&user)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart(&[("a.txt", "aaa"), ("b.txt", "bb")]))
        .send()
        .await;
    assert_eq!(
        r.json::<Value>(),
        json!({ "note": 1, "files": ["a.txt", "b.txt"], "bytes": 5 })
    );
    let r = c.get("/v1/notes?offset=1").bearer(&user).send().await;
    assert_eq!(r.json::<Value>().as_array().unwrap().len(), 2);
    let sse = c.get("/v1/events").bearer(&user).send().await.text();
    assert!(sse.contains("event: note\ndata: third\n"), "{sse}");

    // Role policies.
    assert_eq!(
        c.delete("/v1/notes").bearer(&user).send().await.status(),
        403
    );
    assert_eq!(
        c.delete("/v1/notes").bearer(&admin).send().await.status(),
        204
    );
}
