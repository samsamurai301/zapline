//! The ledger service: every money movement is one database transaction.
//!
//! Three rules keep the books correct under concurrency, retries and partial
//! failure:
//!
//! 1. **One transaction per request.** The debit, the credit and the
//!    `transfers` row commit together or not at all. An early `return` (or
//!    `?`) drops the transaction, and sqlx rolls it back.
//! 2. **No read-modify-write.** The debit is a conditional
//!    `UPDATE .. WHERE balance_cents >= amount`: the database checks and
//!    changes the balance in one step, so two concurrent transfers can't both
//!    spend the same money.
//! 3. **Write transactions start with `BEGIN IMMEDIATE` on SQLite.** It takes
//!    the write lock up front, so concurrent writers queue on the busy
//!    timeout instead of failing with "database is locked" when two deferred
//!    transactions both try to upgrade. (On Postgres, use plain `begin()`:
//!    the conditional `UPDATE` locks the row.)

use sqlx::{Connection, SqliteConnection, SqlitePool};
use zapline::prelude::*;

/// An account, as the API returns it.
#[model]
#[derive(sqlx::FromRow, PartialEq)]
pub struct Account {
    pub id: i64,
    pub owner: String,
    pub balance_cents: i64,
}

/// Body of `POST /accounts`.
#[model]
pub struct NewAccount {
    #[validate(length(min = 1, max = 100))]
    pub owner: String,
    #[serde(default)]
    #[validate(range(min = 0))]
    pub opening_balance_cents: i64,
}

/// A committed transfer.
#[model]
#[derive(sqlx::FromRow, PartialEq)]
pub struct Transfer {
    pub id: i64,
    pub from_id: i64,
    pub to_id: i64,
    pub amount_cents: i64,
    pub idempotency_key: Option<String>,
    pub created_at: String,
}

/// Body of `POST /transfers`, and one item of a batch.
#[model]
pub struct NewTransfer {
    pub from_id: i64,
    pub to_id: i64,
    #[validate(range(min = 1))]
    pub amount_cents: i64,
}

/// How a batch treats a failing item.
#[model]
#[derive(Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BatchMode {
    /// All transfers or none: the first failure rolls back the whole batch.
    Atomic,
    /// Each transfer in its own savepoint: failures are rolled back and
    /// reported, the others are committed.
    Independent,
}

/// Body of `POST /transfers/batch`.
#[model]
pub struct Batch {
    pub mode: BatchMode,
    #[validate(length(min = 1, max = 100), nested)]
    pub transfers: Vec<NewTransfer>,
}

/// The result of one item of an `independent` batch.
#[model]
pub struct BatchItem {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer: Option<Transfer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ItemError>,
}

/// Why one item of a batch failed.
#[model]
pub struct ItemError {
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
}

/// What [`Ledger::transfer`] did.
#[derive(Debug)]
pub enum Outcome {
    /// The money moved.
    Created(Transfer),
    /// The idempotency key was seen before: this is the original transfer,
    /// and nothing moved this time.
    Replayed(Transfer),
}

/// The ledger service. Registered once with `App::provide_with` and
/// received in handlers as `Dep<Ledger>`.
#[derive(Clone)]
pub struct Ledger {
    pool: SqlitePool,
}

const ACCOUNT_COLUMNS: &str = "id, owner, balance_cents";
const TRANSFER_COLUMNS: &str = "id, from_id, to_id, amount_cents, idempotency_key, created_at";

impl Ledger {
    /// A ledger on `pool`.
    pub fn new(pool: SqlitePool) -> Self {
        Ledger { pool }
    }

    /// Opens an account. A taken `owner` is a `409 unique_violation`.
    pub async fn open_account(&self, new: &NewAccount) -> Result<Account> {
        let account = sqlx::query_as::<_, Account>(&format!(
            "INSERT INTO accounts (owner, opening_balance_cents, balance_cents) \
             VALUES ($1, $2, $2) RETURNING {ACCOUNT_COLUMNS}"
        ))
        .bind(&new.owner)
        .bind(new.opening_balance_cents)
        .fetch_one(&self.pool)
        .await?;
        Ok(account)
    }

    /// One account; an unknown id is a 404.
    pub async fn account(&self, id: i64) -> Result<Account> {
        let account = sqlx::query_as::<_, Account>(&format!(
            "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id = $1"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(account)
    }

    /// Every transfer into or out of account `id`, oldest first.
    pub async fn history(&self, id: i64) -> Result<Vec<Transfer>> {
        self.account(id).await?;
        let transfers = sqlx::query_as::<_, Transfer>(&format!(
            "SELECT {TRANSFER_COLUMNS} FROM transfers \
             WHERE from_id = $1 OR to_id = $1 ORDER BY id"
        ))
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(transfers)
    }

    /// Moves money between two accounts.
    ///
    /// With an idempotency key, a repeated request returns the original
    /// transfer ([`Outcome::Replayed`]) instead of moving the money again;
    /// the same key with a different body is a `422 idempotency_key_reused`.
    /// The lookup and the write happen in the same `BEGIN IMMEDIATE`
    /// transaction, so two concurrent retries can't both get through.
    pub async fn transfer(&self, transfer: &NewTransfer, key: Option<&str>) -> Result<Outcome> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = key {
            if let Some(previous) = find_by_key(&mut tx, key).await? {
                // Nothing was written: dropping `tx` just ends it.
                return replay(previous, transfer);
            }
        }
        let created = move_money(&mut tx, transfer, key).await?;
        tx.commit().await?;
        Ok(Outcome::Created(created))
    }

    /// Applies several transfers in one transaction.
    ///
    /// [`BatchMode::Atomic`] stops at the first failure and rolls everything
    /// back; the error says which item failed. [`BatchMode::Independent`]
    /// runs each item in a savepoint (a transaction inside the transaction):
    /// a failing item is rolled back to its savepoint and reported, and the
    /// rest commit together.
    pub async fn batch(&self, batch: &Batch) -> Result<Vec<BatchItem>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut items = Vec::with_capacity(batch.transfers.len());
        for (index, transfer) in batch.transfers.iter().enumerate() {
            match batch.mode {
                BatchMode::Atomic => {
                    let created = move_money(&mut tx, transfer, None)
                        .await
                        .map_err(|e| at_index(index, e))?;
                    items.push(BatchItem::ok(index, created));
                }
                BatchMode::Independent => {
                    // `begin` on a transaction issues `SAVEPOINT`; `commit`
                    // releases it and `rollback` undoes only this item.
                    let mut savepoint = tx.begin().await?;
                    match move_money(&mut savepoint, transfer, None).await {
                        Ok(created) => {
                            savepoint.commit().await?;
                            items.push(BatchItem::ok(index, created));
                        }
                        Err(e) => {
                            savepoint.rollback().await?;
                            items.push(BatchItem::failed(index, &e));
                        }
                    }
                }
            }
        }
        tx.commit().await?;
        Ok(items)
    }

