# Observability: tracing, metrics, health

Three plugins give you logs, metrics and probes. Each is one line, and they work together:

```rust
use std::time::Duration;
use zapline::health::Health;
use zapline::metrics::Metrics;
use zapline::prelude::*;
use zapline::telemetry::{self, Trace};

fn main() -> std::io::Result<()> {
    telemetry::init();                         // RUST_LOG-aware subscriber

    App::new()
        .get("/users/{id}", |Path(id): Path<u64>| async move { format!("user {id}") })
        .plugin(Trace::new().skip_paths(["/health/live", "/metrics"]))   // a span + one event per request
        .plugin(Metrics::new())                                          // GET /metrics (Prometheus)
        .plugin(Health::new().timeout(Duration::from_secs(1)))           // /health/live, /health/ready, /health
        .run("0.0.0.0:8080")
}
```

| Plugin | Needs | Serves |
|---|---|---|
| `telemetry::init()`, `Telemetry`, `Trace` | Cargo feature `tracing` (on by default; adds `tracing` and `tracing-subscriber`) | nothing: logs and spans |
| `Metrics` | nothing (always compiled, no extra dependencies) | `GET /metrics` |
| `Health` | nothing | `GET /health/live`, `/health/ready`, `/health` |

## Logging with `Telemetry`

`telemetry::init()` installs a global subscriber: it honors `RUST_LOG` (default `info`), prints pretty output on a terminal and compact output otherwise, and does nothing if a subscriber is already installed. For production, choose the format and level explicitly:

```rust
use zapline::telemetry::Telemetry;

fn main() {
    Telemetry::new()
        .json()                    // newline-delimited JSON for log pipelines
        .level("info")             // used when RUST_LOG is unset
        .with_target(false)
        .init();
}
```

| Method | Effect |
|---|---|
| `.format(Format)` / `.json()` / `.pretty()` / `.compact()` | Output format. `Format::Auto` (the default) uses `ZAPLINE_LOG_FORMAT` (`json`, `pretty`, `compact`, `full`) if set, otherwise pretty on a terminal and compact elsewhere. |
| `.level(level)` | Default level when `RUST_LOG` is unset (`"debug"`, `Level::WARN`, ...) |
| `.env_filter(directives)` | Explicit directives such as `"info,my_app=debug,hyper=warn"`. Takes precedence over `RUST_LOG`. |
| `.env_var(name)` | Read the directives from this variable instead of `RUST_LOG` |
| `.with_target(bool)` | Show the event target (default `true`) |
| `.ansi(bool)` | Force colors on or off. By default they are on for terminals unless `NO_COLOR` is set, and never for JSON. |
| `.with_thread_ids(bool)`, `.with_thread_names(bool)`, `.with_file_line(bool)` | Extra fields |
| `.writer(w)` | Write somewhere other than stdout, such as `std::io::stderr` or a non-blocking file appender |
| `.init()` | Installs the subscriber. Does nothing if one exists. Panics on invalid filter directives, which is a startup configuration error. |
| `.try_init()` | The same, returning `Result<(), TelemetryError>` (`AlreadyInitialized` or `InvalidFilter`). It also bridges `log` records. |
| `.into_dispatch()` | Builds the subscriber without installing it, for `tracing::dispatcher::with_default` in tests |

The filter is `env_filter(..)`, else `RUST_LOG` when non-empty, else `level(..)`, else `info`.

Log from handlers with the re-exported macros (`zapline::telemetry::{debug, info, warn, error, trace}`), or depend on `tracing` directly. Events emitted inside a request are nested in its span, so they carry the route and the request ID:

```rust
use zapline::prelude::*;
use zapline::telemetry::{info, warn};

#[post("/orders")]
async fn create_order(Json(sku): Json<String>) -> Result<StatusCode> {
    if sku.is_empty() {
        warn!("rejected an empty sku");
        return Err(Error::unprocessable("sku must not be empty").with_code("empty_sku"));
    }
    info!(%sku, "order created");
    Ok(StatusCode::CREATED)
}
```

## Request tracing with `Trace`

`Trace` creates one `request` span per request and one completion event. The span records:

