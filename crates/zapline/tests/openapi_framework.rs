//! OpenAPI coverage of the rest of the framework: credential extractors and
//! auth policies (security requirements, `401`/`403`), plumbing extractors,
//! streaming and multipart bodies, default error responses and
//! infrastructure routes (docs, health, metrics, static files).
#![cfg(all(
    feature = "macros",
    feature = "openapi",
    feature = "validate",
    feature = "query",
    feature = "form"
))]

use std::sync::Arc;

use futures_util::stream::{self, Stream};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zapline::auth::{AllowAnonymous, ApiKey, Authenticate, BasicAuth, BearerToken, CurrentUser};
use zapline::di::{Cached, Dep};
use zapline::health::Health;
use zapline::metrics::Metrics;
use zapline::middleware::RequestId;
use zapline::openapi::{spec_for, ApiInput, AuthKind, OpenApi, OperationDoc};
use zapline::prelude::*;
use zapline::sse::{Event, Sse};
use zapline::stream::{BodyStream, Ndjson, StreamBody};
use zapline::tasks::BackgroundTasks;
use zapline::validate::Valid;

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

/// A documented error response.
fn error(description: &str) -> Value {
    json!({
        "description": description,
        "content": { "application/json": { "schema": schema_ref("Error") } }
    })
}

fn text_ok() -> Value {
    json!({
        "description": "OK",
        "content": { "text/plain": { "schema": { "type": "string" } } }
    })
}

