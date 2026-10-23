# Server: TLS, HTTP/2, graceful shutdown

`App::run(addr)` starts a production-ready server with no extra configuration:

- **Thread-per-core.** One single-threaded Tokio runtime per CPU, each with its own `SO_REUSEPORT` listener. This is the default on Linux, Android and FreeBSD. Elsewhere, or with `.runtime(RuntimeMode::MultiThread)`, it uses one listener on a work-stealing runtime.
- **Graceful shutdown.** On SIGINT or SIGTERM (Ctrl-C on Windows) it stops accepting connections, lets in-flight requests finish, closes keep-alive connections after their next response (or after `drain_grace`, 1s, if idle), waits up to `shutdown_timeout` (default 30s), then runs your [shutdown hooks](lifecycle.md). A second signal forces an immediate stop.
- **HTTP/1.1** with keep-alive and pipelining. **HTTP/2** with the `http2` feature, using h2c prior knowledge and ALPN `h2` over TLS. Detection costs nothing for HTTP/1.1 connections.

```rust
use std::time::Duration;
use zapline::prelude::*;
use zapline::tls::TlsConfig;
use zapline::RuntimeMode;

fn main() -> std::io::Result<()> {
    App::new()
        .get("/", || async { "secure" })
        .tls(TlsConfig::from_pem_files("cert.pem", "key.pem")?)   // feature `tls`
        .http2(true)                                             // feature `http2`
        .workers(8)
        .max_connections(10_000)                                 // per worker
        .shutdown_timeout(Duration::from_secs(20))
        .runtime(RuntimeMode::PerCore)
        .run("0.0.0.0:8443")
}
```

## Custom shutdown and embedding

```rust
// Stop when a channel fires instead of on signals:
let (tx, rx) = tokio::sync::oneshot::channel::<()>();
app.run_with_shutdown("0.0.0.0:8080", async { let _ = rx.await; })?;

// Inside your own Tokio runtime:
let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
app.serve(listener).await?;                         // also stops on SIGINT/SIGTERM
app.serve_with_shutdown(listener, std::future::pending()).await?;  // no signal handling

// Unix domain socket (e.g. behind nginx):
app.run_unix("/run/my-app.sock")?;
```

## `ServerConfig`

For full control, use `App::server_config(..)` or `App::server_config_mut()`:

| Field | Default | Purpose |
|---|---|---|
| `workers` | CPU count | Worker threads |
| `runtime` | `PerCore` (Linux) | `PerCore` or `MultiThread` |
| `header_read_timeout` | 30s | Slowloris protection |
| `keep_alive` / `pipeline_flush` | true / true | HTTP/1.1 behavior |
| `max_connections` | unlimited | Per worker. Accepting pauses at the limit. |
| `max_header_size`, `max_headers`, `max_buf_size` | hyper defaults | Request head limits |
| `http2`, `http2_max_concurrent_streams`, `http2_keep_alive_interval` | true (feature on) | HTTP/2 tuning |
| `shutdown_timeout` | 30s | Drain deadline |
| `drain_grace` | 1s | How long an idle connection may still send a request once draining starts; its response says `connection: close` ([why](deployment.md#why-no-request-is-dropped)) |
| `tls_handshake_timeout` | 10s | Abandon slow handshakes |
| `backlog` | 4096 | Listen backlog |
| `banner` | true | Print the startup line |

Invalid combinations are rejected at startup with `ErrorKind::InvalidInput`.

## TLS details

`TlsConfig::from_pem_files`, `from_pem` or `from_rustls(Arc<rustls::ServerConfig>)` set up TLS. For SNI, mTLS or custom ciphers, build the rustls config yourself; `zapline::tls::rustls` is re-exported. The handshake runs in the connection task with a timeout, so slow clients never block accepting. Handlers can see `ConnInfo::secure == true`.

## Known limitations

- With `pipeline_flush` on (the default, needed for pipelined throughput), hyper 1.12 closes the connection on malformed or oversized request heads without sending the 400/431 response. Set `pipeline_flush = false` if you need those responses.
- Upgraded WebSocket connections and individual HTTP/2 streams aren't tracked by the drain. With `App::run` they end when the workers stop.
- A WebSocket connection gives back its `max_connections` slot when it upgrades, so long-lived sockets aren't counted against that limit. Bound them in your handler, for example with a semaphore in app state.