| Field | Value |
|---|---|
| `http.method` | `GET`, `POST`, ... |
| `http.route` | The route template (`/users/{id}`), or `unmatched` for requests that matched no route |
| `url.path` | The raw path |
| `client.address` | The peer IP address |
| `request_id` | The `x-request-id` request header, or else the response header (as set by [`SetRequestId`](hardening.md)) |
| `http.status_code`, `latency_ms` | Recorded when the response is ready |

The completion event (`request completed`) is `INFO` below 400, `WARN` for 4xx and `ERROR` for 5xx. Its target is `zapline::request`, so `RUST_LOG=zapline::request=warn` quiets the access log without touching the rest. This is what a JSON line looks like (a single line in reality):

```json
{"timestamp":"2026-10-20T20:36:25.390523Z","level":"INFO","message":"request completed",
 "http.status_code":200,"latency_ms":0.262,"target":"zapline::request",
 "span":{"client.address":"127.0.0.1","http.method":"GET","http.route":"/users/{id}",
         "request_id":"abc-123","url.path":"/users/42","name":"request"},"spans":[]}
```

There are three ways to install it:

| Install | Sees | `http.route` |
|---|---|---|
| `.plugin(Trace::new())` (recommended) | Every request, including 404s | The route template, or `unmatched` |
| `.wrap(Trace::new())` | Every request, including 404s | Not recorded (routing happens later) |
| `.wrap_route(Trace::new())` | Matched routes only | The route template |

