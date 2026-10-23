//! OpenAPI generation: macro routes, builder routes, nested groups, schemas,
//! docs pages and export.
#![cfg(all(
    feature = "macros",
    feature = "openapi",
    feature = "validate",
    feature = "query"
))]

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use validator::Validate;
use zapline::extract::FromRequestParts;
use zapline::openapi::{
    export_spec, spec_for, ApiInput, DocsUi, OpenApi, OperationDoc, ParamIn, ENV_EXPORT_OPENAPI,
};
use zapline::prelude::*;
use zapline::validate::Valid;

// ---------------------------------------------------------------------------
// A sample app

/// A postal address.
#[derive(Serialize, Deserialize, JsonSchema, Validate, Clone)]
struct Address {
    street: String,
    #[validate(length(equal = 5))]
    zip: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
struct User {
    id: u64,
    name: String,
    address: Address,
}

#[derive(Deserialize, JsonSchema, Validate)]
struct NewUser {
    #[validate(length(min = 1))]
    name: String,
    #[validate(nested)]
    address: Address,
}

/// Optional profile data: request and response schemas differ (`Option`
/// fields are required when serialized), so the request side becomes
/// `ProfileInput`.
#[derive(Serialize, Deserialize, JsonSchema)]
struct Profile {
    bio: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct Pagination {
    /// Page number, starting at 1.
    page: u32,
    per_page: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct OrgUser {
    org: String,
    id: u64,
}

#[derive(Serialize, JsonSchema)]
struct Stats {
    users: u64,
}

#[derive(Deserialize, JsonSchema)]
struct Reindex {
    full: bool,
}

/// No `JsonSchema`: still works as an argument and return type.
#[derive(Serialize, Deserialize)]
struct Plain {
    x: u32,
}

/// Get a user.
///
/// Looks the user up by id.
/// Returns 404 when missing.
#[get("/users/{id}", tag = "users")]
async fn get_user(Path(id): Path<u64>) -> Option<Json<User>> {
    (id == 1).then(|| {
        Json(User {
            id,
            name: "ada".into(),
            address: Address {
                street: "Main".into(),
                zip: "12345".into(),
            },
        })
    })
}

/// List users.
#[get("/users", tag = "users")]
async fn list_users(Query(p): Query<Pagination>) -> Json<Vec<User>> {
    let _ = (p.page, p.per_page);
    Json(Vec::new())
}

/// Create a user.
#[post("/users", tag = "users", status = 201, security = "bearer")]
async fn create_user(Valid(Json(u)): Valid<Json<NewUser>>) -> Result<(StatusCode, Json<User>)> {
    Ok((
        StatusCode::CREATED,
        Json(User {
            id: 2,
            name: u.name,
            address: u.address,
        }),
    ))
}

#[put(
    "/orgs/{org}/users/{id}",
    tags("orgs", "users"),
    operation_id = "replaceOrgUser",
    summary = "Replace a profile",
    deprecated
)]
async fn org_user(Path(p): Path<OrgUser>, Json(profile): Json<Profile>) -> Json<Profile> {
    let _ = (p.org, p.id);
    Json(profile)
}

#[get("/pair/{a}/{b}")]
async fn pair(Path((a, b)): Path<(String, u32)>) -> String {
    format!("{a}{b}")
}

#[delete("/internal", hidden)]
async fn internal() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[route("/multi", method = "GET", method = "PUT")]
async fn multi(m: Method) -> Html<String> {
    Html(m.to_string())
}

#[get("/go")]
async fn go() -> Redirect {
    Redirect::to("/users")
}

#[get("/files/{*rest}")]
async fn files(Path(rest): Path<String>) -> Vec<u8> {
    rest.into_bytes()
}

#[post("/raw")]
async fn raw(body: Bytes) -> String {
    body.len().to_string()
}

#[get("/opt")]
async fn opt(q: Option<Query<Pagination>>) -> &'static str {
    if q.is_some() {
        "some"
    } else {
        "none"
    }
}

