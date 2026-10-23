//! diesel-shop: products, stock and orders on zapline + Diesel.
//!
//! - **Blocking ORM, async server** (`db.rs`): queries run on tokio's
//!   blocking pool through `DbPool::run`, with r2d2 connections.
//! - **Embedded migrations**: `migrations/` is compiled into the binary and
//!   applied at startup.
//! - **Transactions** (`shop.rs`): placing an order reserves stock for every
//!   line in one `BEGIN IMMEDIATE` transaction; cancelling returns it.
//! - **Dependency injection**: the pool and the `Shop` service are built by
//!   startup factories; handlers receive `Dep<Shop>`.
//! - **Health**: `/health/ready` runs `SELECT 1` on a pooled connection.

pub mod db;
pub mod schema;
pub mod shop;

use zapline::prelude::*;

pub use db::{DbError, DbPool};
pub use shop::{NewLine, NewOrder, NewProduct, Order, OrderLine, Product, Restock, Shop};

/// Builds the app on the database at `url`: a file path such as `shop.db`,
/// or `:memory:` for tests.
pub fn app(url: impl Into<String>) -> App {
    let url = url.into();
    App::new()
        .provide_with(move |_res: Resources| async move { DbPool::open(&url, 8).await })
        .provide_with(|res: Resources| async move {
            let pool = res.dep::<DbPool>()?;
            Ok(Shop::new(DbPool::clone(&pool)))
        })
        .plugin(
            Health::new().check("database", |res: Resources| async move {
                res.dep::<Shop>()?.ping().await
            }),
        )
        .openapi(OpenApi::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
        .mount((
            create_product,
            list_products,
            get_product,
            restock,
            place_order,
            get_order,
            cancel_order,
        ))
}

/// Add a product.
#[post("/products", tag = "products")]
async fn create_product(
    shop: Dep<Shop>,
    Valid(Json(new)): Valid<Json<NewProduct>>,
) -> Result<(StatusCode, Json<Product>)> {
    Ok((StatusCode::CREATED, Json(shop.create_product(new).await?)))
}

/// Every product.
#[get("/products", tag = "products")]
async fn list_products(shop: Dep<Shop>) -> Result<Json<Vec<Product>>> {
    Ok(Json(shop.products().await?))
}

/// One product.
#[get("/products/{id}", tag = "products")]
async fn get_product(shop: Dep<Shop>, Path(id): Path<i32>) -> Result<Json<Product>> {
    Ok(Json(shop.product(id).await?))
}

/// Add stock.
#[post("/products/{id}/restock", tag = "products")]
async fn restock(
    shop: Dep<Shop>,
    Path(id): Path<i32>,
    Valid(Json(body)): Valid<Json<Restock>>,
) -> Result<Json<Product>> {
    Ok(Json(shop.restock(id, body.quantity).await?))
}

/// Place an order. `409 out_of_stock` if any line can't be filled.
#[post("/orders", tag = "orders")]
async fn place_order(
    shop: Dep<Shop>,
    Valid(Json(order)): Valid<Json<NewOrder>>,
) -> Result<(StatusCode, Json<Order>)> {
    Ok((StatusCode::CREATED, Json(shop.place_order(order).await?)))
}

/// One order with its lines.
#[get("/orders/{id}", tag = "orders")]
async fn get_order(shop: Dep<Shop>, Path(id): Path<i32>) -> Result<Json<Order>> {
    Ok(Json(shop.order(id).await?))
}

/// Cancel an order and return its stock.
#[delete("/orders/{id}", tag = "orders")]
async fn cancel_order(shop: Dep<Shop>, Path(id): Path<i32>) -> Result<StatusCode> {
    shop.cancel_order(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