fn paths(spec: &Value) -> Vec<&str> {
    spec["paths"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

// ---------------------------------------------------------------------------
// Credential extractors

#[get("/token")]
async fn token(t: BearerToken) -> String {
    t.into_inner()
}

#[get("/basic")]
async fn basic(auth: BasicAuth) -> String {
    auth.username
}

#[get("/key")]
async fn key(k: ApiKey) -> String {
    k.into_inner()
}

#[get("/me")]
async fn me(CurrentUser(name): CurrentUser<String>) -> String {
    name
}

#[get("/maybe")]
async fn maybe(t: Option<BearerToken>) -> String {
    t.map(BearerToken::into_inner).unwrap_or_default()
}

#[get("/cached")]
async fn cached(Cached(t): Cached<BearerToken>) -> String {
    t.into_inner()
}

#[get("/either", security = "partner")]
async fn either(t: BearerToken) -> String {
    t.into_inner()
}

fn credentials() -> App {
    App::new().mount((token, basic, key, me, maybe, cached, either))
}

#[test]
fn credential_extractors_add_default_schemes() {
    let spec = spec_for(credentials(), OpenApi::new("t", "1"));
    assert_eq!(
        spec["components"]["securitySchemes"],
        json!({
            "bearerAuth": { "type": "http", "scheme": "bearer" },
            "basicAuth": { "type": "http", "scheme": "basic" },
            "apiKeyAuth": { "type": "apiKey", "in": "header", "name": "x-api-key" }
        })
    );
    for (path, scheme) in [
        ("/token", "bearerAuth"),
        ("/basic", "basicAuth"),
        ("/key", "apiKeyAuth"),
        ("/me", "bearerAuth"),
        ("/cached", "bearerAuth"),
    ] {
        let o = op(&spec, path, "get");
        assert_eq!(o["security"], json!([{ scheme: [] }]), "{path}");
        assert_eq!(
            o["responses"],
            json!({ "200": text_ok(), "401": error("Unauthorized") }),
            "{path}"
        );
        // Credentials are a security scheme, not parameters.
        assert!(o.get("parameters").is_none(), "{path}");
    }

    // Optional credentials: anonymous access is an alternative, and the
    // extractor never rejects.
    let optional = op(&spec, "/maybe", "get");
    assert_eq!(optional["security"], json!([{ "bearerAuth": [] }, {}]));
    assert_eq!(optional["responses"], json!({ "200": text_ok() }));

    // Explicit schemes and extractor schemes are alternatives.
    assert_eq!(
        op(&spec, "/either", "get")["security"],
        json!([{ "partner": [] }, { "bearerAuth": [] }])
    );
}

#[test]
fn credential_extractors_use_declared_schemes() {
    let api = OpenApi::new("t", "1")
        .api_key_auth("key", "x-token")
        .basic_auth("ops")
        .bearer_auth("jwt")
        .bearer_auth("other");
    let spec = spec_for(credentials(), api);
    // The first declared scheme of each kind; nothing is added.
    let schemes = spec["components"]["securitySchemes"].as_object().unwrap();
    let mut names: Vec<&str> = schemes.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["jwt", "key", "ops", "other"]);
    for (path, scheme) in [
        ("/token", "jwt"),
        ("/basic", "ops"),
        ("/key", "key"),
        ("/me", "jwt"),
    ] {
        assert_eq!(
            op(&spec, path, "get")["security"],
            json!([{ scheme: [] }]),
            "{path}"
        );
    }
}

#[test]
fn current_user_follows_the_app_security() {
    // The spec-wide default scheme authenticates users...
    let api = || {
        OpenApi::new("t", "1")
            .api_key_auth("key", "x-api-key")
            .security("key")
    };
    let spec = spec_for(App::new().mount((me, token)), api());
    assert_eq!(op(&spec, "/me", "get")["security"], json!([{ "key": [] }]));
    // ...while a bearer extractor still needs a bearer scheme.
    assert_eq!(
        op(&spec, "/token", "get")["security"],
        json!([{ "bearerAuth": [] }])
    );

    // ...and a group's scheme wins over the spec's.
    let app = App::new().nest("/v1", Routes::new().mount(me).security_all("session"));
    let spec = spec_for(app, api());
    assert_eq!(
        op(&spec, "/v1/me", "get")["security"],
        json!([{ "session": [] }])
    );
}

#[test]
fn default_scheme_names_never_clash() {
    let api = OpenApi::new("t", "1").security_scheme(
        "bearerAuth",
        json!({ "type": "apiKey", "in": "header", "name": "x-key" }),
    );
    let spec = spec_for(App::new().mount((token, key)), api);
    // `bearerAuth` is taken by an API key scheme (which `ApiKey` uses).
    assert_eq!(
        op(&spec, "/token", "get")["security"],
        json!([{ "bearerAuth2": [] }])
    );
    assert_eq!(
        op(&spec, "/key", "get")["security"],
        json!([{ "bearerAuth": [] }])
    );
    assert_eq!(
        spec["components"]["securitySchemes"]["bearerAuth2"],
        json!({ "type": "http", "scheme": "bearer" })
    );
}

#[test]
fn custom_extractors_can_require_a_kind_of_scheme() {
    struct Admin;

    impl ApiInput for Admin {
        fn describe_input(op: &mut OperationDoc) {
            op.security_kind(AuthKind::Basic)
                .error_response(401, "Unauthorized");
        }
    }

    impl zapline::extract::FromRequestParts for Admin {
        async fn from_request_parts(_: &mut Request) -> Result<Self> {
            Ok(Admin)
        }
    }

    #[get("/admin")]
    async fn admin(_: Admin) -> &'static str {
        "ok"
    }

    let spec = spec_for(
        App::new().mount(admin),
        OpenApi::new("t", "1").basic_auth("ops"),
    );
    assert_eq!(
        op(&spec, "/admin", "get")["security"],
        json!([{ "ops": [] }])
    );
}

// ---------------------------------------------------------------------------
// Auth policies

fn policies() -> App {
    let admin = Routes::new()
        .get("/stats", || async { "stats" })
        .get("/login", || async { "login" })
        .allow_anonymous()
        .delete("/cache", || async { "purged" })
        .require_roles(["root"])
        .require_roles_all(["admin", "ops"])
        .security_all("jwt");
    App::new()
        .get("/open", || async { "open" })
        .get("/auth", || async { "auth" })
        .require_auth()
        .get("/roles", || async { "roles" })
        .require_roles(["admin", "ops"])
        .get("/scopes", || async { "scopes" })
        .require_scopes(["notes:write", "notes:read"])
        .get("/both", || async { "both" })
        .require_roles(["admin"])
        .require_scopes(["notes:write"])
        .get("/public", || async { "public" })
        .allow_anonymous()
        .get("/documented", || async { "documented" })
        .require_auth()
        .response::<Error>(401, "Token missing or expired")
        .nest("/admin", admin)
}