#[post("/plain")]
async fn plain(Json(p): Json<Plain>) -> Json<Plain> {
    Json(p)
}

/// A custom extractor that documents itself.
struct ApiKey(#[allow(dead_code)] String);

impl FromRequestParts for ApiKey {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(|v| ApiKey(v.to_owned()))
            .ok_or_else(Error::unauthorized)
    }
}

impl ApiInput for ApiKey {
    fn describe_input(op: &mut OperationDoc) {
        op.param::<String>(ParamIn::Header, "x-api-key", true)
            .security("api_key");
    }
}

#[get("/secret")]
async fn secret(_key: ApiKey, State(n): State<u32>) -> impl IntoResponse {
    n.to_string()
}

fn app() -> App {
    let admin = Routes::new()
        .get("/stats", || async { Json(Stats { users: 1 }) })
        .summary("Admin stats")
        .response::<Stats>(200, "Statistics")
        .post("/reindex", |Json(r): Json<Reindex>| async move {
            let _ = r.full;
            StatusCode::ACCEPTED
        })
        .request_body::<Reindex>()
        .api_doc(|d| {
            d.response_empty(202, "Reindex started");
        })
        .tag_all("admin")
        .security_all("bearer");

    App::new()
        .state(7u32)
        .mount((
            get_user,
            list_users,
            create_user,
            org_user,
            pair,
            internal,
            multi,
            go,
            files,
            raw,
            opt,
            plain,
        ))
        .mount(secret)
        .get("/health", || async { "ok" })
        .summary("Health check")
        .tag("ops")
        .get("/legacy", || async { "old" })
        .deprecated()
        .operation_id("legacyEndpoint")
        .description("Use /health instead.")
        .get("/hidden-builder", || async { "x" })
        .hidden()
        .nest("/admin", admin)
        .nest("/v2", Routes::new().mount(get_user))
}

fn api() -> OpenApi {
    OpenApi::new("Test API", "1.2.3")
        .description("A test API.")
        .server("https://api.example.com")
        .server_with("https://staging.example.com", "Staging")
        .contact("Ada", "ada@example.com")
        .license("MIT")
        .tag("users", "User operations")
        .bearer_auth("bearer")
        .api_key_auth("api_key", "x-api-key")
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

fn param<'a>(op: &'a Value, name: &str) -> &'a Value {
    op["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("no parameters in {op:#}"))
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("no parameter {name} in {op:#}"))
}

fn schema_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
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
// Tests

#[test]
fn info_servers_tags_and_security_schemes() {
    let spec = spec_for(app(), api());
    assert_eq!(spec["openapi"], "3.1.0");
    assert_eq!(
        spec["info"],
        json!({
            "title": "Test API",
            "version": "1.2.3",
            "description": "A test API.",
            "contact": { "name": "Ada", "email": "ada@example.com" },
            "license": { "name": "MIT" }
        })
    );
    assert_eq!(
        spec["servers"],
        json!([
            { "url": "https://api.example.com" },
            { "url": "https://staging.example.com", "description": "Staging" }
        ])
    );
    assert_eq!(
        spec["tags"],
        json!([{ "name": "users", "description": "User operations" }])
    );
    assert_eq!(
        spec["components"]["securitySchemes"],
        json!({
            "bearer": { "type": "http", "scheme": "bearer", "bearerFormat": "JWT" },
            "api_key": { "type": "apiKey", "in": "header", "name": "x-api-key" }
        })
    );
    assert!(spec.get("security").is_none());
}

