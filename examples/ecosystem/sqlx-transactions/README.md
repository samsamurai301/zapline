# sqlx-transactions: a ledger that can't lose money

A double-entry ledger on zapline's sqlx integration (`zapline::db`). It is
small, but it handles the cases that make money code hard: concurrent
transfers, retried requests, and batches that fail halfway.

```sh
cargo run                    # http://localhost:8080 , API docs at /docs
cargo test                   # 10 tests, each on its own SQLite database
```

```sh
curl -X POST localhost:8080/accounts -H 'content-type: application/json' \
     -d '{"owner":"ada","opening_balance_cents":10000}'
curl -X POST localhost:8080/accounts -H 'content-type: application/json' \
     -d '{"owner":"bob"}'
curl -X POST localhost:8080/transfers -H 'content-type: application/json' \
     -H 'idempotency-key: order-17' -d '{"from_id":1,"to_id":2,"amount_cents":2500}'
curl localhost:8080/health/ready
```

## What it shows

| Pattern | Where | Proved by |
|---|---|---|
| One transaction per transfer: debit, credit and record commit together | `Ledger::transfer` | `a_failed_credit_rolls_back_the_debit` |
| Check-and-debit in one conditional `UPDATE`, no read-modify-write | `move_money` | `concurrent_transfers_never_overdraw_or_lose_money` (80 transfers at once on 4 threads) |
| `BEGIN IMMEDIATE` so SQLite writers queue instead of failing with "database is locked" | `Ledger::transfer`, `Ledger::batch` | the same concurrency test |
| Idempotency keys: a retry returns the first result, money moves once | `Ledger::transfer` + `UNIQUE` column | `a_retried_request_with_the_same_idempotency_key_moves_money_once` |
| All-or-nothing batches | `BatchMode::Atomic` | `an_atomic_batch_is_all_or_nothing` |
| Savepoints: roll back only the failing items | `BatchMode::Independent` (`tx.begin()` inside a transaction) | `an_independent_batch_rolls_back_only_the_failing_items` |
| DI: the service is built from the pool at startup | `app()` (`provide_with`) | every test |
| A readiness check that audits a business invariant | `Ledger::check_books` | `readiness_checks_the_database_and_the_books` |
| A `CHECK (balance_cents >= 0)` constraint as the last line of defence | `migrations/` | |

## Endpoints

| | |
|---|---|
| `POST /accounts` | `{"owner", "opening_balance_cents"}` → `201` |
| `GET /accounts/{id}` | the balance |
| `GET /accounts/{id}/transfers` | every transfer in or out |
| `POST /transfers` | `{"from_id", "to_id", "amount_cents"}` and an optional `Idempotency-Key` header → `201`, `200` for a replay, `422 insufficient_funds`, `404 unknown_account`, `422 idempotency_key_reused` |
| `POST /transfers/batch` | `{"mode": "atomic" \| "independent", "transfers": [...]}` |
| `GET /health/ready` | database ping and books audit |

## Postgres

The SQL is portable (`$1` placeholders, `RETURNING`). For Postgres, enable
zapline's `postgres` feature, use `Pool<Postgres>`, and replace
`begin_with("BEGIN IMMEDIATE")` with `begin()`: the conditional `UPDATE`
takes a row lock, which is all the serialization a transfer needs.