#[test]
fn policies_become_security_requirements() {
    let spec = spec_for(policies(), OpenApi::new("t", "1"));
    let security = |path: &str, method: &str| op(&spec, path, method)["security"].clone();

    assert!(op(&spec, "/open", "get").get("security").is_none());
    assert_eq!(security("/auth", "get"), json!([{ "bearerAuth": [] }]));
    // Any one role suffices: one alternative per role.
    assert_eq!(
        security("/roles", "get"),
        json!([{ "bearerAuth": ["admin"] }, { "bearerAuth": ["ops"] }])
    );
    // Every scope is needed.
    assert_eq!(
        security("/scopes", "get"),
        json!([{ "bearerAuth": ["notes:write", "notes:read"] }])
    );
    assert_eq!(
        security("/both", "get"),
        json!([{ "bearerAuth": ["admin", "notes:write"] }])
    );
    // Public routes explicitly need nothing.
    assert_eq!(security("/public", "get"), json!([]));

    // Groups name the scheme; route policies and `allow_anonymous` apply.
    assert_eq!(
        security("/admin/stats", "get"),
        json!([{ "jwt": ["admin"] }, { "jwt": ["ops"] }])
    );
    assert_eq!(
        security("/admin/cache", "delete"),
        json!([{ "jwt": ["root"] }])
    );
    assert_eq!(security("/admin/login", "get"), json!([]));

    assert_eq!(
        spec["components"]["securitySchemes"],
        json!({ "bearerAuth": { "type": "http", "scheme": "bearer" } })
    );
}

#[test]
fn policies_document_401_and_403() {
    let spec = spec_for(policies(), OpenApi::new("t", "1"));
    let responses = |path: &str| op(&spec, path, "get")["responses"].clone();
    // Builder routes don't describe their closures: a generic success.
    let ok = json!({ "description": "OK" });

    assert_eq!(responses("/open"), json!({ "200": ok }));
    assert_eq!(responses("/public"), json!({ "200": ok }));
    assert_eq!(responses("/admin/login"), json!({ "200": ok }));
    assert_eq!(
        responses("/auth"),
        json!({ "200": ok, "401": error("Unauthorized") })
    );
    for path in ["/roles", "/scopes", "/both", "/admin/stats"] {
        assert_eq!(
            responses(path),
            json!({
                "200": ok,
                "401": error("Unauthorized"),
                "403": error("Forbidden")
            }),
            "{path}"
        );
    }
    // Documented responses win.
    assert_eq!(
        responses("/documented")["401"],
        error("Token missing or expired")
    );
}

#[test]
fn policies_use_the_spec_default_security() {
    let api = OpenApi::new("t", "1")
        .security_scheme(
            "oauth",
            json!({
                "type": "oauth2",
                "flows": { "clientCredentials": {
                    "tokenUrl": "https://auth.example.com/token",
                    "scopes": { "notes:read": "Read", "notes:write": "Write" }
                } }
            }),
        )
        .security("oauth");
    let spec = spec_for(policies(), api);
    let security = |path: &str| op(&spec, path, "get")["security"].clone();

    // OAuth requirements list scopes only (roles are not OAuth scopes).
    assert_eq!(security("/auth"), json!([{ "oauth": [] }]));
    assert_eq!(security("/roles"), json!([{ "oauth": [] }]));
    assert_eq!(
        security("/scopes"),
        json!([{ "oauth": ["notes:write", "notes:read"] }])
    );
    assert_eq!(security("/both"), json!([{ "oauth": ["notes:write"] }]));
    assert_eq!(security("/public"), json!([]));
    // Without a policy the spec-wide default applies.
    assert!(op(&spec, "/open", "get").get("security").is_none());
    assert_eq!(spec["security"], json!([{ "oauth": [] }]));
    // No default bearer scheme was needed.
    assert!(spec["components"]["securitySchemes"]
        .get("bearerAuth")
        .is_none());
}

