# seaorm-blog: zapline + SeaORM

A blog API with authors and posts on [SeaORM](https://www.sea-ql.org/SeaORM/)
1.1 and SQLite. zapline's own `sqlite` feature is off: SeaORM brings its own
pool, and zapline's DI makes it available to handlers.

```sh
cargo run                    # http://localhost:8080 , API docs at /docs
cargo test                   # 8 tests, each on its own in-memory database
```

## How the pieces fit

```rust,ignore
App::new()
    // 1. Open the connection and migrate, once, at startup. If either
    //    fails, `run` returns the error and the port is never bound.
    .provide_with(|_res: Resources| async move {
        let db = sea_orm::Database::connect(options).await.map_err(db_error)?;
        Migrator::up(&db, None).await.map_err(db_error)?;
        Ok(db)
    })
    // 2. The service, built from the connection.
    .provide_with(|res: Resources| async move {
        Ok(Blog::new(res.dep::<DatabaseConnection>()?))
    })
    // 3. Readiness pings through the same connection.
    .plugin(Health::new().check("database", DatabasePing))
```

Handlers take `Dep<Blog>` and return API types (`Author`, `Post`, with
OpenAPI schemas), never entities, so the schema can change without changing
the API.

| Pattern | Where |
|---|---|
| Entities with `has_many` / `belongs_to` (`ON DELETE CASCADE`) | `src/entity.rs` |
| `sea-orm-migration` without its CLI, applied at startup | `src/migration.rs` |
| Eager loading with `find_with_related` | `Blog::author` |
| Pagination with `paginate` + `num_items_and_pages` | `Blog::posts` (`GET /posts?page=0&per_page=20&published=true`) |
| Partial updates: `ActiveModel` writes only `Set` columns | `Blog::update_post` (`PATCH /posts/{id}`) |
| A multi-step transaction (move posts, delete author, rename) | `Blog::merge_authors` (`POST /authors/{id}/merge`) |
| `DbErr` → HTTP errors with zapline's codes | `blog::db_error` |

`a_failed_merge_changes_nothing` proves the transaction: the rename at the
end hits the unique name, and the posts that had moved and the author that
had been deleted are both back.

## Other databases

Change the features to `sqlx-postgres` or `sqlx-mysql` and pass a matching
URL to `connect_options`; nothing else changes. SeaORM 2.x works the same
way but needs Rust 1.94.
