//! Operation ids for same-named handlers in different modules.
#![cfg(all(feature = "macros", feature = "openapi"))]

use serde_json::Value;
use zapline::openapi::{spec_for, OpenApi};
use zapline::prelude::*;

mod users {
    use zapline::prelude::*;

    #[get("/users")]
    pub async fn list() -> &'static str {
        "users"
    }

    #[get("/users/{id}")]
    pub async fn show(Path(id): Path<u64>) -> String {
        id.to_string()
    }
}

mod posts {
    use zapline::prelude::*;

    #[get("/posts")]
    pub async fn list() -> &'static str {
        "posts"
    }

    #[get("/posts/{id}", operation_id = "users_list")]
    pub async fn named(Path(id): Path<u64>) -> String {
        id.to_string()
    }
}

#[test]
fn clashing_handler_names_get_their_module_as_prefix() {
    let app = App::new().mount((users::list, users::show, posts::list, posts::named));
    let spec = spec_for(app, OpenApi::new("ids", "1"));
    let id = |path: &str| spec["paths"][path]["get"]["operationId"].clone();
    // Unique names stay as they are.
    assert_eq!(id("/users/{id}"), "show");
    // The explicit id is reserved first, so users::list falls back further.
    assert_eq!(id("/posts/{id}"), "users_list");
    assert_eq!(id("/posts"), "posts_list");
    assert_eq!(id("/users"), Value::from("users_list_get"));
}