    /// Readiness check: money is neither created nor destroyed. The sum of
    /// all balances must equal the sum of all opening balances.
    pub async fn check_books(&self) -> Result<()> {
        let (drift,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(balance_cents) - SUM(opening_balance_cents), 0) FROM accounts",
        )
        .fetch_one(&self.pool)
        .await?;
        if drift != 0 {
            return Err(Error::service_unavailable(format!(
                "the books are off by {drift} cents"
            )));
        }
        Ok(())
    }
}

/// The debit, the credit and the record of one transfer, on a connection
/// that is inside a transaction (or a savepoint). Any error leaves the
/// caller to roll back.
async fn move_money(
    conn: &mut SqliteConnection,
    transfer: &NewTransfer,
    key: Option<&str>,
) -> Result<Transfer> {
    let NewTransfer {
        from_id,
        to_id,
        amount_cents,
    } = *transfer;
    if from_id == to_id {
        return Err(
            Error::bad_request("cannot transfer to the same account").with_code("same_account")
        );
    }

    // Check and debit in one statement: no window for a concurrent spend.
    let debited = sqlx::query(
        "UPDATE accounts SET balance_cents = balance_cents - $1 \
         WHERE id = $2 AND balance_cents >= $1",
    )
    .bind(amount_cents)
    .bind(from_id)
    .execute(&mut *conn)
    .await?;
    if debited.rows_affected() == 0 {
        let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM accounts WHERE id = $1")
            .bind(from_id)
            .fetch_optional(&mut *conn)
            .await?;
        return Err(match exists {
            None => Error::not_found().with_code("unknown_account"),
            Some(_) => Error::unprocessable(format!(
                "account {from_id} has less than {amount_cents} cents"
            ))
            .with_code("insufficient_funds"),
        });
    }

    // If this fails, the debit above is rolled back with the transaction.
    let credited =
        sqlx::query("UPDATE accounts SET balance_cents = balance_cents + $1 WHERE id = $2")
            .bind(amount_cents)
            .bind(to_id)
            .execute(&mut *conn)
            .await?;
    if credited.rows_affected() == 0 {
        return Err(Error::not_found().with_code("unknown_account"));
    }

    let created = sqlx::query_as::<_, Transfer>(&format!(
        "INSERT INTO transfers (from_id, to_id, amount_cents, idempotency_key) \
         VALUES ($1, $2, $3, $4) RETURNING {TRANSFER_COLUMNS}"
    ))
    .bind(from_id)
    .bind(to_id)
    .bind(amount_cents)
    .bind(key)
    .fetch_one(&mut *conn)
    .await?;
    Ok(created)
}

async fn find_by_key(conn: &mut SqliteConnection, key: &str) -> Result<Option<Transfer>> {
    let found = sqlx::query_as::<_, Transfer>(&format!(
        "SELECT {TRANSFER_COLUMNS} FROM transfers WHERE idempotency_key = $1"
    ))
    .bind(key)
    .fetch_optional(conn)
    .await?;
    Ok(found)
}

fn replay(previous: Transfer, request: &NewTransfer) -> Result<Outcome> {
    let same = previous.from_id == request.from_id
        && previous.to_id == request.to_id
        && previous.amount_cents == request.amount_cents;
    if !same {
        return Err(Error::unprocessable(
            "this idempotency key was already used for a different transfer",
        )
        .with_code("idempotency_key_reused"));
    }
    Ok(Outcome::Replayed(previous))
}

/// Prefixes an error with the batch item it came from, keeping its status
/// and code.
fn at_index(index: usize, e: Error) -> Error {
    let out = Error::new(e.status(), format!("transfers[{index}]: {}", e.message()));
    match e.code() {
        Some(code) => out.with_code(code.to_owned()),
        None => out,
    }
}

impl BatchItem {
    fn ok(index: usize, transfer: Transfer) -> Self {
        BatchItem {
            index,
            transfer: Some(transfer),
            error: None,
        }
    }

    fn failed(index: usize, e: &Error) -> Self {
        BatchItem {
            index,
            transfer: None,
            error: Some(ItemError {
                status: e.status().as_u16(),
                code: e.code().map(str::to_owned),
                message: e.message().to_owned(),
            }),
        }
    }
}