The plugin form adds a global middleware at the position of the `.plugin(..)` call plus a tiny route middleware that records the template. Put it after `SetRequestId` so the ID is known, and outside `CatchPanic` so panics are logged as `500`s (see the [production stack](hardening.md#a-production-stack)).

| Method | Effect |
|---|---|
| `.level(Level)` | Level of the span and of success events (default `INFO`) |
| `.level_for(StatusClass, Level)` | Level of the completion event for `Success`, `ClientError` or `ServerError` responses |
| `.skip_paths(paths)` | Request paths or route templates to skip entirely, such as probes and scrapes. A trailing `*` matches a prefix: `"/static/*"`. |
| `.request_id_header(name)` | Header carrying the request ID (default `x-request-id`) |
| `.client_ip_header(name)` | Take `client.address` from the first entry of this header (for example `x-forwarded-for`). Only behind a proxy that sets it, since clients can forge it. |

```rust
use zapline::prelude::*;
use zapline::telemetry::{Level, StatusClass, Trace};

fn app() -> App {
    App::new().get("/", || async { "hi" }).plugin(
        Trace::new()
            .level(Level::DEBUG)                                // spans and success events at debug
            .level_for(StatusClass::ClientError, Level::INFO)
            .skip_paths(["/health/live", "/metrics", "/static/*"]),
    )
}
```

When the subscriber filters out every level `Trace` could emit, it costs one level comparison per request. The simpler `Logger` middleware prints one line per request to stderr and needs no setup; prefer `Trace` in services.

## Traces to OpenTelemetry (OTLP)

With the `otlp` feature, spans go to any OpenTelemetry collector or backend that accepts OTLP/HTTP (Jaeger, Grafana Tempo, Honeycomb, Datadog, the OpenTelemetry Collector...), with no OpenTelemetry SDK in your dependencies. Set the standard variable and that's it:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 OTEL_SERVICE_NAME=shop cargo run
```

`telemetry::init()` (and any `Telemetry`) picks it up. Or configure it in code:

```rust
Telemetry::new()
    .otlp("http://localhost:4318") // or the full .../v1/traces URL
    .service_name("shop")
    .init();
```

| Variable | Use |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Collector base URL; traces go to `{url}/v1/traces` |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Full traces URL (wins over the above) |
| `OTEL_EXPORTER_OTLP_HEADERS` | `key=value,key2=value2`, e.g. an API key (values may be percent-encoded) |
| `OTEL_SERVICE_NAME` | `service.name` (default: the executable's name) |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, `deployment.environment=prod,...` |

What you get:

- Each request [`Trace`](#request-tracing-with-trace) records becomes a server span named like `GET /users/{id}`, with `http.request.method`, `http.route`, `url.path`, `http.response.status_code` and `client.address`. 5xx responses are marked as errors.
- Your own `tracing` spans (`info_span!("load_user")`, `#[instrument]`) become child spans, and log events inside them become span events.
- A request with a W3C `traceparent` header continues that trace, so a call from another traced service shows up as one trace.

Spans are exported from a background thread in batches of up to 512, at least every second. If the collector is down, a warning is printed once and spans beyond 8,192 waiting are dropped, so the app never blocks or grows without bound. Only the spans your log filter enables are exported: with `RUST_LOG=warn`, request spans (`INFO`) are filtered out too.

## Metrics

`Metrics` instruments every route and serves a Prometheus scrape endpoint. Install it with `.plugin(Metrics::new())`, then `curl localhost:8080/metrics`:

```text
# HELP http_requests_total Total number of HTTP requests.
# TYPE http_requests_total counter
http_requests_total{method="GET",route="/users/{id}",status="200"} 1
http_requests_total{method="GET",route="unmatched",status="404"} 1
# HELP http_request_duration_seconds HTTP request latency in seconds.
# TYPE http_request_duration_seconds histogram
http_request_duration_seconds_bucket{method="GET",route="/users/{id}",le="0.005"} 1
...
http_request_duration_seconds_bucket{method="GET",route="/users/{id}",le="+Inf"} 1
http_request_duration_seconds_sum{method="GET",route="/users/{id}"} 0.000038321
http_request_duration_seconds_count{method="GET",route="/users/{id}"} 1
# HELP http_requests_in_flight HTTP requests currently being served.
# TYPE http_requests_in_flight gauge
http_requests_in_flight 1
# HELP process_resident_memory_bytes Resident memory size in bytes.
# TYPE process_resident_memory_bytes gauge
process_resident_memory_bytes 7487488
```

| Series | Type | Labels |
|---|---|---|
| `http_requests_total` | counter | `method`, `route`, `status` |
| `http_request_duration_seconds` | histogram | `method`, `route` |
| `http_requests_in_flight` | gauge | |
| `process_start_time_seconds` | gauge | |
| `process_cpu_seconds_total`, `process_resident_memory_bytes`, `process_virtual_memory_bytes`, `process_open_fds`, `process_max_fds` | | Linux only. Read from `/proc` at scrape time, so they cost nothing between scrapes. |

`route` is always the route **template** (`/users/{id}`), never the raw path, so cardinality is bounded by your route table. Requests that match no route are counted with `route="unmatched"`, and non-standard methods are reported as `_OTHER`.

The hot path is built for thread-per-core: per-route series are created at startup, counters are striped across cache lines, and recording a request takes no locks and does not hash strings.

| Method | Effect |
|---|---|
| `.path(path)` | Scrape endpoint path (default `/metrics`) |
| `.endpoint(false)` | Don't register the endpoint. Expose the metrics yourself with `MetricsHandle`. |
| `.prefix(prefix)` | Metric name prefix (default `http`, giving `http_requests_total`). `"api"` gives `api_requests_total`. |
| `.buckets([..])` | Latency histogram upper bounds in seconds (default 5 ms to 10 s). Must be finite and strictly increasing, or the app panics at startup. |
| `.exclude_paths(paths)` | Route templates (or, for unmatched requests, raw paths) not to record. A trailing `*` matches a prefix. |
| `.registry(Registry)` | Render this registry's custom metrics instead of the global one |
| `.track_unmatched(false)` | Don't count unmatched requests. This avoids one global middleware and keeps the router's no-middleware fast path for 404s. |
| `.process_metrics(false)` | Leave out the `process_*` series |

Install `Metrics` after `App::error_handler` and any middleware that replaces responses, so it sees the router's own 404 and 405 responses.

### Custom metrics

The global `metrics::registry()` is rendered after the HTTP and process metrics. Register counters, gauges and histograms once and keep the handle for hot paths. Asking for the same name, type and labels again returns the existing metric, so lazy registration is safe:

```rust
use std::time::Duration;
use zapline::metrics::{self, Counter, Gauge, Histogram};
use zapline::prelude::*;

#[derive(Clone)]
struct ShopMetrics {
    orders: Counter,
    stock: Gauge,
    checkout: Histogram,
}

impl ShopMetrics {
    fn new() -> Self {
        let registry = metrics::registry();
        ShopMetrics {
            orders: registry.counter("shop_orders_total", "Orders placed.", &["sku"]),
            stock: registry.gauge("shop_stock_units", "Units in stock.", &[]),
            checkout: registry.histogram_with_buckets(
                "shop_checkout_seconds",
                "Checkout duration.",
                &[],
                &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5],
            ),
        }
    }
}

async fn order(State(m): State<ShopMetrics>, Json(sku): Json<String>) -> StatusCode {
    let _timer = m.checkout.start_timer();      // observes the elapsed time when dropped
    m.orders.with_labels(&[&sku]).inc();         // one series per label combination
    m.stock.dec();
    tokio::time::sleep(Duration::from_millis(1)).await;
    StatusCode::CREATED
}

fn app() -> App {
    App::new().state(ShopMetrics::new()).post("/orders", order)
}
```

| Type | Methods |
|---|---|
| `Counter` | `inc()`, `add(n)`, `get()` |
| `Gauge` | `set(v)`, `add(v)`, `sub(v)`, `inc()`, `dec()`, `get()` |
| `Histogram` | `observe(v)`, `start_timer()` (a guard that observes on drop; `.stop()` returns the seconds, `.discard()` skips it), `count()`, `sum()` |
| any of them | `with_labels(&["value", ..])`: the series for these values, in label-name order |

Rules and safety nets:

- Registering a name again with a different type or different label names panics. So do invalid metric or label names (`le` is reserved for histograms).
- Each metric family holds at most 10,000 label combinations (`MAX_SERIES_PER_FAMILY`). Further combinations fold into a single series whose label values are all `_overflow`, so a label fed with user input cannot exhaust memory. Still, never use unbounded values (user IDs, raw paths) as labels.
- For values computed at scrape time (queue depths, pool sizes), implement `Collector` or pass a closure `Fn(&mut Encoder)` to `registry.register(..)`. It writes complete metric families with `out.header(name, help, kind)` and `out.sample(name, &[(label, value)], v)`.
- `Registry::new()` makes a private registry, which you hand to `Metrics::registry(..)`. `registry.render()` returns the text, which is useful in tests.

### Protecting the endpoint

`/metrics` is public by default and reveals route names and traffic. Don't expose it to the internet. Either block it at the ingress or serve it behind auth, using `MetricsHandle` (registered as state by the plugin):

```rust
use zapline::auth::BasicAuth;
use zapline::metrics::{Metrics, MetricsHandle};
use zapline::prelude::*;

async fn scrape(auth: BasicAuth, State(metrics): State<MetricsHandle>) -> Result<Response> {
    if !auth.verify("prometheus", "scrape-password") {
        return Err(auth.reject());
    }
    Ok(metrics.into_response())
}

fn app() -> App {
    App::new()
        .plugin(Metrics::new().endpoint(false))
        .get("/admin/metrics", scrape)
}
```

## Health checks

`Health` serves the three endpoints Kubernetes and load balancers expect:

| Endpoint | Behavior |
|---|---|
| `GET /health/live` | Always `200 {"status":"up"}`. Runs no checks. If this fails, the process is wedged. |
| `GET /health/ready` | Runs every check concurrently, each with a timeout. `200` when all critical checks pass (`up`, or `degraded` when a non-critical check fails), `503` otherwise or while the app is marked not ready. |
| `GET /health` | The readiness report plus `info` and `uptime_seconds` |

```rust
use std::time::Duration;
use zapline::health::Health;
use zapline::prelude::*;

#[derive(Clone)]
struct Db;

impl Db {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
}

fn app() -> App {
    App::new().state(Db).plugin(
        Health::new()
            .info("orders", env!("CARGO_PKG_VERSION"))
            .check("db", |res: Resources| async move { res.require::<Db>()?.ping().await })
            .check("cache", |_res: Resources| async move { Ok(()) })
            .critical(false)                       // a failing cache degrades, but stays ready
            .timeout(Duration::from_secs(1)),
    )
}
```

With the cache failing, `GET /health/ready` answers `200` (the `error` field appears only with `.show_errors(true)`):

```json
{"status":"degraded","checks":{"db":{"status":"up","duration_ms":0.006},"cache":{"status":"down","duration_ms":52.576,"error":"cache unreachable"}},"info":{"name":"orders","version":"1.2.3"}}
```

A failing critical check gives `{"status":"down", ...}` with `503`. Responses carry `Cache-Control: no-store`.

| Method | Effect |
|---|---|
| `.check(name, check)` | Adds a named readiness check, critical by default. Panics on a duplicate name. |
| `.critical(bool)` | Whether the **most recently added** check is critical. A failing non-critical check reports `degraded` and keeps the app ready. |
| `.timeout(duration)` | Per-check timeout (default 2 s). A timed-out check is reported as down: `timed out after 1s`. |
| `.info(name, version)` | Static service info in the reports |
| `.details(bool)` | Include per-check results: name, status and duration (default `true`). When it's off, only the overall `status` is exposed. |
| `.show_errors(bool)` | Include each failing check's error text (default `false`). Error text can reveal internal hostnames and library messages, so enable it only on endpoints that aren't publicly reachable. Failures are always logged. |
| `.cache_for(ttl)` | Reuse the last readiness result for this long, so frequent probes don't hammer dependencies |
| `.live_path(p)`, `.ready_path(p)`, `.path(p)` | Change the paths. `""` disables an endpoint. |
| `.state(HealthState)` | Use an existing `HealthState` |

Each check receives `Resources`, so it can reach state, providers (`res.dep::<T>()`) and anything a startup hook registered. A check that returns an error, times out or panics is reported as down; the request itself still succeeds. For reusable checks, implement the `HealthCheck` trait on a struct:

```rust
use zapline::health::HealthCheck;
use zapline::{Error, Resources, Result};

struct DiskSpace {
    min_free_bytes: u64,
}

impl HealthCheck for DiskSpace {
    async fn check(&self, _res: Resources) -> Result<()> {
        let free = 10 * 1024 * 1024 * 1024u64; // e.g. from statvfs
        if free < self.min_free_bytes {
            return Err(Error::service_unavailable("low disk space"));
        }
        Ok(())
    }
}
```

### Readiness toggling

`HealthState` is registered as state by the plugin. While `set_ready(false)` is in effect, `/health/ready` answers `503 {"status":"down","reason":"not ready"}` without running checks, and liveness is unaffected. Use it to stay out of rotation during warm-up or before a deploy:

```rust
use zapline::health::{Health, HealthState};
use zapline::prelude::*;

#[post("/admin/drain")]
async fn drain(State(health): State<HealthState>) -> &'static str {
    health.set_ready(false);
    "draining"
}

fn app() -> App {
    App::new()
        .plugin(Health::new())
        .mount(drain)
        .on_startup(|res: Resources| async move {
            // Not ready until caches are warm; liveness stays up.
            let health = res.require::<HealthState>()?;
            health.set_ready(false);
            tokio::spawn(async move {
                // ... warm caches ...
                health.set_ready(true);
            });
            Ok(())
        })
}
```

## Kubernetes and Prometheus

Point the probes at the health plugin and let Prometheus scrape `/metrics`:

```yaml
containers:
  - name: orders
    ports: [{ containerPort: 8080 }]
    livenessProbe:
      httpGet: { path: /health/live, port: 8080 }
      periodSeconds: 10
    readinessProbe:
      httpGet: { path: /health/ready, port: 8080 }
      periodSeconds: 5
    startupProbe:
      httpGet: { path: /health/live, port: 8080 }
      failureThreshold: 30
      periodSeconds: 2
```

```yaml
# prometheus.yml
scrape_configs:
  - job_name: orders
    metrics_path: /metrics
    kubernetes_sd_configs: [{ role: pod }]
```

Useful queries:

```text
# requests per second by route
sum by (route) (rate(http_requests_total[5m]))

# 5xx error ratio
sum(rate(http_requests_total{status=~"5.."}[5m])) / sum(rate(http_requests_total[5m]))

# 99th percentile latency per route
histogram_quantile(0.99, sum by (le, route) (rate(http_request_duration_seconds_bucket[5m])))
```

Notes for a clean setup:

- Don't make the liveness probe depend on dependencies: a database outage would restart every pod. Dependencies belong in readiness.
- Keep probes and scrapes out of the request log with `Trace::skip_paths([..])`, and out of the metrics with `Metrics::exclude_paths(["/health/*"])`.
- Rate limiting and authentication middleware installed with `App::wrap_route` also cover the plugin routes. Exempt probes, or install such middleware per group; see [Hardening middleware](hardening.md).
- The plugin routes appear in the OpenAPI document like any other route; see [OpenAPI & interactive docs](openapi.md).
- The drain and termination settings that go with these probes are in [Deployment](deployment.md).
