# Validation

Wrap an extractor in `Valid` and derive `Validate` on the type. Bad input never reaches your handler.

```rust
use serde::Deserialize;
use validator::Validate;
use zapline::prelude::*;
use zapline::validate::Valid;

#[derive(Deserialize, Validate)]
struct NewUser {
    #[validate(length(min = 1, max = 64))]
    name: String,
    #[validate(email)]
    email: String,
}

#[post("/users")]
async fn create_user(Valid(Json(user)): Valid<Json<NewUser>>) -> StatusCode {
    println!("creating {} <{}>", user.name, user.email);
    StatusCode::CREATED
}
```

Requirements:

- Cargo feature `validate` (on by default, implies `json`).
- `#[model]` derives `Validate` (plus serde and `JsonSchema`) without adding any crate. The plain derive expands to paths in the `validator` crate, so add it to your `Cargo.toml`:

  ```toml
  validator = { version = "0.21", features = ["derive"] }
  ```

  `zapline::validate` re-exports the crate (`zapline::validate::validator`, plus `Validate`, `ValidationError` and `ValidationErrors`). To skip the direct dependency, put `#[validate(crate = "zapline::validate::validator")]` on the struct.

## Which extractors

`Valid<E>` works with `Json<T>`, `Form<T>`, `Query<T>` and `Path<T>`, where `T: Validate`. It derefs to the wrapped extractor (which derefs to `T`) and destructures like the examples show.

| You write | Source | Position |
|---|---|---|
| `Valid<Json<T>>` | JSON body | last argument |
| `Valid<Form<T>>` | URL-encoded form body | last argument |
| `Valid<Query<T>>` | query string | any |
| `Valid<Path<T>>` | route parameters | any |

```rust
use serde::Deserialize;
use validator::Validate;
use zapline::prelude::*;
use zapline::validate::Valid;

#[derive(Deserialize, Validate)]
struct Page {
    #[validate(range(min = 1, max = 100))]
    size: u32,
    #[validate(length(max = 10))]
    q: Option<String>,
}

#[derive(Deserialize, Validate)]
struct Ids {
    #[validate(range(min = 1))]
    id: u64,
}

#[get("/things/{id}")]
async fn list(Valid(Path(ids)): Valid<Path<Ids>>, page: Valid<Query<Page>>) -> String {
    // `page` derefs through `Valid` and `Query` to `Page`.
    format!("{}:{}", ids.id, page.size)
}
```

The wrapped extractor runs first, so its own failures are unchanged: a wrong content type is still a `415`, an oversized body a `413`, malformed JSON or a missing field a `422` (without validation details), and an unparsable query string or path parameter a `400`. Only a value that deserializes but breaks a rule produces the validation error below.

## The 422 response

A failed validation lists **every** broken rule, not just the first one:

```json
{
  "error": "validation failed",
  "code": "validation_error",
  "details": [
    { "code": "length", "field": "address.zip", "message": "zip must have 5 digits", "params": { "equal": 5 } },
    { "code": "range", "field": "age", "message": "must be at least 18 and at most 120", "params": { "max": 120, "min": 18 } },
    { "code": "no_spaces", "field": "contacts[1].handle", "message": "must not contain spaces", "params": {} },
    { "code": "length", "field": "contacts[1].name", "message": "length must be at least 1", "params": { "min": 1 } },
    { "code": "email", "field": "email", "message": "must be a valid email address", "params": {} },
    { "code": "length", "field": "password", "message": "must be at least 8 characters", "params": { "min": 8 } }
  ]
}
```

Each entry of `details` has:

| Key | Meaning |
|---|---|
| `field` | Path to the field: `email`, `address.zip`, `contacts[1].name`. Empty for struct-level (schema) errors on the top-level value. |
| `code` | The rule that failed: `email`, `length`, `range`, `url`, `must_match`, ... or the code of your custom validator |
| `message` | The rule's `message = ".."`, or a readable default such as `length must be between 1 and 64` |
| `params` | The rule's parameters (`{"min": 8}`). **Never includes the rejected value.** |

The list is sorted by `field`, so responses are stable. Rejected values are never echoed back, which keeps passwords and tokens out of error responses and logs.

`details` is the standard `details` member of a [zapline error](errors.md), so `App::error_handler` can re-render it, and the OpenAPI spec documents the `422` automatically.

## Nested structs and lists

Add `#[validate(nested)]` to fields whose type also derives `Validate`. List items are reported with their index:

```rust
use serde::Deserialize;
use validator::Validate;

#[derive(Deserialize, Validate)]
struct Address {
    #[validate(length(equal = 5, message = "zip must have 5 digits"))]
    zip: String,
}

#[derive(Deserialize, Validate)]
struct Contact {
    #[validate(length(min = 1))]
    name: String,
}

#[derive(Deserialize, Validate)]
struct Customer {
    #[validate(nested)]
    address: Address,            // -> address.zip
    #[validate(nested)]
    contacts: Vec<Contact>,      // -> contacts[1].name
}
```

