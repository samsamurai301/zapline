# Responses

A handler can return anything that implements `IntoResponse`:

| Return type | Response |
|---|---|
| `&'static str`, `String` | `200 text/plain; charset=utf-8` |
| `Bytes`, `Vec<u8>` | `200 application/octet-stream` |
| `Json<T>` | `200 application/json` |
| `Html<T>` | `200 text/html; charset=utf-8` |
| `StatusCode`, `()` | Empty body with that status (`()` → 200) |
| `Redirect::to(uri)` / `temporary` / `permanent` | 303 / 307 / 308 with `Location` |
| `Option<T>` | `T`, or a JSON `404` for `None` |
| `Result<T, E>` | `T` or `E` (any `IntoResponse`, usually `zapline::Error`) |
| `(StatusCode, T)` | `T` with that status |
| `([(HeaderName, HeaderValue); N], T)` | `T` with extra headers |
| `(StatusCode, [(HeaderName, HeaderValue); N], T)` | Both |
| `Response` | Full control |

```rust
use zapline::prelude::*;
use zapline::header;

#[post("/users")]
async fn create(Json(u): Json<NewUser>) -> Result<(StatusCode, Json<User>)> {
    let user = save(u).await?;
    Ok((StatusCode::CREATED, Json(user)))
}

async fn download() -> impl IntoResponse {
    (
        [(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"report.csv\""))],
        "id,name\n1,ada\n",
    )
}
```

Streaming responses such as Server-Sent Events, file downloads and NDJSON are covered in [Server-Sent Events & streaming](streaming.md).

## Custom response types

```rust
struct Csv(String);

impl IntoResponse for Csv {
    fn into_response(self) -> Response {
        ([(zapline::header::CONTENT_TYPE, HeaderValue::from_static("text/csv"))], self.0).into_response()
    }
}
```
