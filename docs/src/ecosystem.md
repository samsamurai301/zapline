# Ecosystem: sqlx, SeaORM, Diesel, Leptos

zapline has no ORM of its own and no opinion on which one you use. Whatever
you pick plugs into the same three hooks:

1. **A startup factory** opens the pool and runs migrations
   (`App::database` for sqlx, `App::provide_with` for anything else). If it
   fails, `run` returns the error and the port is never bound.
2. **A service** built from the pool by a second factory, received in
   handlers as `Dep<Service>` (see [Dependency injection](di.md)).
3. **A readiness check** that pings the database through the same pool
   (see [Health checks](observability.md#health-checks)).

Four complete, tested apps live in
[`examples/ecosystem`](https://github.com/samsamurai301/zapline/tree/main/examples/ecosystem).
Each runs on SQLite with nothing to install:

| Example | Library | Shows |
|---|---|---|
| `sqlx-transactions` | sqlx through `zapline::db` | Transactions, savepoints, idempotency keys, concurrent writers, a health check that audits the books |
| `seaorm` | SeaORM 1.1 | Entities and relations, `sea-orm-migration`, eager loading, pagination, partial updates, a multi-step transaction |
| `diesel` | Diesel 2.3 | A blocking ORM on the async server, embedded migrations, stock reservation that never oversells |
| `leptos-ssr` | Leptos 0.8 | Leptos components rendered by zapline handlers, forms that work without JavaScript |

```sh
cd examples/ecosystem
cargo test                  # all four (their own workspace and Cargo.lock)
cd seaorm && cargo run      # http://localhost:8080 , API docs at /docs
```

## Transactions with sqlx

`Db` (and the `Pool` in resources) is a plain sqlx pool, so a transaction is
`begin`, your queries on `&mut *tx`, then `commit`. Returning early with `?`
drops the transaction, and sqlx rolls it back:

```rust,ignore
pub async fn transfer(&self, t: &NewTransfer, key: Option<&str>) -> Result<Outcome> {
    // SQLite: take the write lock up front, so concurrent writers wait on the
    // busy timeout instead of failing with "database is locked".
    let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(key) = key {
        if let Some(previous) = find_by_key(&mut tx, key).await? {
            return replay(previous, t);      // a retry: nothing moves twice
        }
    }
    let created = move_money(&mut tx, t, key).await?;   // debit, credit, record
    tx.commit().await?;
    Ok(Outcome::Created(created))
}
```

Three rules from the ledger example carry over to any app:

- **Check and change in one statement.**
  `UPDATE accounts SET balance_cents = balance_cents - $1 WHERE id = $2 AND balance_cents >= $1`
  can't be raced; a `SELECT` followed by an `UPDATE` can. Look at
  `rows_affected()` to tell why nothing changed.
- **Savepoints for partial success.** `tx.begin()` inside a transaction
  issues `SAVEPOINT`; `commit` releases it and `rollback` undoes only that
  part:

  ```rust,ignore
  let mut savepoint = tx.begin().await?;
  match move_money(&mut savepoint, transfer, None).await {
      Ok(created) => { savepoint.commit().await?; items.push(BatchItem::ok(i, created)); }
      Err(e) => { savepoint.rollback().await?; items.push(BatchItem::failed(i, &e)); }
  }
  ```
- **Idempotency keys** are a `UNIQUE` column read and written in the same
  transaction: a retried `POST` with the same `Idempotency-Key` header gets
  the original result (`200`), and the same key with a different body is a
  `422`.

A health check can verify more than connectivity. The ledger's checks that
money is conserved:

```rust,ignore
App::new()
    .database(db)
    .provide_with(|res: Resources| async move {
        let pool: SqlitePool = res.require()?;   // registered by .database(..)
        Ok(Ledger::new(pool))
    })
    .plugin(
        Health::new()
            .check("database", DbCheck::new())
            .check("books", |res: Resources| async move {
                res.dep::<Ledger>()?.check_books().await
            }),
    )
```

On Postgres, use `begin()` instead of `BEGIN IMMEDIATE`: the conditional
`UPDATE` locks the row.

## SeaORM

Leave zapline's `sqlite`/`postgres` features off; SeaORM has its own pool.
Open it (and migrate) in a factory, then build services from it:

```rust,ignore
App::new()
    .provide_with(|_res: Resources| async move {
        let db = sea_orm::Database::connect(options).await.map_err(db_error)?;
        Migrator::up(&db, None).await.map_err(db_error)?;
        Ok(db)
    })
    .provide_with(|res: Resources| async move {
        Ok(Blog::new(res.dep::<DatabaseConnection>()?))
    })
    .plugin(Health::new().check("database", DatabasePing))
```

`DbErr` and `zapline::Error` are both foreign to your crate, so convert with
a function instead of `?` alone. Map them to the codes zapline uses for sqlx,
so clients see one vocabulary:

```rust,ignore
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
        DbErr::ConnectionAcquire(_) => Error::service_unavailable("database busy").with_code("db_pool_timeout"),
        other => Error::internal(other.to_string()),   // logged, not sent
    }
}
```

Transactions use SeaORM's closure form; returning `Err` from it rolls back
every step:

```rust,ignore
self.db.transaction::<_, (), DbErr>(|txn| Box::pin(async move {
    post::Entity::update_many()
        .col_expr(post::Column::AuthorId, Expr::value(into_id))
        .filter(post::Column::AuthorId.eq(from_id))
        .exec(txn).await?;
    author::Entity::delete_by_id(from_id).exec(txn).await?;
    if let Some(name) = merge.rename_to {
        let mut active: author::ActiveModel = into.into();
        active.name = Set(name);
        active.update(txn).await?;                   // a taken name undoes both steps above
    }
    Ok(())
})).await
```

Return API types from handlers (`#[model]` structs with OpenAPI schemas),
not entities; a `From<post::Model>` impl keeps the mapping in one place.

## Diesel

Diesel is synchronous: run it on tokio's blocking pool, never directly in a
handler. The example's whole adapter is one method:

```rust,ignore
pub async fn run<T, F>(&self, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut SqliteConnection) -> Result<T, DbError> + Send + 'static,
{
    let pool = self.0.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|_| DbError::PoolTimeout)?;
        f(&mut conn)
    })
    .await
    .map_err(|_| Error::internal("database task panicked"))?;
    Ok(result?)
}
```

`DbError` is your own type, so `impl From<diesel::result::Error> for DbError`
and `impl From<DbError> for zapline::Error` are allowed, and `?` works on
both sides of the closure. Migrations are compiled in with
`embed_migrations!("migrations")` and applied in the factory that builds the
pool. On SQLite, `conn.immediate_transaction(|conn| ..)` takes the write
lock up front; with a conditional `UPDATE .. RETURNING`, thirty concurrent
orders for the last ten units give exactly ten `201`s and twenty
`409 out_of_stock`s.

[diesel-async](https://docs.rs/diesel-async) also fits: register its pool
with `provide_with` and await queries in the service directly.

## Leptos (server-side rendering)

Leptos components render to HTML strings with its `ssr` feature, so a
zapline handler can load data as usual and return the rendered page:

```rust,ignore
use leptos::prelude::*;

pub fn render<F: FnOnce() -> V, V: IntoView>(view: F) -> Html<String> {
    let owner = Owner::new();                       // a reactive scope per render
    let body = owner.with(|| view().to_html());
    Html(format!("<!DOCTYPE html>{body}"))
}

#[get("/")]
async fn home(db: Db) -> Result<Html<String>> {
    let notes = all_notes(&db).await?;
    Ok(render(move || view! {
        <Page title="Notes">
            <NotesBoard notes=notes error=None draft=String::new()/>
        </Page>
    }))
}
```

Forms post to ordinary zapline routes, which answer with `Redirect::to("/")`
(Post/Redirect/Get), or with status `422` and the page re-rendered with the
error and the user's input. Leptos escapes text, so user content can't
inject markup. For browser interactivity, build a second crate with Leptos's
`hydrate` feature to WASM and serve its `pkg/` directory with
[`ServeDir`](files.md); the server side stays the same.

If you prefer templates to components, zapline's own
[views](views.md) (MiniJinja + htmx) need no extra crates.
