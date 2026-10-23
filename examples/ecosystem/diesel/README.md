# diesel-shop: zapline + Diesel

Products, stock and orders on [Diesel](https://diesel.rs) 2.3 and SQLite
(compiled in, nothing to install).

```sh
cargo run                    # http://localhost:8080 , API docs at /docs
cargo test                   # 7 tests
```

## A blocking ORM on an async server

Diesel's connections are synchronous. `DbPool::run` (`src/db.rs`) runs a
closure with a pooled r2d2 connection on tokio's blocking thread pool, so a
query never stalls a server thread:

```rust,ignore
pub async fn product(&self, id: i32) -> Result<Product> {
    self.db
        .run(move |conn| Ok(products::table.find(id).select(Product::as_select()).first(conn)?))
        .await
}
```

Inside the closure `?` turns Diesel errors into `DbError`; outside, `?`
turns `DbError` into a zapline `Error` with the usual statuses (`404` for no
row, `409 unique_violation`, `503 db_pool_timeout`, ...). A panic in the
closure is a `500`, not a crashed server.

| Pattern | Where |
|---|---|
| `embed_migrations!`: SQL compiled into the binary, applied at startup | `src/db.rs` |
| Pool settings per connection (WAL, `busy_timeout`, foreign keys) | `Pragmas` in `src/db.rs` |
| `immediate_transaction`: the write lock up front, so concurrent orders queue | `Shop::place_order`, `Shop::cancel_order` |
| Reserve stock with one conditional `UPDATE .. RETURNING` | `Shop::place_order` |
| `Associations` / `belonging_to` for an order's lines | `load_lines` |
| DI: the pool and the service are startup factories | `app()` in `src/lib.rs` |
| Readiness: `SELECT 1` on a pooled connection | `Shop::ping` |

`concurrent_orders_never_oversell` sends 30 orders for the last 10 units at
once over 8 connections: exactly 10 succeed, 20 get `409 out_of_stock` with
the numbers in `details`, and the stock ends at zero.

## diesel-async

[diesel-async](https://docs.rs/diesel-async) works too: register its pool
(`bb8` or `deadpool`) with `provide_with` and await queries directly in the
service. For SQLite it wraps the same synchronous connection
(`SyncConnectionWrapper`), so this example uses the plain, widely known
`spawn_blocking` approach.
