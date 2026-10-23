//! End-to-end tests: each test gets its own SQLite database.

use serde_json::{json, Value};
use sqlx_transactions::{app, Account, BatchItem, Transfer, IDEMPOTENCY_KEY};
use zapline::db::Database;
use zapline::test::TestClient;

async fn open(client: &TestClient, owner: &str, cents: i64) -> Account {
    let res = client
        .post("/accounts")
        .json(&json!({ "owner": owner, "opening_balance_cents": cents }))
        .send()
        .await;
    res.assert_status(201);
    res.json()
}

async fn balance(client: &TestClient, id: i64) -> i64 {
    let account: Account = client.get(&format!("/accounts/{id}")).send().await.json();
    account.balance_cents
}

async fn transfer(
    client: &TestClient,
    from: i64,
    to: i64,
    cents: i64,
) -> zapline::test::TestResponse {
    client
        .post("/transfers")
        .json(&json!({ "from_id": from, "to_id": to, "amount_cents": cents }))
        .send()
        .await
}

#[tokio::test]
async fn a_transfer_moves_money_and_is_recorded_on_both_accounts() {
    let client = app(Database::sqlite_memory()).test();
    let ada = open(&client, "ada", 10_000).await;
    let bob = open(&client, "bob", 0).await;

    let res = transfer(&client, ada.id, bob.id, 2_500).await;
    res.assert_status(201);
    let t: Transfer = res.json();
    assert_eq!(
        (t.from_id, t.to_id, t.amount_cents),
        (ada.id, bob.id, 2_500)
    );

    assert_eq!(balance(&client, ada.id).await, 7_500);
    assert_eq!(balance(&client, bob.id).await, 2_500);
    for id in [ada.id, bob.id] {
        let history: Vec<Transfer> = client
            .get(&format!("/accounts/{id}/transfers"))
            .send()
            .await
            .json();
        assert_eq!(history, vec![t.clone()]);
    }
}

#[tokio::test]
async fn insufficient_funds_changes_nothing() {
    let client = app(Database::sqlite_memory()).test();
    let ada = open(&client, "ada", 100).await;
    let bob = open(&client, "bob", 0).await;

    let res = transfer(&client, ada.id, bob.id, 101).await;
    res.assert_status(422);
    assert_eq!(res.json::<Value>()["code"], "insufficient_funds");
    assert_eq!(balance(&client, ada.id).await, 100);
    assert_eq!(balance(&client, bob.id).await, 0);
    let history: Vec<Transfer> = client
        .get(&format!("/accounts/{}/transfers", ada.id))
        .send()
        .await
        .json();
    assert!(history.is_empty());
}

#[tokio::test]
async fn a_failed_credit_rolls_back_the_debit() {
    let client = app(Database::sqlite_memory()).test();
    let ada = open(&client, "ada", 1_000).await;

    // The debit succeeds, then the credit finds no account 999: the whole
    // transaction is rolled back, so ada keeps her money.
    let res = transfer(&client, ada.id, 999, 300).await;
    res.assert_status(404);
    assert_eq!(res.json::<Value>()["code"], "unknown_account");
    assert_eq!(balance(&client, ada.id).await, 1_000);
}

#[tokio::test]
async fn bad_requests_are_rejected_before_touching_the_database() {
    let client = app(Database::sqlite_memory()).test();
    let ada = open(&client, "ada", 1_000).await;
    open(&client, "bob", 0).await;

    transfer(&client, ada.id, ada.id, 1)
        .await
        .assert_status(400);
    transfer(&client, ada.id, 2, 0).await.assert_status(422);
    client
        .post("/accounts")
        .json(&json!({ "owner": "ada" }))
        .send()
        .await
        .assert_status(409);
    client
        .post("/transfers")
        .header(IDEMPOTENCY_KEY, "x".repeat(256))
        .json(&json!({ "from_id": ada.id, "to_id": 2, "amount_cents": 1 }))
        .send()
        .await
        .assert_status(400);
    assert_eq!(balance(&client, ada.id).await, 1_000);
}

#[tokio::test]
async fn a_retried_request_with_the_same_idempotency_key_moves_money_once() {
    let client = app(Database::sqlite_memory()).test();
    let ada = open(&client, "ada", 1_000).await;
    let bob = open(&client, "bob", 0).await;
    let body = json!({ "from_id": ada.id, "to_id": bob.id, "amount_cents": 400 });

    let first = client
        .post("/transfers")
        .header(IDEMPOTENCY_KEY, "order-17")
        .json(&body)
        .send()
        .await;
    first.assert_status(201);
    let retry = client
        .post("/transfers")
        .header(IDEMPOTENCY_KEY, "order-17")
        .json(&body)
        .send()
        .await;
    retry.assert_status(200);
    assert_eq!(first.json::<Transfer>(), retry.json::<Transfer>());
    assert_eq!(balance(&client, ada.id).await, 600);

    // The same key for a different transfer is a client bug: refuse it.
    let reused = client
        .post("/transfers")
        .header(IDEMPOTENCY_KEY, "order-17")
        .json(&json!({ "from_id": ada.id, "to_id": bob.id, "amount_cents": 1 }))
        .send()
        .await;
    reused.assert_status(422);
    assert_eq!(reused.json::<Value>()["code"], "idempotency_key_reused");
    assert_eq!(balance(&client, ada.id).await, 600);
}

