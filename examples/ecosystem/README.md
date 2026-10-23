# Ecosystem examples

zapline with the database and UI libraries you already use. Each example is
a complete, tested app on SQLite: no database server, no Node, nothing to
install.

| Example | Library | Shows |
|---|---|---|
| [`sqlx-transactions`](sqlx-transactions) | [sqlx](https://docs.rs/sqlx) via `zapline::db` | A double-entry ledger: transactions, savepoints, idempotency keys, concurrent writers, a health check that audits the books |
| [`seaorm`](seaorm) | [SeaORM](https://www.sea-ql.org/SeaORM/) 1.1 | A blog API: entities and relations, `sea-orm-migration`, eager loading, pagination, partial updates, a multi-step transaction |
| [`diesel`](diesel) | [Diesel](https://diesel.rs) 2.3 | A shop: a blocking ORM on the async server (r2d2 + `spawn_blocking`), embedded migrations, `BEGIN IMMEDIATE` stock reservation that never oversells |
| [`leptos-ssr`](leptos-ssr) | [Leptos](https://leptos.dev) 0.8 | Server-rendered Leptos components as zapline responses, with forms that work without JavaScript |

All four use the same zapline building blocks:

- **Dependency injection**: the pool or connection is built by a startup
  factory (`App::database` or `App::provide_with`), the service by a second
  factory that depends on it, and handlers receive `Dep<Service>`.
- **Health checks**: `/health/ready` pings the database through the same
  pool the handlers use.
- **One error vocabulary**: each ORM's errors map to the statuses and codes
  zapline uses for sqlx (`404`, `409 unique_violation`,
  `409 foreign_key_violation`, `503 db_pool_timeout`, ...).
- **Tests through the real app**: `app(..).test()` with a fresh in-memory
  database per test, and a temporary file where a test needs concurrent
  connections.

## Running

This directory is its own Cargo workspace (with its own `Cargo.lock`), so
the ORMs and Leptos never slow down the main build.

```sh
cd examples/ecosystem
cargo test                          # all four
cargo test -p seaorm-blog           # one
cd diesel && cargo run              # http://localhost:8080 , API docs at /docs
```

From the repository root, `./scripts/check.sh ecosystem` runs clippy and the
tests of every example.

The guide's [Ecosystem page](../../docs/src/ecosystem.md) walks through the
patterns with snippets.
