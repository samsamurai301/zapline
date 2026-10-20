//! Observability: structured logs, request tracing, Prometheus metrics and
//! health checks for Kubernetes-style probes.
//!
//!     cargo run -p zapline-examples --example observability   # PORT=8080 by default
//!     curl -X POST localhost:8080/orders -H 'content-type: application/json' -d '{"sku":"tea","qty":2}'
//!     curl localhost:8080/orders/1
//!     curl localhost:8080/metrics
//!     curl localhost:8080/health/ready
//!     curl -X POST localhost:8080/admin/drain      # readiness -> 503
//!
//! JSON logs for production: `ZAPLINE_LOG_FORMAT=json RUST_LOG=info,zapline=debug`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zapline::health::{Health, HealthState};
use zapline::metrics::{self, Counter, Gauge, Metrics};
use zapline::prelude::*;
use zapline::telemetry::{self, info, warn, Trace};

#[derive(Deserialize)]
struct NewOrder {
    sku: String,
    qty: u32,
}

#[derive(Serialize, Clone)]
struct Order {
    id: u64,
    sku: String,
    qty: u32,
}

/// Business metrics, registered once in the global registry (which the
/// `Metrics` plugin renders after the HTTP and process metrics).
#[derive(Clone)]
struct ShopMetrics {
    orders: Counter,
    stock: Gauge,
}

impl ShopMetrics {
    fn new() -> Self {
        let r = metrics::registry();
        ShopMetrics {
            orders: r.counter("shop_orders_total", "Orders placed.", &["sku"]),
            stock: r.gauge("shop_stock_units", "Units in stock.", &[]),
        }
    }
}

#[derive(Clone, Default)]
struct Store {
    next_id: Arc<AtomicU64>,
    orders: Arc<std::sync::Mutex<Vec<Order>>>,
}

#[post("/orders")]
async fn create_order(
    State(store): State<Store>,
    State(m): State<ShopMetrics>,
    Json(new): Json<NewOrder>,
) -> Result<(StatusCode, Json<Order>)> {
    if new.qty == 0 {
        warn!(sku = %new.sku, "rejected empty order");
        return Err(Error::unprocessable("qty must be positive").with_code("empty_order"));
    }
    let id = store.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let order = Order {
        id,
        sku: new.sku,
        qty: new.qty,
    };
    store.orders.lock().unwrap().push(order.clone());
    m.orders.with_labels(&[&order.sku]).inc();
    m.stock.sub(order.qty as f64);
    info!(order = id, "order created"); // nested in the request span
    Ok((StatusCode::CREATED, Json(order)))
}

#[get("/orders/{id}")]
async fn get_order(State(store): State<Store>, Path(id): Path<u64>) -> Result<Json<Order>> {
    let orders = store.orders.lock().unwrap();
    orders
        .iter()
        .find(|o| o.id == id)
        .cloned()
        .map(Json)
        .ok_or_else(|| Error::not_found().with_code("order_not_found"))
}

/// Take the instance out of rotation (e.g. before a deploy).
#[post("/admin/drain")]
async fn drain(State(health): State<HealthState>) -> &'static str {
    health.set_ready(false);
    "draining"
}

fn main() -> std::io::Result<()> {
    telemetry::init();

    let shop = ShopMetrics::new();
    shop.stock.set(100.0);

    App::new()
        .state(Store::default())
        .state(shop)
        .mount((create_order, get_order, drain))
        // One span + one completion event per request (WARN for 4xx, ERROR for 5xx).
        .plugin(Trace::new().skip_paths(["/health/live", "/metrics"]))
        // http_requests_total, http_request_duration_seconds, process_*, shop_*.
        .plugin(Metrics::new().exclude_paths(["/health/*"]))
        .plugin(
            Health::new()
                .info("shop", env!("CARGO_PKG_VERSION"))
                .timeout(Duration::from_secs(1))
                .check("store", |res: Resources| async move {
                    let store = res.require::<Store>()?;
                    let _orders = store
                        .orders
                        .lock()
                        .map_err(|_| Error::internal("store poisoned"))?;
                    Ok(())
                })
                .check("payments", |_res: Resources| async move {
                    // A flaky dependency we can live without: reported as
                    // "degraded" but the instance stays ready.
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    Err(Error::service_unavailable("payments sandbox unreachable"))
                })
                .critical(false),
        )
        .run(format!(
            "0.0.0.0:{}",
            std::env::var("PORT").as_deref().unwrap_or("8080")
        ))
}
