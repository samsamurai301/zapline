//! The blog service: SeaORM queries behind plain methods. Handlers never see
//! an entity; they get the API types below, so the schema can change
//! without changing the API.

use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, DbErr, EntityTrait,
    ItemsAndPagesNumber, ModelTrait, PaginatorTrait, QueryFilter, QueryOrder, Set, SqlErr,
    TransactionError, TransactionTrait,
};
use zapline::prelude::*;

use crate::entity::{author, post};

/// An author, with their posts when asked for one author.
#[model]
#[derive(PartialEq)]
pub struct Author {
    pub id: i32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posts: Option<Vec<Post>>,
}

/// A post.
#[model]
#[derive(PartialEq)]
pub struct Post {
    pub id: i32,
    pub author_id: i32,
    pub title: String,
    pub body: String,
    pub published: bool,
}

/// Body of `POST /authors`.
#[model]
pub struct NewAuthor {
    #[validate(length(min = 1, max = 100))]
    pub name: String,
}

/// Body of `POST /authors/{id}/posts`.
#[model]
pub struct NewPost {
    #[validate(length(min = 1, max = 200))]
    pub title: String,
    #[serde(default)]
    pub body: String,
}

/// Body of `PATCH /posts/{id}`: only the fields that are present change.
#[model]
pub struct PostChanges {
    #[validate(length(min = 1, max = 200))]
    pub title: Option<String>,
    pub body: Option<String>,
    pub published: Option<bool>,
}

/// Query string of `GET /posts`.
#[model]
pub struct PostFilter {
    /// Zero-based page number.
    #[serde(default)]
    pub page: u64,
    #[serde(default = "default_per_page")]
    #[validate(range(min = 1, max = 100))]
    pub per_page: u64,
    pub published: Option<bool>,
    pub author_id: Option<i32>,
}

fn default_per_page() -> u64 {
    20
}

/// One page of posts.
#[model]
pub struct PostPage {
    pub items: Vec<Post>,
    pub page: u64,
    pub per_page: u64,
    pub total_items: u64,
    pub total_pages: u64,
}

/// Body of `POST /authors/{id}/merge`.
#[model]
pub struct Merge {
    /// The author whose posts move; deleted afterwards.
    pub from_id: i32,
    /// A new name for the remaining author.
    #[validate(length(min = 1, max = 100))]
    pub rename_to: Option<String>,
}

/// The blog service, registered with `provide_with` and received as
/// `Dep<Blog>`.
#[derive(Clone)]
pub struct Blog {
    db: Arc<DatabaseConnection>,
}