#[test]
fn every_visible_route_is_documented() {
    let spec = spec_for(app(), api());
    let paths: HashSet<&str> = spec["paths"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let expected: HashSet<&str> = [
        "/users/{id}",
        "/users",
        "/orgs/{org}/users/{id}",
        "/pair/{a}/{b}",
        "/multi",
        "/go",
        "/files/{rest}",
        "/raw",
        "/opt",
        "/plain",
        "/secret",
        "/health",
        "/legacy",
        "/admin/stats",
        "/admin/reindex",
        "/v2/users/{id}",
    ]
    .into();
    assert_eq!(paths, expected);
    // Hidden routes (macro and builder) are excluded.
    assert!(spec["paths"].get("/internal").is_none());
    assert!(spec["paths"].get("/hidden-builder").is_none());
}

#[test]
fn operation_ids_are_unique() {
    let spec = spec_for(app(), api());
    let mut ids = Vec::new();
    for item in spec["paths"].as_object().unwrap().values() {
        for op in item.as_object().unwrap().values() {
            ids.push(op["operationId"].as_str().unwrap().to_owned());
        }
    }
    let unique: HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "{ids:?}");

    assert_eq!(op(&spec, "/users/{id}", "get")["operationId"], "get_user");
    // The same handler mounted twice gets a distinct id.
    assert_eq!(
        op(&spec, "/v2/users/{id}", "get")["operationId"],
        "get_user_get"
    );
    assert_eq!(op(&spec, "/multi", "get")["operationId"], "multi");
    assert_eq!(op(&spec, "/multi", "put")["operationId"], "multi_put");
    assert_eq!(op(&spec, "/health", "get")["operationId"], "get_health");
    assert_eq!(
        op(&spec, "/admin/stats", "get")["operationId"],
        "get_admin_stats"
    );
    assert_eq!(op(&spec, "/legacy", "get")["operationId"], "legacyEndpoint");
    assert_eq!(
        op(&spec, "/orgs/{org}/users/{id}", "put")["operationId"],
        "replaceOrgUser"
    );
}

#[test]
fn doc_comments_become_summary_and_description() {
    let spec = spec_for(app(), api());
    let get = op(&spec, "/users/{id}", "get");
    assert_eq!(get["summary"], "Get a user.");
    assert_eq!(
        get["description"],
        "Looks the user up by id.\nReturns 404 when missing."
    );
    assert_eq!(get["tags"], json!(["users"]));
    // `summary = ..` overrides; builder docs on plain routes.
    let put = op(&spec, "/orgs/{org}/users/{id}", "put");
    assert_eq!(put["summary"], "Replace a profile");
    assert_eq!(put["deprecated"], true);
    assert_eq!(put["tags"], json!(["orgs", "users"]));
    let health = op(&spec, "/health", "get");
    assert_eq!(health["summary"], "Health check");
    assert_eq!(health["tags"], json!(["ops"]));
    let legacy = op(&spec, "/legacy", "get");
    assert_eq!(legacy["deprecated"], true);
    assert_eq!(legacy["description"], "Use /health instead.");
    assert!(op(&spec, "/pair/{a}/{b}", "get").get("summary").is_none());
}

#[test]
fn path_parameters_are_typed() {
    let spec = spec_for(app(), api());
    let id = param(op(&spec, "/users/{id}", "get"), "id");
    assert_eq!(id["in"], "path");
    assert_eq!(id["required"], true);
    assert_eq!(id["schema"]["type"], "integer");
    assert_eq!(id["schema"]["format"], "uint64");

    // Struct fields by name.
    let put = op(&spec, "/orgs/{org}/users/{id}", "put");
    assert_eq!(param(put, "org")["schema"], json!({ "type": "string" }));
    assert_eq!(param(put, "id")["schema"]["type"], "integer");

    // Tuples by position.
    let pair_op = op(&spec, "/pair/{a}/{b}", "get");
    assert_eq!(param(pair_op, "a")["schema"], json!({ "type": "string" }));
    assert_eq!(param(pair_op, "b")["schema"]["format"], "uint32");

    // Wildcards, and untyped params default to strings.
    let rest = param(op(&spec, "/files/{rest}", "get"), "rest");
    assert_eq!(rest["schema"], json!({ "type": "string" }));
    assert_eq!(rest["required"], true);
    // Clients keep the slashes of a catch-all.
    assert_eq!(rest["x-zapline-catch-all"], true);

    // Params from the nest prefix are documented too.
    let v2 = op(&spec, "/v2/users/{id}", "get");
    assert_eq!(v2["parameters"].as_array().unwrap().len(), 1);
}

