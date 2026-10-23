//! sqlx-transactions: a small double-entry ledger on zapline + sqlx.
//!
//! - **Transactions**: a transfer's debit, credit and record commit together
//!   (`ledger.rs`).
//! - **Savepoints**: `POST /transfers/batch` with `"mode": "independent"`
//!   rolls back only the failing items.
//! - **Idempotency keys**: a retried `POST /transfers` with the same
//!   `Idempotency-Key` header returns the first result; money moves once.
//! - **Dependency injection**: the pool comes from `App::database`, the
//!   `Ledger` service is built from it at startup with `provide_with`, and
//!   handlers receive it as `Dep<Ledger>`.
//! - **Health checks**: `/health/ready` pings the database and verifies that
//!   the books balance.

pub mod ledger;

use sqlx::SqlitePool;
use zapline::db::{Database, DbCheck};
use zapline::prelude::*;

pub use ledger::{
    Account, Batch, BatchItem, BatchMode, Ledger, NewAccount, NewTransfer, Outcome, Transfer,
};

/// The header that makes `POST /transfers` safe to retry.
pub const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// Builds the app on `db`. `main` passes a file database; tests pass
/// `Database::sqlite_memory()` or a temporary file.
pub fn app(db: Database) -> App {
    App::new()
        .database(db)
        // Runs after the pool exists (registration order), so it can use it.
        .provide_with(|res: Resources| async move {
            let pool: SqlitePool = res.require()?;
            Ok(Ledger::new(pool))
        })
        .plugin(
            Health::new()
                .check("database", DbCheck::new())
                .check("books", |res: Resources| async move {
                    res.dep::<Ledger>()?.check_books().await
                }),
        )
        .openapi(OpenApi::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
        .mount((
            open_account,
            get_account,
            account_transfers,
            transfer,
            batch,
        ))
}

/// Open an account with an opening balance.
#[post("/accounts", tag = "accounts")]
async fn open_account(
    ledger: Dep<Ledger>,
    Valid(Json(new)): Valid<Json<NewAccount>>,
) -> Result<(StatusCode, Json<Account>)> {
    Ok((StatusCode::CREATED, Json(ledger.open_account(&new).await?)))
}

/// One account and its balance.
#[get("/accounts/{id}", tag = "accounts")]
async fn get_account(ledger: Dep<Ledger>, Path(id): Path<i64>) -> Result<Json<Account>> {
    Ok(Json(ledger.account(id).await?))
}

/// Every transfer into or out of an account.
#[get("/accounts/{id}/transfers", tag = "accounts")]
async fn account_transfers(
    ledger: Dep<Ledger>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<Transfer>>> {
    Ok(Json(ledger.history(id).await?))
}

/// Move money. `201` for a new transfer, `200` for a replayed
/// `Idempotency-Key`.
#[post("/transfers", tag = "transfers")]
async fn transfer(
    ledger: Dep<Ledger>,
    headers: HeaderMap,
    Valid(Json(new)): Valid<Json<NewTransfer>>,
) -> Result<(StatusCode, Json<Transfer>)> {
    let key = idempotency_key(&headers)?;
    Ok(match ledger.transfer(&new, key).await? {
        Outcome::Created(t) => (StatusCode::CREATED, Json(t)),
        Outcome::Replayed(t) => (StatusCode::OK, Json(t)),
    })
}

/// Several transfers in one transaction: `atomic` (all or none) or
/// `independent` (each in its own savepoint).
#[post("/transfers/batch", tag = "transfers")]
async fn batch(
    ledger: Dep<Ledger>,
    Valid(Json(batch)): Valid<Json<Batch>>,
) -> Result<Json<Vec<BatchItem>>> {
    Ok(Json(ledger.batch(&batch).await?))
}

fn idempotency_key(headers: &HeaderMap) -> Result<Option<&str>> {
    let Some(value) = headers.get(IDEMPOTENCY_KEY) else {
        return Ok(None);
    };
    match value.to_str() {
        Ok(key) if !key.is_empty() && key.len() <= 255 => Ok(Some(key)),
        _ => Err(
            Error::bad_request("Idempotency-Key must be 1 to 255 visible ASCII characters")
                .with_code("invalid_idempotency_key"),
        ),
    }
}