#[tokio::test]
async fn an_atomic_batch_is_all_or_nothing() {
    let client = app(Database::sqlite_memory()).test();
    let ada = open(&client, "ada", 1_000).await;
    let bob = open(&client, "bob", 0).await;

    let res = client
        .post("/transfers/batch")
        .json(&json!({ "mode": "atomic", "transfers": [
            { "from_id": ada.id, "to_id": bob.id, "amount_cents": 600 },
            { "from_id": ada.id, "to_id": bob.id, "amount_cents": 600 },
        ]}))
        .send()
        .await;
    res.assert_status(422);
    let error: Value = res.json();
    assert_eq!(error["code"], "insufficient_funds");
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .starts_with("transfers[1]:"),
        "{error}"
    );
    // The first item had succeeded inside the transaction; it is undone too.
    assert_eq!(balance(&client, ada.id).await, 1_000);
    assert_eq!(balance(&client, bob.id).await, 0);
}

#[tokio::test]
async fn an_independent_batch_rolls_back_only_the_failing_items() {
    let client = app(Database::sqlite_memory()).test();
    let ada = open(&client, "ada", 1_000).await;
    let bob = open(&client, "bob", 0).await;

    let res = client
        .post("/transfers/batch")
        .json(&json!({ "mode": "independent", "transfers": [
            { "from_id": ada.id, "to_id": bob.id, "amount_cents": 600 },
            { "from_id": ada.id, "to_id": bob.id, "amount_cents": 600 },
            { "from_id": ada.id, "to_id": 999, "amount_cents": 100 },
            { "from_id": ada.id, "to_id": bob.id, "amount_cents": 400 },
        ]}))
        .send()
        .await;
    res.assert_status(200);
    let items: Vec<BatchItem> = res.json();
    let failed: Vec<(usize, Option<String>)> = items
        .iter()
        .filter_map(|i| i.error.as_ref().map(|e| (i.index, e.code.clone())))
        .collect();
    assert_eq!(
        failed,
        vec![
            (1, Some("insufficient_funds".to_owned())),
            (2, Some("unknown_account".to_owned())),
        ]
    );
    assert!(items[0].transfer.is_some() && items[3].transfer.is_some());
    // Item 2's debit was rolled back to its savepoint, so item 3 could spend it.
    assert_eq!(balance(&client, ada.id).await, 0);
    assert_eq!(balance(&client, bob.id).await, 1_000);
}

/// Concurrency needs a real file: the server runs on several threads, and
/// writers queue on SQLite's busy timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_transfers_never_overdraw_or_lose_money() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("ledger.db").display()
    );
    let client = app(Database::sqlite(url)).test();
    let ada = open(&client, "ada", 1_000).await;
    let bob = open(&client, "bob", 1_000).await;

    // 40 transfers of 100 each way at once: every one either commits whole
    // or fails whole with 422; no balance ever goes negative.
    let mut tasks = Vec::new();
    for i in 0..80 {
        let client = client.clone();
        let (from, to) = if i % 2 == 0 {
            (ada.id, bob.id)
        } else {
            (bob.id, ada.id)
        };
        tasks.push(tokio::spawn(async move {
            transfer(&client, from, to, 100).await.status().as_u16()
        }));
    }
    let mut created = 0;
    for task in tasks {
        match task.await.unwrap() {
            201 => created += 1,
            422 => {}
            other => panic!("unexpected status {other}"),
        }
    }
    assert!(created > 0);

    let (a, b) = (
        balance(&client, ada.id).await,
        balance(&client, bob.id).await,
    );
    assert_eq!(a + b, 2_000, "money is conserved");
    assert!(a >= 0 && b >= 0);
    let history: Vec<Transfer> = client
        .get(&format!("/accounts/{}/transfers", ada.id))
        .send()
        .await
        .json();
    assert_eq!(history.len(), created);

    let ready = client.get("/health/ready").send().await;
    ready.assert_status(200);
}

#[tokio::test]
async fn readiness_checks_the_database_and_the_books() {
    let client = app(Database::sqlite_memory()).test();
    let ada = open(&client, "ada", 500).await;
    let bob = open(&client, "bob", 0).await;
    transfer(&client, ada.id, bob.id, 200)
        .await
        .assert_status(201);

    let res = client.get("/health/ready").send().await;
    res.assert_status(200);
    let report: Value = res.json();
    assert_eq!(report["checks"]["database"]["status"], "up", "{report}");
    assert_eq!(report["checks"]["books"]["status"], "up", "{report}");

    // Corrupt the books behind the service's back: readiness fails.
    let pool: sqlx::SqlitePool = client.resources().await.require().unwrap();
    sqlx::query("UPDATE accounts SET balance_cents = balance_cents + 1 WHERE id = $1")
        .bind(bob.id)
        .execute(&pool)
        .await
        .unwrap();
    let res = client.get("/health/ready").send().await;
    res.assert_status(503);
    assert_eq!(res.json::<Value>()["checks"]["books"]["status"], "down");
}

#[tokio::test]
async fn the_api_is_documented() {
    let client = app(Database::sqlite_memory()).test();
    let spec: Value = client.get("/openapi.json").send().await.json();
    for path in [
        "/accounts",
        "/accounts/{id}",
        "/transfers",
        "/transfers/batch",
    ] {
        assert!(spec["paths"][path].is_object(), "{path} missing");
    }
}
