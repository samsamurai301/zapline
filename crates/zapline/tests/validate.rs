//! `Valid<T>`: validation of JSON, form, query and path input.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use validator::{Validate, ValidationError};
use zapline::prelude::*;
use zapline::validate::{field_errors, validation_error, FieldError, Valid};

#[derive(Debug, Serialize, Deserialize, Validate)]
struct Address {
    #[validate(length(min = 1))]
    street: String,
    #[validate(length(equal = 5, message = "zip must have 5 digits"))]
    zip: String,
}

#[derive(Debug, Serialize, Deserialize, Validate)]
struct Item {
    #[validate(length(min = 1, max = 20))]
    name: String,
    #[validate(range(min = 1, max = 99))]
    qty: u32,
}

#[derive(Debug, Serialize, Deserialize, Validate)]
struct Order {
    #[validate(email)]
    email: String,
    #[validate(nested)]
    address: Address,
    #[validate(length(min = 1), nested)]
    items: Vec<Item>,
}

fn order() -> Value {
    json!({
        "email": "ada@example.com",
        "address": { "street": "Main St", "zip": "12345" },
        "items": [{ "name": "apple", "qty": 3 }]
    })
}

async fn create(Valid(Json(o)): Valid<Json<Order>>) -> Json<Order> {
    Json(o)
}

fn details(r: &zapline::test::TestResponse) -> Vec<Value> {
    let body: Value = r.json();
    assert_eq!(body["error"], "validation failed", "{body}");
    assert_eq!(body["code"], "validation_error", "{body}");
    body["details"].as_array().cloned().unwrap()
}

#[tokio::test]
async fn valid_json_passes_through() {
    let c = App::new().post("/orders", create).test();
    let r = c.post("/orders").json(&order()).send().await;
    assert_eq!(r.status(), 200, "{}", r.text());
    assert_eq!(r.json::<Value>(), order());
}

#[tokio::test]
async fn nested_and_list_errors_are_flattened() {
    let c = App::new().post("/orders", create).test();
    let bad = json!({
        "email": "not-an-email",
        "address": { "street": "", "zip": "12" },
        "items": [
            { "name": "ok", "qty": 1 },
            { "name": "", "qty": 500 }
        ]
    });
    let r = c.post("/orders").json(&bad).send().await;
    assert_eq!(r.status(), 422);
    assert_eq!(r.header("content-type"), Some("application/json"));
    let d = details(&r);
    let fields: Vec<&str> = d.iter().map(|e| e["field"].as_str().unwrap()).collect();
    assert_eq!(
        fields,
        [
            "address.street",
            "address.zip",
            "email",
            "items[1].name",
            "items[1].qty"
        ]
    );

    let zip = &d[1];
    assert_eq!(zip["code"], "length");
    assert_eq!(zip["message"], "zip must have 5 digits");
    assert_eq!(zip["params"], json!({ "equal": 5 }));

    let email = &d[2];
    assert_eq!(
        email,
        &json!({
            "field": "email",
            "code": "email",
            "message": "must be a valid email address",
            "params": {}
        })
    );

    let qty = &d[4];
    assert_eq!(qty["code"], "range");
    assert_eq!(qty["message"], "must be at least 1 and at most 99");
    assert_eq!(qty["params"], json!({ "min": 1, "max": 99 }));

    assert_eq!(d[3]["message"], "length must be between 1 and 20");
    assert_eq!(d[0]["message"], "length must be at least 1");
}

#[tokio::test]
async fn rejected_values_are_never_echoed() {
    #[derive(Deserialize, Validate)]
    struct SignUp {
        #[validate(length(min = 12))]
        password: String,
        #[validate(must_match(other = "password"))]
        confirm: String,
    }
    async fn signup(Valid(Json(s)): Valid<Json<SignUp>>) -> String {
        s.password.len().to_string()
    }
    let c = App::new().post("/signup", signup).test();
    let r = c
        .post("/signup")
        .json(&json!({ "password": "hunter2", "confirm": "hunter3" }))
        .send()
        .await;
    assert_eq!(r.status(), 422);
    let text = r.text();
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("hunter3"), "{text}");
    let d = details(&r);
    assert_eq!(d[0]["field"], "confirm");
    assert_eq!(d[0]["code"], "must_match");
    assert_eq!(d[0]["message"], "must match the other field");
    assert_eq!(d[1]["field"], "password");
    assert_eq!(d[1]["params"], json!({ "min": 12 }));
}

#[tokio::test]
async fn inner_extractor_errors_pass_through() {
    let c = App::new().post("/orders", create).test();
    // Wrong content type: the Json extractor's 415.
    let r = c.post("/orders").body("{}").send().await;
    assert_eq!(r.status(), 415);
    // Malformed JSON / missing fields: the Json extractor's 422, without
    // validation details.
    let r = c
        .post("/orders")
        .header("content-type", "application/json")
        .body("{\"email\":")
        .send()
        .await;
    assert_eq!(r.status(), 422);
    let body: Value = r.json();
    assert!(body.get("code").is_none(), "{body}");
    let r = c
        .post("/orders")
        .json(&json!({ "email": "a@b.co" }))
        .send()
        .await;
    assert_eq!(r.status(), 422);
}

#[derive(Debug, Deserialize, Validate)]
struct Page {
    #[validate(range(min = 1, max = 100))]
    size: u32,
    #[validate(length(max = 10))]
    q: Option<String>,
}