impl Blog {
    /// A service on an open connection.
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Blog { db }
    }

    /// Creates an author. A taken name is a `409 unique_violation`.
    pub async fn create_author(&self, new: NewAuthor) -> Result<Author> {
        let created = author::ActiveModel {
            name: Set(new.name),
            ..Default::default()
        }
        .insert(&*self.db)
        .await
        .map_err(db_error)?;
        Ok(Author::from(created))
    }

    /// One author with all their posts, loaded in one query.
    pub async fn author(&self, id: i32) -> Result<Author> {
        let mut found = author::Entity::find_by_id(id)
            .find_with_related(post::Entity)
            .order_by_asc(post::Column::Id)
            .all(&*self.db)
            .await
            .map_err(db_error)?;
        let (model, posts) = found.pop().ok_or_else(Error::not_found)?;
        let mut author = Author::from(model);
        author.posts = Some(posts.into_iter().map(Post::from).collect());
        Ok(author)
    }

    /// Deletes an author; their posts go with them (`ON DELETE CASCADE`).
    pub async fn delete_author(&self, id: i32) -> Result<()> {
        let found = author::Entity::find_by_id(id)
            .one(&*self.db)
            .await
            .map_err(db_error)?
            .ok_or_else(Error::not_found)?;
        found.delete(&*self.db).await.map_err(db_error)?;
        Ok(())
    }

    /// Adds a draft post to an author.
    pub async fn create_post(&self, author_id: i32, new: NewPost) -> Result<Post> {
        author::Entity::find_by_id(author_id)
            .one(&*self.db)
            .await
            .map_err(db_error)?
            .ok_or_else(Error::not_found)?;
        let created = post::ActiveModel {
            author_id: Set(author_id),
            title: Set(new.title),
            body: Set(new.body),
            published: Set(false),
            ..Default::default()
        }
        .insert(&*self.db)
        .await
        .map_err(db_error)?;
        Ok(Post::from(created))
    }

    /// Changes the fields of a post that `changes` sets. SeaORM's
    /// `ActiveModel` only writes columns marked `Set`.
    pub async fn update_post(&self, id: i32, changes: PostChanges) -> Result<Post> {
        let found = post::Entity::find_by_id(id)
            .one(&*self.db)
            .await
            .map_err(db_error)?
            .ok_or_else(Error::not_found)?;
        let mut active: post::ActiveModel = found.into();
        if let Some(title) = changes.title {
            active.title = Set(title);
        }
        if let Some(body) = changes.body {
            active.body = Set(body);
        }
        if let Some(published) = changes.published {
            active.published = Set(published);
        }
        let updated = active.update(&*self.db).await.map_err(db_error)?;
        Ok(Post::from(updated))
    }

    /// A page of posts, newest first, optionally filtered.
    pub async fn posts(&self, filter: &PostFilter) -> Result<PostPage> {
        let mut condition = Condition::all();
        if let Some(published) = filter.published {
            condition = condition.add(post::Column::Published.eq(published));
        }
        if let Some(author_id) = filter.author_id {
            condition = condition.add(post::Column::AuthorId.eq(author_id));
        }
        let paginator = post::Entity::find()
            .filter(condition)
            .order_by_desc(post::Column::Id)
            .paginate(&*self.db, filter.per_page);
        let ItemsAndPagesNumber {
            number_of_items,
            number_of_pages,
        } = paginator.num_items_and_pages().await.map_err(db_error)?;
        let items = paginator.fetch_page(filter.page).await.map_err(db_error)?;
        Ok(PostPage {
            items: items.into_iter().map(Post::from).collect(),
            page: filter.page,
            per_page: filter.per_page,
            total_items: number_of_items,
            total_pages: number_of_pages,
        })
    }

    /// Moves every post of `merge.from_id` to author `into_id`, deletes
    /// `from_id`, and optionally renames `into_id`: in one transaction, so a
    /// failure at any step (say, the new name is taken) undoes all of them.
    pub async fn merge_authors(&self, into_id: i32, merge: Merge) -> Result<Author> {
        let from_id = merge.from_id;
        if from_id == into_id {
            return Err(
                Error::bad_request("cannot merge an author into itself").with_code("same_author")
            );
        }
        self.db
            .transaction::<_, (), DbErr>(|txn| {
                Box::pin(async move {
                    let into = author::Entity::find_by_id(into_id)
                        .one(txn)
                        .await?
                        .ok_or_else(|| DbErr::RecordNotFound(format!("author {into_id}")))?;
                    post::Entity::update_many()
                        .col_expr(post::Column::AuthorId, Expr::value(into_id))
                        .filter(post::Column::AuthorId.eq(from_id))
                        .exec(txn)
                        .await?;
                    let deleted = author::Entity::delete_by_id(from_id).exec(txn).await?;
                    if deleted.rows_affected == 0 {
                        return Err(DbErr::RecordNotFound(format!("author {from_id}")));
                    }
                    if let Some(name) = merge.rename_to {
                        let mut active: author::ActiveModel = into.into();
                        active.name = Set(name);
                        active.update(txn).await?;
                    }
                    Ok(())
                })
            })
            .await
            .map_err(|e| match e {
                TransactionError::Connection(e) | TransactionError::Transaction(e) => db_error(e),
            })?;
        self.author(into_id).await
    }

    /// Readiness check: the database answers.
    pub async fn ping(&self) -> Result<()> {
        self.db
            .ping()
            .await
            .map_err(|_| Error::service_unavailable("database did not answer"))
    }
}

/// Turns a SeaORM error into the HTTP error a client should see. The same
/// statuses and codes zapline uses for its own sqlx errors, so an app can mix
/// both.
pub fn db_error(e: DbErr) -> Error {
    match e.sql_err() {
        Some(SqlErr::UniqueConstraintViolation(_)) => {
            return Error::conflict("already exists").with_code("unique_violation");
        }
        Some(SqlErr::ForeignKeyConstraintViolation(_)) => {
            return Error::conflict("referenced row does not exist or is still in use")
                .with_code("foreign_key_violation");
        }
        _ => {}
    }
    match e {
        DbErr::RecordNotFound(_) => Error::not_found(),
        DbErr::ConnectionAcquire(_) => {
            Error::service_unavailable("database busy").with_code("db_pool_timeout")
        }
        // Logged by zapline, never sent to the client.
        other => Error::internal(other.to_string()),
    }
}

impl From<author::Model> for Author {
    fn from(m: author::Model) -> Self {
        Author {
            id: m.id,
            name: m.name,
            posts: None,
        }
    }
}

impl From<post::Model> for Post {
    fn from(m: post::Model) -> Self {
        Post {
            id: m.id,
            author_id: m.author_id,
            title: m.title,
            body: m.body,
            published: m.published,
        }
    }
}