#[test]
fn policies_and_credential_extractors_combine() {
    #[get("/optional")]
    async fn optional(user: Option<CurrentUser<String>>) -> String {
        user.map(CurrentUser::into_inner).unwrap_or_default()
    }

    let app = App::new()
        .mount(me)
        .require_scopes(["profile"])
        .mount(optional)
        .allow_anonymous()
        .nest("/strict", Routes::new().mount(optional).require_auth_all());
    let spec = spec_for(app, OpenApi::new("t", "1").bearer_auth("jwt"));
    let me_op = op(&spec, "/me", "get");
    assert_eq!(me_op["security"], json!([{ "jwt": ["profile"] }]));
    assert_eq!(me_op["responses"]["401"], error("Unauthorized"));
    assert_eq!(me_op["responses"]["403"], error("Forbidden"));
    // Optional user on a public route: anonymous is an alternative.
    let public = op(&spec, "/optional", "get");
    assert_eq!(public["security"], json!([{ "jwt": [] }, {}]));
    assert!(public["responses"].get("401").is_none());
    // A policy makes authentication required anyway.
    let strict = op(&spec, "/strict/optional", "get");
    assert_eq!(strict["security"], json!([{ "jwt": [] }]));
    assert_eq!(strict["responses"]["401"], error("Unauthorized"));
}

// ---------------------------------------------------------------------------
// Plumbing extractors, bodies and streaming responses

trait Clock: Send + Sync {
    fn now(&self) -> u64;
}

#[derive(Deserialize, JsonSchema, validator::Validate)]
struct Page {
    page: u32,
}

#[post("/plumbing/{id}")]
#[allow(clippy::too_many_arguments)]
async fn plumbing(
    Path(id): Path<u32>,
    clock: Dep<dyn Clock>,
    tasks: BackgroundTasks,
    request_id: Option<RequestId>,
    conn: ConnInfo,
    MatchedPath(matched): MatchedPath,
    State(n): State<u32>,
    method: Method,
) -> StatusCode {
    let _ = (id, clock.now(), request_id, conn, matched, n, method);
    tasks.spawn(async {});
    StatusCode::NO_CONTENT
}

