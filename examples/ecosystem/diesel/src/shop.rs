//! The shop service: products, stock and orders with Diesel.

use diesel::prelude::*;
use diesel::sqlite::Sqlite;
use serde::Serialize;
use zapline::prelude::*;

use crate::db::{DbError, DbPool};
use crate::schema::{order_lines, orders, products};

/// A product and how many are in stock.
#[model]
#[derive(Queryable, Selectable, Identifiable, PartialEq)]
#[diesel(table_name = products, check_for_backend(Sqlite))]
pub struct Product {
    pub id: i32,
    pub sku: String,
    pub name: String,
    pub price_cents: i64,
    pub stock: i32,
}

/// Body of `POST /products`.
#[model]
#[derive(Insertable)]
#[diesel(table_name = products)]
pub struct NewProduct {
    #[validate(length(min = 1, max = 64))]
    pub sku: String,
    #[validate(length(min = 1, max = 200))]
    pub name: String,
    #[validate(range(min = 0))]
    pub price_cents: i64,
    #[serde(default)]
    #[validate(range(min = 0))]
    pub stock: i32,
}

/// Body of `POST /products/{id}/restock`.
#[model]
pub struct Restock {
    #[validate(range(min = 1, max = 1_000_000))]
    pub quantity: i32,
}

/// Body of `POST /orders`.
#[model]
pub struct NewOrder {
    #[validate(length(min = 1, max = 200))]
    pub customer: String,
    #[validate(length(min = 1, max = 50), nested)]
    pub lines: Vec<NewLine>,
}

/// One line of a new order.
#[model]
pub struct NewLine {
    pub product_id: i32,
    #[validate(range(min = 1, max = 10_000))]
    pub quantity: i32,
}

/// An order with its lines.
#[model]
#[derive(PartialEq)]
pub struct Order {
    pub id: i32,
    pub customer: String,
    pub total_cents: i64,
    pub created_at: String,
    pub lines: Vec<OrderLine>,
}

/// One line of an order, at the price it was bought for.
#[model]
#[derive(Queryable, Selectable, Identifiable, Associations, PartialEq)]
#[diesel(table_name = order_lines, belongs_to(OrderRow, foreign_key = order_id), check_for_backend(Sqlite))]
pub struct OrderLine {
    #[serde(skip)]
    pub id: i32,
    #[serde(skip)]
    pub order_id: i32,
    pub product_id: i32,
    pub quantity: i32,
    pub unit_price_cents: i64,
}

#[derive(Queryable, Selectable, Identifiable)]
#[diesel(table_name = orders, check_for_backend(Sqlite))]
struct OrderRow {
    id: i32,
    customer: String,
    total_cents: i64,
    created_at: String,
}

#[derive(Insertable)]
#[diesel(table_name = order_lines)]
struct NewLineRow {
    order_id: i32,
    product_id: i32,
    quantity: i32,
    unit_price_cents: i64,
}

/// Details of a `409 out_of_stock`.
#[derive(Serialize)]
struct Shortage {
    product_id: i32,
    available: i32,
    requested: i32,
}

/// The shop service, registered with `provide_with` and received as
/// `Dep<Shop>`.
#[derive(Clone)]
pub struct Shop {
    db: DbPool,
}

impl Shop {
    /// A service on `db`.
    pub fn new(db: DbPool) -> Self {
        Shop { db }
    }

    /// Adds a product. A taken SKU is a `409 unique_violation`.
    pub async fn create_product(&self, new: NewProduct) -> Result<Product> {
        self.db
            .run(move |conn| {
                Ok(diesel::insert_into(products::table)
                    .values(&new)
                    .returning(Product::as_returning())
                    .get_result(conn)?)
            })
            .await
    }

    /// Every product, by id.
    pub async fn products(&self) -> Result<Vec<Product>> {
        self.db
            .run(|conn| {
                Ok(products::table
                    .order(products::id)
                    .select(Product::as_select())
                    .load(conn)?)
            })
            .await
    }

    /// One product; an unknown id is a 404.
    pub async fn product(&self, id: i32) -> Result<Product> {
        self.db
            .run(move |conn| {
                Ok(products::table
                    .find(id)
                    .select(Product::as_select())
                    .first(conn)?)
            })
            .await
    }

    /// Adds stock to a product.
    pub async fn restock(&self, id: i32, quantity: i32) -> Result<Product> {
        self.db
            .run(move |conn| {
                Ok(diesel::update(products::table.find(id))
                    .set(products::stock.eq(products::stock + quantity))
                    .returning(Product::as_returning())
                    .get_result(conn)?)
            })
            .await
    }