`validator` records one kind of error per field. If a list breaks its own rule (`#[validate(length(max = 2), nested)]` with three items), you get that error first, and the item errors appear once it passes.

## Built-in rules

These are `validator` 0.21's rules; see its documentation for all options.

| Rule | Example |
|---|---|
| `email`, `url`, `ip`, `non_control_character` | `#[validate(email)]` |
| `length` | `#[validate(length(min = 1, max = 64))]` or `length(equal = 5)`. Works on strings, lists and maps. |
| `range` | `#[validate(range(min = 18, max = 120))]`, also `exclusive_min` / `exclusive_max` |
| `must_match` | `#[validate(must_match(other = "password"))]` |
| `contains`, `does_not_contain` | `#[validate(contains(pattern = "@"))]` |
| `regex` | `#[validate(regex(path = *USERNAME))]` with a `regex::Regex` static |
| `required` | `#[validate(required)]` on an `Option<T>` |
| `custom` | `#[validate(custom(function = "name"))]` |
| `nested` | `#[validate(nested)]` |

Every rule accepts `message = ".."` (shown to the client) and `code = ".."`.

## Custom validators

A field validator takes a reference to the value and returns `Result<(), ValidationError>`. The error code and message end up in the response:

```rust
use std::borrow::Cow;
use serde::Deserialize;
use validator::{Validate, ValidationError};

fn no_spaces(value: &str) -> Result<(), ValidationError> {
    if value.contains(' ') {
        return Err(ValidationError::new("no_spaces")
            .with_message(Cow::Borrowed("must not contain spaces")));
    }
    Ok(())
}

#[derive(Deserialize, Validate)]
struct Handle {
    #[validate(custom(function = "no_spaces"))]
    handle: String,
}
```

Rules that involve several fields are struct-level validators. Their `field` is empty (or the parent's path when nested):

```rust
use std::borrow::Cow;
use serde::Deserialize;
use validator::{Validate, ValidationError};

fn check_total(order: &Totals) -> Result<(), ValidationError> {
    if order.a + order.b > 10 {
        return Err(ValidationError::new("total")
            .with_message(Cow::Borrowed("a + b must be <= 10")));
    }
    Ok(())
}

#[derive(Deserialize, Validate)]
#[validate(schema(function = "check_total"))]
struct Totals {
    a: u32,
    b: u32,
}
```

Validators here must be synchronous and pure. For checks that need the database ("is this email taken?"), do them in the handler and return `Error::conflict(..).with_code("email_taken")`.

## Validating by hand

`Valid` is a convenience around two public functions. Use them when the data doesn't come from an extractor, or to merge validation with your own checks:

```rust
use serde::Deserialize;
use validator::Validate;
use zapline::prelude::*;
use zapline::validate::validation_error;

#[derive(Deserialize, Validate)]
struct Link {
    #[validate(url)]
    link: String,
}

async fn save(Json(body): Json<Link>) -> Result<String> {
    // A plain `?` on `ValidationErrors` would produce a 500, like any other
    // `std::error::Error`. `validation_error` builds the same 422 as `Valid`.
    body.validate().map_err(validation_error)?;
    Ok(body.link)
}
```

`zapline::validate::field_errors(&errors)` returns the flattened, sorted `Vec<FieldError>` if you want to build your own response.

## HTML forms

A JSON 422 is right for an API client, but a browser that posts a form needs the form back, with a message next to each field. For pages, take `ValidForm<T>` (feature `views`) instead of `Valid<Form<T>>`: it runs the same rules but hands the outcome to the handler as a `Result`, and `views.render_invalid` re-renders the form as a 422 with `{{ errors.field }}` and `{{ old.field }}`:

```rust,ignore
use zapline::view::{context, ValidForm, Views};

#[post("/signup")]
async fn signup(views: Views, ValidForm(form): ValidForm<SignUp>) -> Result<Response> {
    let form = match form {
        Ok(form) => form,
        Err(invalid) => return views.render_invalid("signup.html", context! {}, invalid),
    };
    // ...
    Ok(Redirect::to("/").into_response())
}
```

The messages are the ones listed above; fields that don't parse at all get "must be a number" or "is required". Struct-level (`schema`) errors appear as `{{ errors._form }}`. `FormErrors::from(&validation_errors)` and `FormErrors::from_error(&err)` convert by hand. See [Views: Forms](views.md#forms-errors-next-to-each-field).

## OpenAPI

`Valid<Json<T>>` documents the request body exactly like `Json<T>` and adds a `422 Validation Error` response. The `#[validate(..)]` rules are visible to `#[derive(JsonSchema)]`, so `length`, `range` and `email` also appear in the schema as `minLength`, `maximum`, `format: email` and so on. See [OpenAPI & interactive docs](openapi.md).