#[test]
fn query_parameters_from_struct_properties() {
    let spec = spec_for(app(), api());
    let list = op(&spec, "/users", "get");
    let page = param(list, "page");
    assert_eq!(page["in"], "query");
    assert_eq!(page["required"], true);
    assert_eq!(page["description"], "Page number, starting at 1.");
    assert_eq!(page["schema"]["type"], "integer");
    assert!(page["schema"].get("description").is_none());
    let per_page = param(list, "per_page");
    assert_eq!(per_page["required"], false);
    // `Option<u32>` is not nullable as a query parameter.
    assert_eq!(per_page["schema"]["type"], "integer");

    // `Option<Query<T>>` makes every parameter optional.
    let opt_op = op(&spec, "/opt", "get");
    assert_eq!(param(opt_op, "page")["required"], false);
    // Query structs are inlined, not components.
    assert!(spec["components"]["schemas"].get("Pagination").is_none());
    assert!(spec["components"]["schemas"].get("OrgUser").is_none());
}

#[test]
fn request_bodies() {
    let spec = spec_for(app(), api());
    let create = op(&spec, "/users", "post");
    assert_eq!(
        create["requestBody"],
        json!({
            "required": true,
            "content": { "application/json": { "schema": schema_ref("NewUser") } }
        })
    );
    let raw_op = op(&spec, "/raw", "post");
    assert_eq!(
        raw_op["requestBody"]["content"],
        json!({ "application/octet-stream": {} })
    );
    let reindex = op(&spec, "/admin/reindex", "post");
    assert_eq!(
        reindex["requestBody"]["content"]["application/json"]["schema"],
        schema_ref("Reindex")
    );
    // A type without `JsonSchema` is skipped, but the route is still there.
    let plain_op = op(&spec, "/plain", "post");
    assert!(plain_op.get("requestBody").is_none());
    assert_eq!(
        plain_op["responses"],
        json!({ "200": { "description": "OK" } })
    );
}

#[test]
fn responses_from_return_types() {
    let spec = spec_for(app(), api());
    let error = json!({ "application/json": { "schema": schema_ref("Error") } });

    // Option<Json<T>>: 200 + 404.
    let get = &op(&spec, "/users/{id}", "get")["responses"];
    assert_eq!(
        get["200"],
        json!({
            "description": "OK",
            "content": { "application/json": { "schema": schema_ref("User") } }
        })
    );
    assert_eq!(get["404"]["description"], "Not Found");
    assert_eq!(get["404"]["content"], error);

    // Result<(StatusCode, Json<T>)> with status = 201, plus Valid's 422.
    let create = &op(&spec, "/users", "post")["responses"];
    assert!(create.get("200").is_none());
    assert_eq!(create["201"]["description"], "Created");
    assert_eq!(
        create["201"]["content"]["application/json"]["schema"],
        schema_ref("User")
    );
    assert_eq!(create["default"]["content"], error);
    assert_eq!(create["422"]["description"], "Validation Error");
    assert_eq!(create["422"]["content"], error);

    // Vec<T>.
    let list = &op(&spec, "/users", "get")["responses"]["200"];
    assert_eq!(
        list["content"]["application/json"]["schema"],
        json!({ "type": "array", "items": schema_ref("User") })
    );

    assert_eq!(
        op(&spec, "/multi", "put")["responses"]["200"]["content"],
        json!({ "text/html": { "schema": { "type": "string" } } })
    );
    assert_eq!(
        op(&spec, "/raw", "post")["responses"]["200"]["content"],
        json!({ "text/plain": { "schema": { "type": "string" } } })
    );
    assert_eq!(
        op(&spec, "/files/{rest}", "get")["responses"]["200"]["content"],
        json!({ "application/octet-stream": {} })
    );
    let go_op = &op(&spec, "/go", "get")["responses"];
    assert!(go_op.get("200").is_none());
    assert_eq!(go_op["303"]["description"], "See Other");
    assert!(go_op["303"]["headers"]["Location"].is_object());

    // `impl IntoResponse` cannot be inspected: a generic success response.
    assert_eq!(
        op(&spec, "/secret", "get")["responses"],
        json!({ "200": { "description": "OK" } })
    );

    // Builder responses.
    let stats = &op(&spec, "/admin/stats", "get")["responses"]["200"];
    assert_eq!(stats["description"], "Statistics");
    assert_eq!(
        stats["content"]["application/json"]["schema"],
        schema_ref("Stats")
    );
    let reindex = &op(&spec, "/admin/reindex", "post")["responses"];
    assert_eq!(reindex["202"], json!({ "description": "Reindex started" }));
    assert!(reindex.get("200").is_none());
}

