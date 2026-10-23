//! End-to-end tests on SQLite: in memory, or a temporary file where several
//! connections are needed.

use diesel_shop::{app, Order, Product};
use serde_json::{json, Value};
use zapline::test::TestClient;

async fn product(client: &TestClient, sku: &str, price_cents: i64, stock: i32) -> Product {
    let res = client
        .post("/products")
        .json(&json!({ "sku": sku, "name": format!("{sku} name"), "price_cents": price_cents, "stock": stock }))
        .send()
        .await;
    res.assert_status(201);
    res.json()
}

async fn stock(client: &TestClient, id: i32) -> i32 {
    client
        .get(&format!("/products/{id}"))
        .send()
        .await
        .json::<Product>()
        .stock
}

#[tokio::test]
async fn an_order_reserves_stock_at_the_current_price() {
    let client = app(":memory:").test();
    let pen = product(&client, "pen", 150, 10).await;
    let pad = product(&client, "pad", 400, 3).await;

    let res = client
        .post("/orders")
        .json(&json!({ "customer": "ada", "lines": [
            { "product_id": pen.id, "quantity": 4 },
            { "product_id": pad.id, "quantity": 1 },
        ]}))
        .send()
        .await;
    res.assert_status(201);
    let order: Order = res.json();
    assert_eq!(order.total_cents, 4 * 150 + 400);
    assert_eq!(order.lines.len(), 2);
    assert_eq!(
        (stock(&client, pen.id).await, stock(&client, pad.id).await),
        (6, 2)
    );

    let loaded: Order = client
        .get(&format!("/orders/{}", order.id))
        .send()
        .await
        .json();
    assert_eq!(loaded, order);
}

#[tokio::test]
async fn one_unfillable_line_rolls_back_the_whole_order() {
    let client = app(":memory:").test();
    let pen = product(&client, "pen", 150, 10).await;
    let pad = product(&client, "pad", 400, 1).await;

    // The pen line is reserved first, then the pad line fails: the pen's
    // stock is given back by the rollback.
    let res = client
        .post("/orders")
        .json(&json!({ "customer": "ada", "lines": [
            { "product_id": pen.id, "quantity": 4 },
            { "product_id": pad.id, "quantity": 2 },
        ]}))
        .send()
        .await;
    res.assert_status(409);
    let error: Value = res.json();
    assert_eq!(error["code"], "out_of_stock");
    assert_eq!(
        error["details"],
        json!({ "product_id": pad.id, "available": 1, "requested": 2 })
    );
    assert_eq!(
        (stock(&client, pen.id).await, stock(&client, pad.id).await),
        (10, 1)
    );

    let res = client
        .post("/orders")
        .json(&json!({ "customer": "ada", "lines": [
            { "product_id": pen.id, "quantity": 1 },
            { "product_id": 999, "quantity": 1 },
        ]}))
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(res.json::<Value>()["code"], "unknown_product");
    assert_eq!(stock(&client, pen.id).await, 10);
}

#[tokio::test]
async fn cancelling_returns_the_stock() {
    let client = app(":memory:").test();
    let pen = product(&client, "pen", 150, 5).await;
    let order: Order = client
        .post("/orders")
        .json(&json!({ "customer": "ada", "lines": [{ "product_id": pen.id, "quantity": 5 }] }))
        .send()
        .await
        .json();
    assert_eq!(stock(&client, pen.id).await, 0);

    client
        .delete(&format!("/orders/{}", order.id))
        .send()
        .await
        .assert_status(204);
    assert_eq!(stock(&client, pen.id).await, 5);
    client
        .get(&format!("/orders/{}", order.id))
        .send()
        .await
        .assert_status(404);
    client
        .delete(&format!("/orders/{}", order.id))
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn products_are_validated_and_skus_unique() {
    let client = app(":memory:").test();
    let pen = product(&client, "pen", 150, 0).await;

    let dup = client
        .post("/products")
        .json(&json!({ "sku": "pen", "name": "another", "price_cents": 1 }))
        .send()
        .await;
    dup.assert_status(409);
    assert_eq!(dup.json::<Value>()["code"], "unique_violation");
    client
        .post("/products")
        .json(&json!({ "sku": "x", "name": "x", "price_cents": -1 }))
        .send()
        .await
        .assert_status(422);

    let restocked: Product = client
        .post(&format!("/products/{}/restock", pen.id))
        .json(&json!({ "quantity": 7 }))
        .send()
        .await
        .json();
    assert_eq!(restocked.stock, 7);
    client
        .post("/products/999/restock")
        .json(&json!({ "quantity": 1 }))
        .send()
        .await
        .assert_status(404);
    let all: Vec<Product> = client.get("/products").send().await.json();
    assert_eq!(all, vec![restocked]);
}

/// 30 customers race for 10 units over 8 pooled connections: exactly 10
/// orders succeed and the stock ends at zero, never below.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_orders_never_oversell() {
    let dir = tempfile::tempdir().unwrap();
    let client = app(dir.path().join("shop.db").display().to_string()).test();
    let pen = product(&client, "pen", 150, 10).await;

    let mut tasks = Vec::new();
    for i in 0..30 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            client
                .post("/orders")
                .json(&json!({ "customer": format!("c{i}"), "lines": [{ "product_id": pen.id, "quantity": 1 }] }))
                .send()
                .await
                .status()
                .as_u16()
        }));
    }
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap());
    }
    assert_eq!(
        statuses.iter().filter(|&&s| s == 201).count(),
        10,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|&&s| s == 409).count(),
        20,
        "{statuses:?}"
    );
    assert_eq!(stock(&client, pen.id).await, 0);
}

#[tokio::test]
async fn readiness_and_docs() {
    let client = app(":memory:").test();
    let res = client.get("/health/ready").send().await;
    res.assert_status(200);
    assert_eq!(res.json::<Value>()["checks"]["database"]["status"], "up");

    let spec: Value = client.get("/openapi.json").send().await.json();
    for path in [
        "/products",
        "/products/{id}",
        "/products/{id}/restock",
        "/orders",
        "/orders/{id}",
    ] {
        assert!(spec["paths"][path].is_object(), "{path} missing");
    }
}

#[tokio::test]
#[should_panic(expected = "cannot open /nonexistent-dir/shop.db")]
async fn a_bad_database_path_stops_startup() {
    // `App::run` returns this error before binding the port; the test
    // client panics with it.
    app("/nonexistent-dir/shop.db").test().start().await;
}
