//! A FastAPI-style CRUD API: validated input, typed responses, and docs that
//! write themselves from the handlers.
//!
//!     cargo run -p zapline-examples --example openapi
//!     open http://localhost:8080/docs         # Swagger UI
//!     open http://localhost:8080/reference    # Scalar
//!     curl localhost:8080/openapi.json
//!
//!     curl -X POST localhost:8080/books -H 'authorization: Bearer demo-token' \
//!          -H 'content-type: application/json' \
//!          -d '{"title":"Dune","author":"Frank Herbert","year":1965}'
//!
//! Export the spec without starting the server:
//!
//!     ZAPLINE_EXPORT_OPENAPI=openapi.json cargo run -p zapline-examples --example openapi

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use validator::Validate;
use zapline::openapi::{DocsUi, OpenApi};
use zapline::prelude::*;
use zapline::validate::Valid;

/// A book in the catalog.
#[derive(Clone, Serialize, JsonSchema)]
struct Book {
    id: u64,
    title: String,
    author: String,
    /// Year of first publication.
    year: u16,
    tags: Vec<String>,
}

/// A new book.
#[derive(Deserialize, JsonSchema, Validate)]
struct NewBook {
    #[validate(length(min = 1, max = 200))]
    title: String,
    #[validate(length(min = 1, max = 100))]
    author: String,
    /// Year of first publication.
    #[validate(range(min = 1450, max = 2100))]
    year: u16,
    #[serde(default)]
    #[validate(length(max = 10))]
    tags: Vec<String>,
}

/// Fields to change; missing fields are left as they are.
#[derive(Deserialize, JsonSchema, Validate)]
struct BookPatch {
    #[validate(length(min = 1, max = 200))]
    title: Option<String>,
    #[validate(length(min = 1, max = 100))]
    author: Option<String>,
    #[validate(range(min = 1450, max = 2100))]
    year: Option<u16>,
}

/// Filters for listing books.
#[derive(Deserialize, JsonSchema, Validate)]
struct ListQuery {
    /// Only books by this author.
    author: Option<String>,
    /// Maximum number of results.
    #[validate(range(min = 1, max = 100))]
    limit: Option<usize>,
}

#[derive(Clone, Default)]
struct Db(Arc<RwLock<(u64, BTreeMap<u64, Book>)>>);

/// List books.
///
/// Books are returned in the order they were added.
#[get("/books", tag = "books")]
async fn list_books(
    State(db): State<Db>,
    Valid(Query(q)): Valid<Query<ListQuery>>,
) -> Json<Vec<Book>> {
    let db = db.0.read().unwrap();
    let books =
        db.1.values()
            .filter(|b| q.author.as_ref().is_none_or(|a| &b.author == a))
            .take(q.limit.unwrap_or(100))
            .cloned()
            .collect();
    Json(books)
}

/// Get a book.
#[get("/books/{id}", tag = "books")]
async fn get_book(State(db): State<Db>, Path(id): Path<u64>) -> Option<Json<Book>> {
    db.0.read().unwrap().1.get(&id).cloned().map(Json)
}

/// Add a book.
#[post("/books", tag = "books", status = 201)]
async fn create_book(
    State(db): State<Db>,
    Valid(Json(new)): Valid<Json<NewBook>>,
) -> (StatusCode, Json<Book>) {
    let mut db = db.0.write().unwrap();
    db.0 += 1;
    let book = Book {
        id: db.0,
        title: new.title,
        author: new.author,
        year: new.year,
        tags: new.tags,
    };
    db.1.insert(book.id, book.clone());
    (StatusCode::CREATED, Json(book))
}

/// Update a book.
#[patch("/books/{id}", tag = "books")]
async fn update_book(
    State(db): State<Db>,
    Path(id): Path<u64>,
    Valid(Json(patch)): Valid<Json<BookPatch>>,
) -> Result<Json<Book>> {
    let mut db = db.0.write().unwrap();
    let book = db.1.get_mut(&id).ok_or_else(Error::not_found)?;
    if let Some(t) = patch.title {
        book.title = t;
    }
    if let Some(a) = patch.author {
        book.author = a;
    }
    if let Some(y) = patch.year {
        book.year = y;
    }
    Ok(Json(book.clone()))
}

/// Delete a book.
#[delete("/books/{id}", tag = "books", status = 204)]
async fn delete_book(State(db): State<Db>, Path(id): Path<u64>) -> Result<StatusCode> {
    match db.0.write().unwrap().1.remove(&id) {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(Error::not_found()),
    }
}

/// Demo auth for write routes. Real apps would use `zapline::auth`.
async fn require_token(req: Request, next: Next) -> Response {
    match req.headers().get("authorization") {
        Some(v) if v == "Bearer demo-token" => next.run(req).await,
        _ => Error::unauthorized().into_response(),
    }
}

fn app() -> App {
    // Write routes share middleware and are documented as requiring a token.
    let writes = Routes::new()
        .mount((create_book, update_book, delete_book))
        .wrap(require_token)
        .security_all("bearer");

    App::new()
        .state(Db::default())
        .mount((list_books, get_book))
        .mount(writes)
        .get("/health", || async { "ok" })
        .summary("Liveness probe")
        .tag("ops")
        .openapi(
            OpenApi::new("Bookshelf API", "1.0.0")
                .description("A tiny catalog showing zapline's automatic API docs.")
                .tag("books", "Manage the catalog")
                .bearer_auth("bearer")
                .docs(DocsUi::Swagger, "/docs")
                .docs(DocsUi::Scalar, "/reference")
                .docs(DocsUi::Redoc, "/redoc"),
        )
}

fn main() -> std::io::Result<()> {
    println!("docs at http://localhost:8080/docs");
    app().run("0.0.0.0:8080")
}