#[test]
fn security_and_group_defaults() {
    let spec = spec_for(app(), api());
    assert_eq!(
        op(&spec, "/users", "post")["security"],
        json!([{ "bearer": [] }])
    );
    assert!(op(&spec, "/users", "get").get("security").is_none());

    // Custom extractor docs: header param + security.
    let secret_op = op(&spec, "/secret", "get");
    assert_eq!(secret_op["security"], json!([{ "api_key": [] }]));
    let key = param(secret_op, "x-api-key");
    assert_eq!(key["in"], "header");
    assert_eq!(key["required"], true);

    // Group defaults apply to every route in the group.
    for (path, method) in [("/admin/stats", "get"), ("/admin/reindex", "post")] {
        let o = op(&spec, path, method);
        assert_eq!(o["tags"], json!(["admin"]), "{path}");
        assert_eq!(o["security"], json!([{ "bearer": [] }]), "{path}");
    }
}

#[test]
fn component_schemas_are_shared_and_resolvable() {
    let spec = spec_for(app(), api());
    let schemas = spec["components"]["schemas"].as_object().unwrap();
    for name in [
        "Error", "User", "NewUser", "Address", "Stats", "Reindex", "Profile",
    ] {
        assert!(schemas.contains_key(name), "missing {name}: {schemas:#?}");
    }
    assert!(!schemas.contains_key("Plain"));

    // `Address` is used by requests and responses with the same schema:
    // one component, referenced from both.
    assert!(!schemas.contains_key("AddressInput"));
    assert_eq!(
        schemas["User"]["properties"]["address"],
        schema_ref("Address")
    );
    assert_eq!(
        schemas["NewUser"]["properties"]["address"],
        schema_ref("Address")
    );
    assert_eq!(schemas["Address"]["description"], "A postal address.");

    // `Profile` differs between contracts: the request side is `ProfileInput`.
    let put = op(&spec, "/orgs/{org}/users/{id}", "put");
    assert_eq!(
        put["requestBody"]["content"]["application/json"]["schema"],
        schema_ref("ProfileInput")
    );
    assert_eq!(
        put["responses"]["200"]["content"]["application/json"]["schema"],
        schema_ref("Profile")
    );
    assert_eq!(schemas["Profile"]["required"], json!(["bio"]));
    assert!(schemas["ProfileInput"].get("required").is_none());

    // The framework error body.
    assert_eq!(schemas["Error"]["required"], json!(["error"]));
    assert_eq!(schemas["Error"]["properties"]["error"]["type"], "string");
    assert!(schemas["Error"]["properties"]["code"].is_object());
    assert!(schemas["Error"]["properties"]["details"].is_object());

    // Every $ref points at an existing component, and no temporary
    // locations leak out.
    let mut refs = Vec::new();
    collect_refs(&spec, &mut refs);
    assert!(!refs.is_empty());
    for r in &refs {
        let name = r
            .strip_prefix("#/components/schemas/")
            .unwrap_or_else(|| panic!("unexpected ref {r}"));
        assert!(schemas.contains_key(name), "dangling ref {r}");
    }
    // (`x-zapline-catch-all` on `{*rest}` parameters is documented output.)
    assert!(!spec
        .to_string()
        .replace("\"x-zapline-catch-all\"", "")
        .contains("x-zapline"));
    assert!(spec["components"].get("x-zapline-input").is_none());
}