#[tokio::test]
async fn valid_query_in_any_position() {
    // Parts extractor in a non-last position, followed by a body.
    async fn first(Valid(Query(p)): Valid<Query<Page>>, body: String) -> String {
        format!("{}:{}", p.size, body)
    }
    // ... and as the last argument.
    async fn last(m: Method, page: Valid<Query<Page>>) -> String {
        // Deref through `Valid` and `Query` to the value.
        format!("{m}:{}:{}", page.size, page.q.as_deref().unwrap_or("-"))
    }
    let c = App::new().post("/first", first).get("/last", last).test();
    assert_eq!(
        c.post("/first?size=5").body("hi").send().await.text(),
        "5:hi"
    );
    assert_eq!(c.get("/last?size=7&q=x").send().await.text(), "GET:7:x");

    let r = c.get("/last?size=0&q=waytoolongquery").send().await;
    assert_eq!(r.status(), 422);
    let d = details(&r);
    assert_eq!(d.len(), 2);
    assert_eq!(d[0]["field"], "q");
    assert_eq!(d[0]["message"], "length must be at most 10");
    assert_eq!(d[1]["field"], "size");
    // Deserialization errors stay 400s from the Query extractor.
    assert_eq!(c.get("/last?size=abc").send().await.status(), 400);
}

#[tokio::test]
async fn valid_form_and_path() {
    #[derive(Deserialize, Validate)]
    struct Login {
        #[validate(email)]
        email: String,
    }
    #[derive(Deserialize, Validate)]
    struct Ids {
        #[validate(range(min = 1))]
        id: u64,
    }
    async fn login(Valid(Form(l)): Valid<Form<Login>>) -> String {
        l.email
    }
    async fn by_id(Valid(Path(ids)): Valid<Path<Ids>>) -> String {
        ids.id.to_string()
    }
    let c = App::new()
        .post("/login", login)
        .get("/things/{id}", by_id)
        .test();

    let ok = c
        .post("/login")
        .form(&[("email", "ada@example.com")])
        .send()
        .await;
    assert_eq!(ok.text(), "ada@example.com");
    let bad = c.post("/login").form(&[("email", "nope")]).send().await;
    assert_eq!(bad.status(), 422);
    assert_eq!(details(&bad)[0]["field"], "email");

    assert_eq!(c.get("/things/3").send().await.text(), "3");
    let r = c.get("/things/0").send().await;
    assert_eq!(r.status(), 422);
    assert_eq!(details(&r)[0]["field"], "id");
}

fn check_total(o: &Totals) -> Result<(), ValidationError> {
    if o.a + o.b > 10 {
        return Err(
            ValidationError::new("total").with_message(Cow::Borrowed("a + b must be <= 10"))
        );
    }
    Ok(())
}

#[derive(Deserialize, Validate)]
#[validate(schema(function = "check_total"))]
struct Totals {
    a: u32,
    b: u32,
}

#[tokio::test]
async fn struct_level_errors_use_the_parent_path() {
    async fn h(Valid(Json(t)): Valid<Json<Totals>>) -> String {
        (t.a + t.b).to_string()
    }
    let c = App::new().post("/t", h).test();
    assert_eq!(
        c.post("/t")
            .json(&json!({"a": 1, "b": 2}))
            .send()
            .await
            .text(),
        "3"
    );
    let r = c.post("/t").json(&json!({"a": 9, "b": 2})).send().await;
    assert_eq!(r.status(), 422);
    assert_eq!(
        details(&r),
        [json!({ "field": "", "code": "total", "message": "a + b must be <= 10", "params": {} })]
    );
}

#[tokio::test]
async fn manual_validation_helper() {
    #[derive(Deserialize, Validate)]
    struct Q {
        #[validate(url)]
        link: String,
    }
    async fn h(Json(q): Json<Q>) -> Result<String> {
        q.validate().map_err(validation_error)?;
        Ok(q.link)
    }
    let c = App::new().post("/", h).test();
    let r = c.post("/").json(&json!({ "link": "nope" })).send().await;
    assert_eq!(r.status(), 422);
    assert_eq!(details(&r)[0]["message"], "must be a valid URL");
}

#[test]
fn field_errors_are_public_and_sorted() {
    let o = Order {
        email: "x".into(),
        address: Address {
            street: "s".into(),
            zip: "1".into(),
        },
        items: vec![],
    };
    let errs = o.validate().unwrap_err();
    let list = field_errors(&errs);
    let fields: Vec<&str> = list.iter().map(|e: &FieldError| e.field.as_str()).collect();
    assert_eq!(fields, ["address.zip", "email", "items"]);
    assert_eq!(list[2].code, "length");
}

#[tokio::test]
async fn works_with_macro_routes() {
    #[post("/orders")]
    async fn create_order(Valid(Json(o)): Valid<Json<Order>>) -> (StatusCode, Json<Order>) {
        (StatusCode::CREATED, Json(o))
    }
    let c = App::new().mount(create_order).test();
    assert_eq!(c.post("/orders").json(&order()).send().await.status(), 201);
    let mut bad = order();
    bad["items"] = json!([]);
    let r = c.post("/orders").json(&bad).send().await;
    assert_eq!(r.status(), 422);
    assert_eq!(details(&r)[0]["field"], "items");
}
