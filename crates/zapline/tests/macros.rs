//! Route macros: arguments, doc comments and type-driven docs.
//!
//! Also compiled without the `openapi` feature (spec assertions are gated) to
//! check that expansions don't depend on it:
//! `cargo test -p zapline --no-default-features --features macros,json,query,form --test macros`.

#![allow(clippy::unused_async)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zapline::extract::MatchedPath;
use zapline::header::{HeaderName, HeaderValue};
use zapline::prelude::*;

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
struct Thing {
    id: u32,
}

/// No `JsonSchema`.
#[derive(Serialize, Deserialize)]
struct Opaque {
    x: u32,
}

type ApiResult<T> = std::result::Result<T, Error>;

/// Ignored: `summary = ..` wins.
///
/// The description comes from the doc comment.
#[get(
    "/all",
    tag = "a",
    tags("b", "c"),
    tags = ["d", "a"],
    summary = "S",
    operation_id = "allArgs",
    status = 202,
    deprecated,
    hidden = false,
    security = "bearer",
    security("k1", "k2"),
)]
async fn all_args() -> StatusCode {
    StatusCode::ACCEPTED
}

/**
 * Block doc comment.
 *
 * Second paragraph,
 * on two lines.
 */
#[route("/r", method = "get", method = "DELETE", description = "Explicit.")]
async fn multi_lower(m: Method) -> &'static str {
    if m == Method::GET {
        "get"
    } else {
        "delete"
    }
}

#[get("/unit")]
async fn unit() {}

#[get("/result")]
async fn result_json() -> Result<Json<Thing>, Error> {
    Ok(Json(Thing { id: 1 }))
}

#[post("/headers", status = 201)]
async fn with_headers() -> (StatusCode, [(HeaderName, HeaderValue); 1], Json<Thing>) {
    (
        StatusCode::CREATED,
        [(
            HeaderName::from_static("x-id"),
            HeaderValue::from_static("1"),
        )],
        Json(Thing { id: 1 }),
    )
}

#[get("/maybe/{id}")]
async fn maybe(Path(id): Path<u32>) -> Option<Result<Json<Thing>>> {
    match id {
        0 => None,
        1 => Some(Ok(Json(Thing { id }))),
        _ => Some(Err(Error::conflict("nope"))),
    }
}

#[get("/raw-response")]
async fn raw_response() -> Response {
    StatusCode::IM_A_TEAPOT.into_response()
}

#[get("/html")]
async fn html() -> Html<&'static str> {
    Html("<p>hi</p>")
}

#[get("/qualified")]
async fn qualified() -> zapline::Result<String> {
    Ok("q".into())
}

#[get("/alias")]
async fn alias() -> ApiResult<Json<Thing>> {
    Ok(Json(Thing { id: 2 }))
}

#[get("/paren")]
#[allow(unused_parens)]
async fn paren() -> ((StatusCode, String)) {
    (StatusCode::OK, "p".into())
}

#[derive(Clone)]
struct AppState;

#[post("/noops")]
#[allow(clippy::too_many_arguments)]
async fn noops(
    State(_): State<AppState>,
    _h: HeaderMap,
    _m: Method,
    _u: Uri,
    _c: ConnInfo,
    _p: MatchedPath,
    _req: Request,
) -> &'static str {
    "ok"
}

#[post("/opaque")]
async fn opaque(Json(o): Json<Opaque>) -> Json<Opaque> {
    Json(o)
}

#[post("/optional-query")]
async fn optional_query(q: Option<Query<Thing>>, Json(body): Json<Thing>) -> String {
    format!("{}:{}", q.map_or(0, |Query(t)| t.id), body.id)
}

#[post("/form")]
async fn form(Form(t): Form<Thing>) -> String {
    t.id.to_string()
}

#[post("/text")]
async fn text(body: String) -> String {
    body
}

#[get("/type")]
async fn r#type() -> &'static str {
    "raw ident"
}