#[test]
fn user_type_named_error_does_not_clash() {
    mod user {
        #[derive(serde::Serialize, schemars::JsonSchema)]
        pub struct Error {
            pub reason: String,
        }
    }
    let app = App::new()
        .get("/e", || async { "x" })
        .response::<user::Error>(400, "Custom error")
        .get("/f", || async { Err::<&str, _>(Error::not_found()) })
        .response::<Error>(409, "Conflict");
    let spec = spec_for(app, OpenApi::new("t", "1"));
    let schemas = spec["components"]["schemas"].as_object().unwrap();
    assert_eq!(schemas["Error"]["required"], json!(["error"]));
    assert_eq!(schemas["Error2"]["required"], json!(["reason"]));
    assert_eq!(
        op(&spec, "/e", "get")["responses"]["400"]["content"]["application/json"]["schema"],
        schema_ref("Error2")
    );
    assert_eq!(
        op(&spec, "/f", "get")["responses"]["409"]["content"]["application/json"]["schema"],
        schema_ref("Error")
    );
}

#[test]
fn global_security_and_modify_hook() {
    let spec = spec_for(
        App::new().get("/a", || async { "a" }),
        OpenApi::new("t", "1")
            .bearer_auth("bearer")
            .security("bearer")
            .modify(|spec| {
                spec["x-custom"] = json!(true);
            }),
    );
    assert_eq!(spec["security"], json!([{ "bearer": [] }]));
    assert_eq!(spec["x-custom"], true);
    assert!(op(&spec, "/a", "get").get("security").is_none());
}

#[test]
fn any_and_nonstandard_methods_are_skipped() {
    let app = App::new()
        .any("/proxy", || async { "x" })
        .route(Method::from_bytes(b"PROPFIND").unwrap(), "/dav", || async {
            "x"
        })
        .get("/ok", || async { "x" });
    let spec = spec_for(app, OpenApi::new("t", "1"));
    let paths = spec["paths"].as_object().unwrap();
    assert_eq!(paths.len(), 1);
    assert!(paths.contains_key("/ok"));
}