    /// Places an order: reserves the stock of every line and records the
    /// order, in one `BEGIN IMMEDIATE` transaction. If any line can't be
    /// filled, nothing is reserved and the answer is `409 out_of_stock`.
    ///
    /// The stock check and the decrement are one conditional `UPDATE`, so
    /// concurrent orders can never sell the same unit twice.
    pub async fn place_order(&self, order: NewOrder) -> Result<Order> {
        self.db
            .run(move |conn| {
                conn.immediate_transaction(|conn| {
                    let mut total = 0i64;
                    let mut lines = Vec::with_capacity(order.lines.len());
                    for line in &order.lines {
                        let reserved = diesel::update(
                            products::table
                                .find(line.product_id)
                                .filter(products::stock.ge(line.quantity)),
                        )
                        .set(products::stock.eq(products::stock - line.quantity))
                        .returning(products::price_cents)
                        .get_result::<i64>(conn)
                        .optional()?;
                        let Some(price) = reserved else {
                            return Err(shortage(conn, line));
                        };
                        total += price * i64::from(line.quantity);
                        lines.push(NewLineRow {
                            order_id: 0,
                            product_id: line.product_id,
                            quantity: line.quantity,
                            unit_price_cents: price,
                        });
                    }
                    let row: OrderRow = diesel::insert_into(orders::table)
                        .values((
                            orders::customer.eq(&order.customer),
                            orders::total_cents.eq(total),
                        ))
                        .returning(OrderRow::as_returning())
                        .get_result(conn)?;
                    for line in &mut lines {
                        line.order_id = row.id;
                    }
                    diesel::insert_into(order_lines::table)
                        .values(&lines)
                        .execute(conn)?;
                    load_lines(conn, row)
                })
            })
            .await
    }

    /// One order with its lines.
    pub async fn order(&self, id: i32) -> Result<Order> {
        self.db
            .run(move |conn| {
                let row: OrderRow = orders::table
                    .find(id)
                    .select(OrderRow::as_select())
                    .first(conn)?;
                load_lines(conn, row)
            })
            .await
    }

    /// Cancels an order: returns its stock and deletes it, in one
    /// transaction.
    pub async fn cancel_order(&self, id: i32) -> Result<()> {
        self.db
            .run(move |conn| {
                conn.immediate_transaction(|conn| {
                    let row: OrderRow = orders::table
                        .find(id)
                        .select(OrderRow::as_select())
                        .first(conn)?;
                    let lines = OrderLine::belonging_to(&row)
                        .select(OrderLine::as_select())
                        .load(conn)?;
                    for line in lines {
                        diesel::update(products::table.find(line.product_id))
                            .set(products::stock.eq(products::stock + line.quantity))
                            .execute(conn)?;
                    }
                    // The lines go with it: ON DELETE CASCADE.
                    diesel::delete(orders::table.find(id)).execute(conn)?;
                    Ok(())
                })
            })
            .await
    }

    /// Readiness check: a connection is free and answers.
    pub async fn ping(&self) -> Result<()> {
        self.db
            .run(|conn| {
                diesel::sql_query("SELECT 1").execute(conn)?;
                Ok(())
            })
            .await
            .map_err(|_| Error::service_unavailable("database did not answer"))
    }
}

/// Why a line couldn't be reserved: an unknown product (404) or too little
/// stock (409 with the numbers).
fn shortage(conn: &mut SqliteConnection, line: &NewLine) -> DbError {
    let available = products::table
        .find(line.product_id)
        .select(products::stock)
        .first::<i32>(conn)
        .optional();
    match available {
        Ok(Some(available)) => DbError::Rejected(
            Error::conflict(format!(
                "product {} has {available} in stock, {} requested",
                line.product_id, line.quantity
            ))
            .with_code("out_of_stock")
            .with_details(Shortage {
                product_id: line.product_id,
                available,
                requested: line.quantity,
            }),
        ),
        Ok(None) => DbError::Rejected(Error::not_found().with_code("unknown_product")),
        Err(e) => DbError::Diesel(e),
    }
}

fn load_lines(conn: &mut SqliteConnection, row: OrderRow) -> Result<Order, DbError> {
    let lines = OrderLine::belonging_to(&row)
        .select(OrderLine::as_select())
        .order(order_lines::id)
        .load(conn)?;
    Ok(Order {
        id: row.id,
        customer: row.customer,
        total_cents: row.total_cents,
        created_at: row.created_at,
        lines,
    })
}
