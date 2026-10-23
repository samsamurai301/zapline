//! seaorm-blog: a blog API on zapline + SeaORM.
//!
//! - **Entities and relations** (`entity.rs`): authors have many posts.
//! - **Migrations** (`migration.rs`) with `sea-orm-migration`, applied at
//!   startup.
//! - **Dependency injection**: the `DatabaseConnection` is built by a startup
//!   factory, the `Blog` service by a second one that depends on it;
//!   handlers receive `Dep<Blog>`.
//! - **Eager loading, pagination, partial updates** and a **transaction**
//!   (merging two authors) in `blog.rs`.
//! - **Health**: `/health/ready` pings the database.

pub mod blog;
pub mod entity;
pub mod migration;

use std::time::Duration;

use sea_orm::{ConnectOptions, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use zapline::health::HealthCheck;
use zapline::prelude::*;

pub use blog::{Author, Blog, Merge, NewAuthor, NewPost, Post, PostChanges, PostFilter, PostPage};
use migration::Migrator;

/// Connection settings for a SQLite file (or any URL SeaORM accepts).
pub fn connect_options(url: impl Into<String>) -> ConnectOptions {
    let mut options = ConnectOptions::new(url.into());
    options
        .acquire_timeout(Duration::from_secs(5))
        .sqlx_logging(false);
    options
}

/// A private in-memory database, for tests. SQLite gives each connection to
/// `sqlite::memory:` its own database, so the pool holds exactly one.
pub fn memory() -> ConnectOptions {
    let mut options = connect_options("sqlite::memory:");
    options.max_connections(1).min_connections(1);
    options
}

/// Builds the app. Startup opens the connection and applies the
/// migrations; if either fails, the app doesn't start.
pub fn app(options: ConnectOptions) -> App {
    App::new()
        .provide_with(|_res: Resources| async move {
            let db = sea_orm::Database::connect(options)
                .await
                .map_err(blog::db_error)?;
            Migrator::up(&db, None).await.map_err(blog::db_error)?;
            Ok(db)
        })
        .provide_with(
            |res: Resources| async move { Ok(Blog::new(res.dep::<DatabaseConnection>()?)) },
        )
        .plugin(Health::new().check("database", DatabasePing))
        .openapi(OpenApi::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
        .mount((
            create_author,
            get_author,
            delete_author,
            merge_authors,
            create_post,
            update_post,
            list_posts,
        ))
}

/// A reusable readiness check for any app with a `Blog`.
pub struct DatabasePing;

impl HealthCheck for DatabasePing {
    async fn check(&self, res: Resources) -> Result<()> {
        res.dep::<Blog>()?.ping().await
    }
}

/// Create an author.
#[post("/authors", tag = "authors")]
async fn create_author(
    blog: Dep<Blog>,
    Valid(Json(new)): Valid<Json<NewAuthor>>,
) -> Result<(StatusCode, Json<Author>)> {
    Ok((StatusCode::CREATED, Json(blog.create_author(new).await?)))
}

/// An author and all their posts.
#[get("/authors/{id}", tag = "authors")]
async fn get_author(blog: Dep<Blog>, Path(id): Path<i32>) -> Result<Json<Author>> {
    Ok(Json(blog.author(id).await?))
}

/// Delete an author and their posts.
#[delete("/authors/{id}", tag = "authors")]
async fn delete_author(blog: Dep<Blog>, Path(id): Path<i32>) -> Result<StatusCode> {
    blog.delete_author(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Merge another author into this one, in one transaction.
#[post("/authors/{id}/merge", tag = "authors")]
async fn merge_authors(
    blog: Dep<Blog>,
    Path(id): Path<i32>,
    Valid(Json(merge)): Valid<Json<Merge>>,
) -> Result<Json<Author>> {
    Ok(Json(blog.merge_authors(id, merge).await?))
}

/// Add a draft post.
#[post("/authors/{id}/posts", tag = "posts")]
async fn create_post(
    blog: Dep<Blog>,
    Path(id): Path<i32>,
    Valid(Json(new)): Valid<Json<NewPost>>,
) -> Result<(StatusCode, Json<Post>)> {
    Ok((StatusCode::CREATED, Json(blog.create_post(id, new).await?)))
}

/// Change a post's title, body or published flag.
#[patch("/posts/{id}", tag = "posts")]
async fn update_post(
    blog: Dep<Blog>,
    Path(id): Path<i32>,
    Valid(Json(changes)): Valid<Json<PostChanges>>,
) -> Result<Json<Post>> {
    Ok(Json(blog.update_post(id, changes).await?))
}

/// Posts, newest first: `?page=0&per_page=20&published=true&author_id=1`.
#[get("/posts", tag = "posts")]
async fn list_posts(
    blog: Dep<Blog>,
    Valid(Query(filter)): Valid<Query<PostFilter>>,
) -> Result<Json<PostPage>> {
    Ok(Json(blog.posts(&filter).await?))
}