mod nested {
    use super::*;

    /// Visible from the parent module.
    #[get("/nested")]
    pub async fn nested_route() -> &'static str {
        "nested"
    }
}

fn app() -> App {
    App::new()
        .state(AppState)
        .mount((
            all_args,
            multi_lower,
            unit,
            result_json,
            with_headers,
            maybe,
            raw_response,
            html,
            qualified,
            alias,
            paren,
            noops,
        ))
        .mount((
            opaque,
            optional_query,
            form,
            text,
            r#type,
            nested::nested_route,
        ))
}

#[tokio::test]
async fn handlers_behave_as_before() {
    let c = app().test();
    assert_eq!(c.get("/all").send().await.status(), 202);
    assert_eq!(c.get("/r").send().await.text(), "get");
    assert_eq!(c.delete("/r").send().await.text(), "delete");
    assert_eq!(c.get("/unit").send().await.status(), 200);
    assert_eq!(c.get("/result").send().await.text(), r#"{"id":1}"#);
    let r = c.post("/headers").send().await;
    assert_eq!(r.status(), 201);
    assert_eq!(r.header("x-id"), Some("1"));
    assert_eq!(c.get("/maybe/0").send().await.status(), 404);
    assert_eq!(c.get("/maybe/1").send().await.status(), 200);
    assert_eq!(c.get("/maybe/2").send().await.status(), 409);
    assert_eq!(c.get("/raw-response").send().await.status(), 418);
    assert_eq!(c.get("/html").send().await.text(), "<p>hi</p>");
    assert_eq!(c.get("/qualified").send().await.text(), "q");
    assert_eq!(c.get("/alias").send().await.text(), r#"{"id":2}"#);
    assert_eq!(c.get("/paren").send().await.text(), "p");
    assert_eq!(c.post("/noops").send().await.text(), "ok");
    assert_eq!(
        c.post("/opaque").json(&Opaque { x: 3 }).send().await.text(),
        r#"{"x":3}"#
    );
    assert_eq!(
        c.post("/optional-query")
            .json(&Thing { id: 9 })
            .send()
            .await
            .text(),
        "0:9"
    );
    assert_eq!(
        c.post("/optional-query?id=5")
            .json(&Thing { id: 9 })
            .send()
            .await
            .text(),
        "5:9"
    );
    assert_eq!(
        c.post("/form").form(&Thing { id: 4 }).send().await.text(),
        "4"
    );
    assert_eq!(c.post("/text").body("hey").send().await.text(), "hey");
    assert_eq!(c.get("/type").send().await.text(), "raw ident");
    assert_eq!(c.get("/nested").send().await.text(), "nested");
}

#[test]
fn route_values_are_zero_sized_and_copy() {
    fn assert_copy<T: Copy>(_: T) {}
    assert_eq!(std::mem::size_of_val(&all_args), 0);
    assert_copy(multi_lower);
    assert_eq!(format!("{:?}", unit), "unit");
}

#[cfg(feature = "openapi")]
mod spec {
    use super::*;
    use serde_json::{json, Value};
    use zapline::openapi::{spec_for, OpenApi};

    fn spec() -> Value {
        spec_for(app(), OpenApi::new("macros", "1"))
    }

    fn op<'a>(spec: &'a Value, path: &str, method: &str) -> &'a Value {
        let o = &spec["paths"][path][method];
        assert!(o.is_object(), "missing {method} {path}");
        o
    }

    fn json_ref(name: &str) -> Value {
        json!({ "application/json": { "schema": { "$ref": format!("#/components/schemas/{name}") } } })
    }

    #[test]
    fn all_arguments() {
        let spec = spec();
        let o = op(&spec, "/all", "get");
        assert_eq!(o["tags"], json!(["a", "b", "c", "d"]));
        assert_eq!(o["summary"], "S");
        assert_eq!(
            o["description"],
            "The description comes from the doc comment."
        );
        assert_eq!(o["operationId"], "allArgs");
        assert_eq!(o["deprecated"], true);
        assert_eq!(
            o["security"],
            json!([{ "bearer": [] }, { "k1": [] }, { "k2": [] }])
        );
        assert_eq!(
            o["responses"],
            json!({ "202": { "description": "Accepted" } })
        );
    }

    #[test]
    fn block_doc_comments_and_explicit_description() {
        let spec = spec();
        for method in ["get", "delete"] {
            let o = op(&spec, "/r", method);
            assert_eq!(o["summary"], "Block doc comment.");
            assert_eq!(o["description"], "Explicit.");
        }
        assert_eq!(op(&spec, "/r", "get")["operationId"], "multi_lower");
        assert_eq!(
            op(&spec, "/r", "delete")["operationId"],
            "multi_lower_delete"
        );
        assert_eq!(
            op(&spec, "/nested", "get")["summary"],
            "Visible from the parent module."
        );
        assert_eq!(op(&spec, "/type", "get")["operationId"], "type");
    }

    #[test]
    fn return_types() {
        let spec = spec();
        let error = json_ref("Error");
        let r = |path: &str, method: &str| op(&spec, path, method)["responses"].clone();

        assert_eq!(r("/unit", "get"), json!({ "200": { "description": "OK" } }));

        let res = r("/result", "get");
        assert_eq!(res["200"]["content"], json_ref("Thing"));
        assert_eq!(res["default"]["content"], error);

        let headers = r("/headers", "post");
        assert_eq!(headers["201"]["content"], json_ref("Thing"));
        assert!(headers.get("200").is_none());

        let maybe_r = r("/maybe/{id}", "get");
        assert_eq!(maybe_r["200"]["content"], json_ref("Thing"));
        assert_eq!(maybe_r["404"]["content"], error);
        assert_eq!(maybe_r["default"]["content"], error);

        assert_eq!(
            r("/raw-response", "get"),
            json!({ "200": { "description": "OK" } })
        );
        assert_eq!(
            r("/html", "get")["200"]["content"],
            json!({ "text/html": { "schema": { "type": "string" } } })
        );
        let q = r("/qualified", "get");
        assert_eq!(
            q["200"]["content"],
            json!({ "text/plain": { "schema": { "type": "string" } } })
        );
        assert_eq!(q["default"]["content"], error);

        // A type alias goes through the trait impls instead of the macro.
        let alias_r = r("/alias", "get");
        assert_eq!(alias_r["200"]["content"], json_ref("Thing"));
        assert_eq!(alias_r["default"]["content"], error);

        assert_eq!(
            r("/paren", "get")["200"]["content"],
            json!({ "text/plain": { "schema": { "type": "string" } } })
        );
    }

    #[test]
    fn inputs() {
        let spec = spec();
        let noops_op = op(&spec, "/noops", "post");
        assert!(noops_op.get("parameters").is_none(), "{noops_op:#}");
        assert!(noops_op.get("requestBody").is_none(), "{noops_op:#}");

        let opaque_op = op(&spec, "/opaque", "post");
        assert!(opaque_op.get("requestBody").is_none());
        assert_eq!(
            opaque_op["responses"],
            json!({ "200": { "description": "OK" } })
        );

        let optional = op(&spec, "/optional-query", "post");
        assert_eq!(optional["parameters"][0]["name"], "id");
        assert_eq!(optional["parameters"][0]["required"], false);
        assert_eq!(optional["requestBody"]["required"], true);
        assert_eq!(optional["requestBody"]["content"], json_ref("Thing"));

        assert_eq!(
            op(&spec, "/form", "post")["requestBody"]["content"],
            json!({ "application/x-www-form-urlencoded": {
                "schema": { "$ref": "#/components/schemas/Thing" }
            } })
        );
        assert_eq!(
            op(&spec, "/text", "post")["requestBody"]["content"],
            json!({ "text/plain": { "schema": { "type": "string" } } })
        );
    }
}