#[test]
fn plumbing_extractors_are_known_but_add_nothing() {
    fn known<T: ApiInput>() {}
    known::<Dep<dyn Clock>>();
    known::<Dep<u32>>();
    known::<BackgroundTasks>();
    known::<RequestId>();
    known::<ConnInfo>();
    known::<MatchedPath>();
    known::<State<u32>>();
    known::<Extension<u32>>();
    known::<Method>();
    known::<HeaderMap>();
    known::<Request>();
    known::<Cached<BearerToken>>();

    let spec = spec_for(App::new().mount(plumbing), OpenApi::new("t", "1"));
    let o = op(&spec, "/plumbing/{id}", "post");
    let params = o["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 1, "{params:#?}");
    assert_eq!(params[0]["name"], "id");
    assert!(o.get("requestBody").is_none());
    assert!(o.get("security").is_none());
    assert_eq!(
        o["responses"],
        json!({
            "200": { "description": "OK" },
            "400": error("Invalid request parameters")
        })
    );
}

#[post("/upload")]
async fn upload(body: BodyStream) -> StatusCode {
    drop(body);
    StatusCode::ACCEPTED
}

#[get("/events")]
async fn events() -> Sse<impl Stream<Item = Event>> {
    Sse::new(stream::iter([Event::default().data("hi")]))
}

#[get("/download")]
async fn download() -> Result<StreamBody<impl Stream<Item = Result<Bytes, std::io::Error>>>> {
    Ok(StreamBody::new(stream::iter([Ok(Bytes::from("data"))])))
}

#[get("/export")]
async fn export() -> Ndjson {
    Ndjson::new(stream::iter([json!({ "id": 1 })]))
}

#[derive(Serialize, JsonSchema)]
struct Profile {
    name: String,
}

#[test]
fn streaming_bodies_and_responses() {
    let app = App::new().mount((upload, events, download, export));
    let spec = spec_for(app, OpenApi::new("t", "1"));

    assert_eq!(
        op(&spec, "/upload", "post")["requestBody"],
        json!({ "required": true, "content": { "application/octet-stream": {} } })
    );
    assert_eq!(
        op(&spec, "/events", "get")["responses"],
        json!({ "200": {
            "description": "OK",
            "content": { "text/event-stream": { "schema": { "type": "string" } } }
        } })
    );
    let streamed = &op(&spec, "/download", "get")["responses"];
    assert_eq!(
        streamed["200"]["content"],
        json!({ "application/octet-stream": {} })
    );
    assert_eq!(streamed["default"], error("Error"));
    assert_eq!(
        op(&spec, "/export", "get")["responses"]["200"]["content"],
        json!({ "application/x-ndjson": {} })
    );
}

#[tokio::test]
async fn streaming_handlers_still_work() {
    let c = App::new().mount((events, download, export)).test();
    assert!(c.get("/events").send().await.text().contains("data: hi"));
    assert_eq!(c.get("/download").send().await.text(), "data");
    assert_eq!(c.get("/export").send().await.text(), "{\"id\":1}\n");
}

#[cfg(feature = "multipart")]
#[test]
fn multipart_request_body() {
    use zapline::multipart::Multipart;

    #[post("/files")]
    async fn files(mut form: Multipart) -> Result<String> {
        let mut names = Vec::new();
        while let Some(field) = form.next_field().await? {
            names.push(field.name().unwrap_or_default().to_owned());
        }
        Ok(names.join(","))
    }

    let spec = spec_for(App::new().mount(files), OpenApi::new("t", "1"));
    assert_eq!(
        op(&spec, "/files", "post")["requestBody"],
        json!({
            "required": true,
            "content": { "multipart/form-data": { "schema": { "type": "object" } } }
        })
    );
}

#[cfg(feature = "ws")]
#[test]
fn websocket_handshake_documents_101() {
    use zapline::ws::WebSocketUpgrade;

    #[get("/ws")]
    async fn chat(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|_socket| async {})
    }

    #[get("/ws2")]
    async fn chat2(ws: WebSocketUpgrade) -> Result<Response> {
        Ok(ws.on_upgrade(|_socket| async {}))
    }

    let spec = spec_for(App::new().mount((chat, chat2)), OpenApi::new("t", "1"));
    assert_eq!(
        op(&spec, "/ws", "get")["responses"],
        json!({ "101": { "description": "Switching Protocols" } })
    );
    let r = &op(&spec, "/ws2", "get")["responses"];
    assert_eq!(r["101"], json!({ "description": "Switching Protocols" }));
    assert!(r.get("200").is_none());
    assert!(r["default"].is_object());
}

#[cfg(feature = "cookies")]
#[test]
fn cookies_sessions_and_csrf() {
    use zapline::auth::CsrfToken;
    use zapline::cookies::{CookieJar, PrivateCookieJar, SignedCookieJar};
    use zapline::openapi::ApiOutput;
    use zapline::session::Session;

    fn known<T: ApiInput>() {}
    known::<CookieJar>();
    known::<SignedCookieJar>();
    known::<PrivateCookieJar>();
    known::<Session>();
    known::<CsrfToken>();

    #[post("/login")]
    async fn login(
        jar: CookieJar,
        _session: Session,
        _csrf: CsrfToken,
    ) -> (StatusCode, CookieJar, Json<Profile>) {
        (
            StatusCode::CREATED,
            jar,
            Json(Profile { name: "ada".into() }),
        )
    }

    let app = App::new()
        .mount(login)
        .get("/logout", || async { "" })
        .api_doc(<CookieJar as ApiOutput>::describe_output)
        .get("/signed", || async { "" })
        .api_doc(<(SignedCookieJar, Json<Profile>) as ApiOutput>::describe_output)
        .get("/private", || async { "" })
        .api_doc(<(StatusCode, PrivateCookieJar, Html<String>) as ApiOutput>::describe_output);
    let spec = spec_for(app, OpenApi::new("t", "1"));

    let o = op(&spec, "/login", "post");
    assert!(o.get("parameters").is_none() && o.get("requestBody").is_none());
    assert_eq!(
        o["responses"]["200"]["content"]["application/json"]["schema"],
        schema_ref("Profile")
    );
    assert_eq!(
        op(&spec, "/logout", "get")["responses"],
        json!({ "200": { "description": "OK" } })
    );
    assert_eq!(
        op(&spec, "/signed", "get")["responses"]["200"]["content"]["application/json"]["schema"],
        schema_ref("Profile")
    );
    assert!(op(&spec, "/private", "get")["responses"]["200"]["content"]["text/html"].is_object());
}

#[cfg(feature = "jwt")]
#[test]
fn jwt_extractor_requires_bearer() {
    use zapline::auth::{Claims, Jwt};

    #[get("/claims")]
    async fn claims(Jwt(c): Jwt) -> String {
        c.sub
    }

    #[get("/custom")]
    async fn custom(Jwt(c): Jwt<Claims>) -> String {
        c.sub
    }

    let spec = spec_for(
        App::new().mount((claims, custom)),
        OpenApi::new("t", "1").bearer_auth("jwt"),
    );
    for path in ["/claims", "/custom"] {
        let o = op(&spec, path, "get");
        assert_eq!(o["security"], json!([{ "jwt": [] }]), "{path}");
        assert_eq!(o["responses"]["401"], error("Unauthorized"), "{path}");
    }
}

// ---------------------------------------------------------------------------
// Default error responses

#[derive(Deserialize, JsonSchema, validator::Validate)]
struct NewProfile {
    #[validate(length(min = 1))]
    name: String,
}

#[get("/by-id/{id}")]
async fn by_id(Path(id): Path<u64>, Query(p): Query<Page>) -> String {
    format!("{id}/{}", p.page)
}

#[get("/maybe-page")]
async fn maybe_page(p: Option<Query<Page>>) -> String {
    p.map_or(0, |p| p.page).to_string()
}

#[post("/profiles")]
async fn create_profile(Json(p): Json<NewProfile>) -> Json<Profile> {
    Json(Profile { name: p.name })
}

#[post("/valid-profiles")]
async fn create_valid(Valid(Json(p)): Valid<Json<NewProfile>>) -> Json<Profile> {
    Json(Profile { name: p.name })
}

#[post("/form-profiles")]
async fn create_form(Form(p): Form<NewProfile>) -> String {
    p.name
}

#[get("/valid-page")]
async fn valid_page(Valid(Query(p)): Valid<Query<Page>>) -> String {
    p.page.to_string()
}

#[test]
fn extractor_failures_are_documented() {
    let app = App::new().mount((
        by_id,
        maybe_page,
        create_profile,
        create_valid,
        create_form,
        valid_page,
    ));
    let spec = spec_for(app, OpenApi::new("t", "1"));
    let responses = |path: &str, method: &str| op(&spec, path, method)["responses"].clone();

    // Path and query parameters that don't parse: 400.
    let r = responses("/by-id/{id}", "get");
    assert_eq!(r["400"], error("Invalid request parameters"));
    assert!(r.get("422").is_none());
    // Optional extractors never fail.
    assert_eq!(responses("/maybe-page", "get"), json!({ "200": text_ok() }));
    // Bodies that don't deserialize: 422.
    assert_eq!(
        responses("/profiles", "post")["422"],
        error("Invalid request body")
    );
    assert_eq!(
        responses("/form-profiles", "post")["422"],
        error("Invalid request body")
    );
    // Validation failures: 422 as well.
    let valid = responses("/valid-profiles", "post");
    assert_eq!(valid["422"], error("Validation Error"));
    assert!(valid.get("400").is_none());
    let valid = responses("/valid-page", "get");
    assert_eq!(valid["400"], error("Invalid request parameters"));
    assert_eq!(valid["422"], error("Validation Error"));
}

// ---------------------------------------------------------------------------
// Infrastructure routes

#[test]
fn health_and_metrics_are_hidden_by_default() {
    let app = || {
        App::new()
            .get("/api", || async { "api" })
            .plugin(Health::new())
            .plugin(Metrics::new().track_unmatched(false))
    };
    let spec = spec_for(app(), OpenApi::new("t", "1"));
    assert_eq!(paths(&spec), ["/api"]);

    // Health checks are public; metrics follow the app's auth.
    let table = app().route_table();
    for r in &table {
        let anonymous = r.meta::<AllowAnonymous>().is_some();
        assert_eq!(anonymous, r.path().starts_with("/health"), "{}", r.path());
    }
}

#[test]
fn health_and_metrics_can_be_documented() {
    let app = App::new()
        .plugin(Health::new().live_path("").document(true))
        .plugin(
            Metrics::new()
                .path("/internal/metrics")
                .track_unmatched(false)
                .document(true),
        );
    let spec = spec_for(app, OpenApi::new("t", "1"));
    let mut documented = paths(&spec);
    documented.sort_unstable();
    assert_eq!(
        documented,
        ["/health", "/health/ready", "/internal/metrics"]
    );

    let ready = op(&spec, "/health/ready", "get");
    assert_eq!(ready["summary"], "Readiness probe");
    assert_eq!(ready["tags"], json!(["health"]));
    // Public even when the spec requires auth elsewhere.
    assert_eq!(ready["security"], json!([]));
    let report = json!({ "application/json": { "schema": schema_ref("HealthReport") } });
    assert_eq!(ready["responses"]["200"]["content"], report);
    assert_eq!(ready["responses"]["503"]["content"], report);
    assert_eq!(op(&spec, "/health", "get")["summary"], "Health report");
    let schema = &spec["components"]["schemas"]["HealthReport"];
    assert_eq!(schema["required"], json!(["status"]));
    assert_eq!(
        schema["properties"]["status"]["enum"],
        json!(["up", "degraded", "down"])
    );

    let metrics = op(&spec, "/internal/metrics", "get");
    assert_eq!(metrics["summary"], "Prometheus metrics");
    assert_eq!(metrics["tags"], json!(["metrics"]));
    assert!(metrics.get("security").is_none());
    assert_eq!(metrics["responses"], json!({ "200": text_ok() }));
}

#[tokio::test]
async fn docs_stay_reachable_behind_required_auth() {
    let auth = Authenticate::new(|req: &Request| {
        let user = req
            .headers()
            .get("x-user")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        async move { Ok(user) }
    })
    .required();
    let c = App::new()
        .get("/api", || async { "api" })
        .plugin(Health::new())
        .plugin(Metrics::new().track_unmatched(false))
        .openapi(OpenApi::new("t", "1"))
        .wrap_route(auth)
        .test();

    for path in [
        "/openapi.json",
        "/docs",
        "/health/live",
        "/health/ready",
        "/health",
    ] {
        assert_eq!(c.get(path).send().await.status(), 200, "{path}");
    }
    // Metrics and the API itself stay protected.
    for path in ["/metrics", "/api"] {
        assert_eq!(c.get(path).send().await.status(), 401, "{path}");
        let r = c.get(path).header("x-user", "ada").send().await;
        assert_eq!(r.status(), 200, "{path}");
    }

    let spec = c.get("/openapi.json").send().await.json::<Value>();
    assert_eq!(paths(&spec), ["/api"]);
    // The docs routes are hidden and public.
    let docs = c
        .routes()
        .iter()
        .filter(|r| r.path() == "/openapi.json" || r.path() == "/docs")
        .collect::<Vec<_>>();
    assert_eq!(docs.len(), 2);
    for r in docs {
        assert!(r.meta::<AllowAnonymous>().is_some());
        assert!(r.meta::<OperationDoc>().is_some());
    }
}

#[cfg(feature = "fs")]
#[tokio::test]
async fn static_files_are_left_out() {
    use zapline::fs::{ServeDir, ServeFile};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.js"), "js").unwrap();
    let app = || {
        let assets = Routes::new().static_files("/assets", ServeDir::new(dir.path()));
        App::new()
            .get("/api", || async { "api" })
            .static_files("/", ServeDir::new(dir.path()))
            .nest("/v1", assets)
            .get("/favicon.ico", ServeFile::new(dir.path().join("app.js")))
    };
    let spec = spec_for(app(), OpenApi::new("t", "1"));
    let mut documented = paths(&spec);
    documented.sort_unstable();
    // A plain `ServeFile` route is an ordinary (undescribed) route.
    assert_eq!(documented, ["/api", "/favicon.ico"]);

    let c = app().test();
    assert_eq!(c.get("/v1/assets/app.js").send().await.text(), "js");
    assert_eq!(c.get("/app.js").send().await.text(), "js");
}

/// Dependency for `Dep<dyn Clock>`.
struct Fixed;

impl Clock for Fixed {
    fn now(&self) -> u64 {
        7
    }
}

#[tokio::test]
async fn plumbing_handler_still_works() {
    let c = App::new()
        .state(1u32)
        .provide_dyn::<dyn Clock>(Arc::new(Fixed))
        .mount(plumbing)
        .test();
    assert_eq!(c.post("/plumbing/3").send().await.status(), 204);
    assert_eq!(c.post("/plumbing/x").send().await.status(), 400);
}
