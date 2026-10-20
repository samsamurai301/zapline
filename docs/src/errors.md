# Errors

zapline has one error type, `zapline::Error`, and one result alias, `zapline::Result<T>`.

```rust
#[get("/users/{id}")]
async fn get_user(State(db): State<Db>, Path(id): Path<u64>) -> Result<Json<User>> {
    let user = db.find(id).await?                // any std::error::Error -> 500
        .ok_or_else(Error::not_found)?;          // explicit client error -> 404
    Ok(Json(user))
}
```

## Rules

- **Any `std::error::Error` converts into a 500 with `?`.** The underlying error is logged (through `tracing` when enabled) and **never** sent to the client. The client sees `{"error":"internal server error"}`.
- **Client errors are explicit.** Use the constructors: `bad_request(msg)`, `unauthorized()`, `forbidden()`, `not_found()`, `conflict(msg)`, `unprocessable(msg)`, `too_many_requests()`, `service_unavailable(msg)`, `payload_too_large()`, `unsupported_media_type(msg)`, or `Error::new(status, msg)`.
- **Errors are structured.** Each one renders as JSON:

  ```json
  { "error": "validation failed", "code": "validation_error", "details": [ ... ] }
  ```

  `code` and `details` are omitted when unset.

```rust
Err(Error::conflict("email already registered")
    .with_code("email_taken")                       // stable, machine-readable
    .with_details(json!({ "field": "email" }))      // structured context
    .with_header(header::RETRY_AFTER, HeaderValue::from_static("30")))
```

## Customizing the error format

Every error response includes an `ErrorReport` (status, message, code, details) in its extensions. That covers extractor rejections, handler errors, 404s and 405s. `App::error_handler` re-renders all of them in one place. For example, to emit RFC 7807 `application/problem+json`:

```rust
App::new().error_handler(|e: &ErrorReport| {
    let body = serde_json::json!({
        "type": "about:blank",
        "title": e.status.canonical_reason(),
        "status": e.status.as_u16(),
        "detail": e.message,
        "code": e.code,
    });
    (e.status, [(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"))], body.to_string())
        .into_response()
})
```

## Domain errors

Map your own error enums into `zapline::Error` once, and `?` will work everywhere:

```rust
enum AppError { NotFound, Db(sqlx::Error) }

impl From<AppError> for zapline::Error {
    fn from(e: AppError) -> Self {
        match e {
            AppError::NotFound => Error::not_found().with_code("user_not_found"),
            AppError::Db(e) => Error::internal("database error").with_source(e),
        }
    }
}
```

(Implement `From` for your error type instead of `std::error::Error`. Otherwise, the blanket conversion turns it into a plain 500.)
