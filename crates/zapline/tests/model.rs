//! `#[model]`: serde, schemars and validator derives without the app
//! depending on those crates. (This test crate does depend on them; the
//! derives must still resolve through zapline's paths.)
#![cfg(all(feature = "macros", feature = "openapi", feature = "validate"))]

use serde_json::json;
use zapline::openapi::{spec_for, OpenApi};
use zapline::prelude::*;

/// A user.
#[model]
struct User {
    id: u64,
    #[validate(email)]
    email: String,
    #[serde(default, rename = "isAdmin")]
    admin: bool,
}

#[model]
#[serde(rename_all = "snake_case")]
enum Role {
    Admin,
    Member,
}

#[post("/users", status = 201)]
async fn create(Valid(Json(user)): Valid<Json<User>>) -> (StatusCode, Json<User>) {
    (StatusCode::CREATED, Json(user))
}

#[get("/roles")]
async fn roles() -> Json<Vec<Role>> {
    Json(vec![Role::Admin, Role::Member])
}

#[tokio::test]
async fn models_serialize_validate_and_document() {
    let client = App::new().mount((create, roles)).test();
    let res = client
        .post("/users")
        .json(&json!({"id": 1, "email": "a@b.co"}))
        .send()
        .await;
    res.assert_status(201);
    assert_eq!(
        res.json::<serde_json::Value>(),
        json!({"id": 1, "email": "a@b.co", "isAdmin": false})
    );
    client
        .post("/users")
        .json(&json!({"id": 1, "email": "nope"}))
        .send()
        .await
        .assert_status(422);
    client
        .get("/roles")
        .send()
        .await
        .assert_text(r#"["admin","member"]"#);

    let spec = spec_for(App::new().mount((create, roles)), OpenApi::new("t", "1"));
    let user = &spec["components"]["schemas"]["User"];
    assert_eq!(user["description"], "A user.");
    assert!(user["properties"]["isAdmin"].is_object(), "{user}");
    let cloned = User {
        id: 1,
        email: "x".into(),
        admin: false,
    }
    .clone();
    assert!(format!("{cloned:?}").contains("User"));
}