#[tokio::test]
async fn serves_spec_and_swagger_ui() {
    let expected = spec_for(app(), api());
    let c = app().openapi(api()).test();

    let r = c.get("/openapi.json").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert!(r
        .text()
        .starts_with(r#"{"openapi":"3.1.0","info":{"title":"Test API""#));
    assert_eq!(r.json::<Value>(), expected);

    let r = c.get("/docs").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
    let html = r.text();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("SwaggerUIBundle"));
    assert!(html.contains(r#"url: "/openapi.json""#));
    assert!(html.contains("swagger-ui-dist@5.33.1/swagger-ui-bundle.js"));
    assert!(html.contains(r#"integrity="sha384-"#));
    assert!(html.contains("<title>Test API - Swagger UI</title>"));

    // The plugin's own routes are not part of the spec.
    let served = c.get("/openapi.json").send().await.json::<Value>();
    assert!(served["paths"].get("/openapi.json").is_none());
    assert!(served["paths"].get("/docs").is_none());
}

#[tokio::test]
async fn multiple_uis_custom_paths_and_spec_url() {
    let c = App::new()
        .get("/a", || async { "a" })
        .openapi(
            OpenApi::new("Multi", "1")
                .json_path("api/spec.json/")
                .docs(DocsUi::Scalar, "/reference")
                .docs(DocsUi::Redoc, "/redoc")
                .docs(DocsUi::Swagger, "/swagger"),
        )
        .test();

    assert_eq!(c.get("/api/spec.json").send().await.status(), 200);
    assert_eq!(c.get("/openapi.json").send().await.status(), 404);
    // The first `.docs(..)` replaces the default Swagger UI at /docs.
    assert_eq!(c.get("/docs").send().await.status(), 404);

    let scalar = c.get("/reference").send().await;
    assert_eq!(
        scalar.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    let html = scalar.text();
    assert!(html.contains("Scalar.createApiReference"));
    assert!(html.contains("@scalar/api-reference@1.72.3"));
    assert!(html.contains(r#"url: "/api/spec.json""#));

    let redoc = c.get("/redoc").send().await.text();
    assert!(redoc.contains(r#"<redoc spec-url="/api/spec.json"></redoc>"#));
    assert!(redoc.contains("redoc@2.5.4"));

    assert!(c
        .get("/swagger")
        .send()
        .await
        .text()
        .contains("SwaggerUIBundle"));

    let c = App::new()
        .get("/a", || async { "a" })
        .openapi(
            OpenApi::new("p", "1")
                .spec_url("/prefix/openapi.json")
                .cdn("https://npm.internal.example/"),
        )
        .test();
    let html = c.get("/docs").send().await.text();
    assert!(html.contains(r#"url: "/prefix/openapi.json""#));
    assert!(html.contains(
        r#"src="https://npm.internal.example/swagger-ui-dist@5.33.1/swagger-ui-bundle.js""#
    ));
    assert!(!html.contains("jsdelivr"));
}

#[tokio::test]
async fn no_docs_serves_only_json() {
    let c = App::new()
        .get("/a", || async { "a" })
        .plugin(OpenApi::new("t", "1").no_docs())
        .test();
    assert_eq!(c.get("/openapi.json").send().await.status(), 200);
    assert_eq!(c.get("/docs").send().await.status(), 404);
    // HEAD is served by the GET route.
    let r = c.request(Method::HEAD, "/openapi.json").send().await;
    assert_eq!(r.status(), 200);
}

#[test]
fn export_spec_writes_pretty_json() {
    let spec = spec_for(app(), api());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("openapi.json");
    export_spec(&spec, &path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("{\n  \"openapi\": \"3.1.0\",\n  \"info\": {"),
        "{}",
        &text[..40]
    );
    assert!(text.ends_with("}\n"));
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), spec);

    let missing = dir.path().join("no/such/dir/spec.json");
    assert!(export_spec(&spec, missing).is_err());
}

/// Runs only in the child process spawned by `export_env_var_exits`.
#[test]
fn export_env_var_child() {
    if std::env::var_os(ENV_EXPORT_OPENAPI).is_none() {
        return;
    }
    let _ = app().openapi(api()).route_table();
    unreachable!("the OpenAPI build hook should have exited the process");
}

#[test]
fn export_env_var_exits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spec.json");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["export_env_var_child", "--exact", "--test-threads=1"])
        .env(ENV_EXPORT_OPENAPI, &path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    // The process exited inside the hook, before the harness could report.
    assert!(!String::from_utf8_lossy(&out.stdout).contains("test result"));
    let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(written, spec_for(app(), api()));
}

#[test]
fn generate_from_route_table() {
    let table = App::new().mount(get_user).route_table();
    let spec = OpenApi::new("t", "1").generate(&table);
    assert_eq!(op(&spec, "/users/{id}", "get")["operationId"], "get_user");
}

#[tokio::test]
async fn documented_handlers_still_work() {
    let c = app().test();
    assert_eq!(c.get("/users/1").send().await.status(), 200);
    assert_eq!(c.get("/users/2").send().await.status(), 404);
    assert_eq!(c.get("/pair/x/5").send().await.text(), "x5");
    assert_eq!(c.get("/opt").send().await.text(), "none");
    assert_eq!(c.get("/opt?page=1").send().await.text(), "some");
    assert_eq!(c.get("/go").send().await.status(), 303);
    assert_eq!(c.get("/secret").send().await.status(), 401);
    assert_eq!(
        c.get("/secret")
            .header("x-api-key", "k")
            .send()
            .await
            .text(),
        "7"
    );
    assert_eq!(c.delete("/internal").send().await.status(), 204);
    let r = c
        .post("/users")
        .json(&json!({ "name": "bob", "address": { "street": "s", "zip": "123" } }))
        .send()
        .await;
    assert_eq!(r.status(), 422);
    assert_eq!(r.json::<Value>()["details"][0]["field"], "address.zip");
}

#[test]
fn differing_schemas_propagate_to_containers() {
    /// Identical in both directions by itself, but it contains `Profile`,
    /// which differs.
    #[derive(Serialize, Deserialize, JsonSchema)]
    struct Wrapper {
        profile: Profile,
        tags: Vec<String>,
    }
    #[post("/wrap")]
    async fn wrap(Json(w): Json<Wrapper>) -> Json<Wrapper> {
        Json(w)
    }
    let spec = spec_for(App::new().mount(wrap), OpenApi::new("t", "1"));
    let schemas = spec["components"]["schemas"].as_object().unwrap();
    let o = op(&spec, "/wrap", "post");
    assert_eq!(
        o["requestBody"]["content"]["application/json"]["schema"],
        schema_ref("WrapperInput")
    );
    assert_eq!(
        o["responses"]["200"]["content"]["application/json"]["schema"],
        schema_ref("Wrapper")
    );
    assert_eq!(
        schemas["WrapperInput"]["properties"]["profile"],
        schema_ref("ProfileInput")
    );
    assert_eq!(
        schemas["Wrapper"]["properties"]["profile"],
        schema_ref("Profile")
    );
}

#[tokio::test]
async fn served_over_a_real_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app().openapi(api()).serve(listener));

    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(
        b"GET /openapi.json HTTP/1.1\r\nHost: x\r\n\r\nGET /docs HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK"), "{}", &out[..200]);
    assert!(out.contains("content-type: application/json"));
    assert!(out.contains(r#"{"openapi":"3.1.0","info":{"title":"Test API""#));
    assert!(out.contains("content-type: text/html; charset=utf-8"));
    assert!(out.contains("SwaggerUIBundle"));
}

#[test]
fn nested_group_docs_accumulate() {
    let inner = Routes::new()
        .get("/x", || async { "x" })
        .tag("route")
        .tag_all("inner")
        .security_all("api_key");
    let outer = Routes::new()
        .nest("/in", inner)
        .get("/y", || async { "y" })
        .get("/z", || async { "z" })
        .security("bearer")
        .tag_all("outer")
        .tag_all("inner")
        .security_all("bearer");
    let app = App::new()
        .nest("/out", outer)
        .nest("/other", Routes::new().get("/w", || async { "w" }));
    let spec = spec_for(app, api());

    // Inner group first, then outer groups (deduplicated), then the route's.
    let x = op(&spec, "/out/in/x", "get");
    assert_eq!(x["tags"], json!(["inner", "outer", "route"]));
    assert_eq!(x["security"], json!([{ "api_key": [] }, { "bearer": [] }]));
    let y = op(&spec, "/out/y", "get");
    assert_eq!(y["tags"], json!(["outer", "inner"]));
    assert_eq!(y["security"], json!([{ "bearer": [] }]));
    // Route-level security still replaces the group's.
    assert_eq!(
        op(&spec, "/out/z", "get")["security"],
        json!([{ "bearer": [] }])
    );
    // Groups don't leak into siblings.
    let w = op(&spec, "/other/w", "get");
    assert!(w.get("tags").is_none() && w.get("security").is_none());
}
