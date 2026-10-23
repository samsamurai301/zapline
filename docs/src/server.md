# Server: TLS, HTTP/2, HTTP/3, graceful shutdown

`App::run(addr)` starts a production-ready server with no extra configuration:

- **Thread-per-core.** One single-threaded Tokio runtime per CPU, each with its own `SO_REUSEPORT` listener. This is the default on Linux, Android and FreeBSD. Elsewhere, or with `.runtime(RuntimeMode::MultiThread)`, it uses one listener on a work-stealing runtime.
- **Graceful shutdown.** On SIGINT or SIGTERM (Ctrl-C on Windows) it stops accepting connections, lets in-flight requests finish, closes keep-alive connections after their next response (or after `drain_grace`, 1s, if idle), waits up to `shutdown_timeout` (default 30s), then runs your [shutdown hooks](lifecycle.md). A second signal forces an immediate stop.
- **HTTP/1.1** with keep-alive and pipelining. **HTTP/2** with the `http2` feature, using h2c prior knowledge and ALPN `h2` over TLS. Detection costs nothing for HTTP/1.1 connections. WebSockets work over both (RFC 8441 for HTTP/2).
- **HTTP/3** (experimental) over QUIC with the `http3` feature, next to HTTPS. See [below](#http3-experimental).

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
| `max_connections` | unlimited | Per worker. Accepting pauses at the limit. WebSockets keep their slot after the upgrade. |
| `max_upgraded` | none | Per worker. A separate limit for WebSockets; upgrades beyond it get `503`. |
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

## WebSockets and the server

An upgraded WebSocket stays a connection as far as the server is concerned:

- **It keeps counting against `max_connections`.** The socket takes over the slot of the HTTP connection it came from, with no gap in between, so a new connection can't slip in when hyper hands the socket over. At the limit, an upgrade is never refused: it doesn't need a new slot.
- **Or against its own limit.** Set `max_upgraded` to count WebSockets separately. The HTTP connection's slot is then freed at the upgrade, and an upgrade beyond `max_upgraded` is answered with `503` (`ws_too_many_connections`, `Retry-After: 1`) instead of `101`, so your callback never runs.
- **Over HTTP/2 (RFC 8441)** a socket is one stream on a connection that stays open, so it counts once more against `max_connections` on top of that connection (still never refused), or against `max_upgraded` like any other socket, with the same `503` when it is full.
- **Graceful shutdown waits for it.** When the drain starts, each socket sends a close frame with code `1001` (`close_code::AWAY`, "server shutting down") the next time your callback reads from or writes to it. A `recv` loop then sees the close handshake and ends; a `send` afterwards fails with `ws_closed`. The server waits for the callbacks to return, up to `shutdown_timeout`, and then drops the ones still running (the socket closes). Upgrades that arrive during the drain get `503` (`ws_shutting_down`).

```rust
use std::time::Duration;
use zapline::prelude::*;
use zapline::ws::{WebSocket, WebSocketUpgrade};

async fn feed(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket: WebSocket| async move {
        // Ends on shutdown: recv() completes the 1001 close handshake.
        while let Some(Ok(msg)) = socket.recv().await {
            if msg.is_text() && socket.send(msg).await.is_err() {
                break;
            }
        }
        // Cleanup here still runs before the server returns.
    })
}

fn main() -> std::io::Result<()> {
    let mut app = App::new()
        .get("/feed", feed)
        .max_connections(2_000)                  // HTTP connections per worker
        .shutdown_timeout(Duration::from_secs(10));
    app.server_config_mut().max_upgraded = Some(50_000); // WebSockets per worker
    app.run("0.0.0.0:8080")
}
```

A callback that waits on something other than the socket (a channel, a timer) sees the shutdown when it next touches the socket. Put `socket.recv()` in your `select!` so the close frame goes out right away. None of this costs plain HTTP connections anything: the accounting is set up in the handler, only for upgrades.

## HTTP/3 (experimental)

> **Experimental.** The `http3` feature is not part of `full`, and its API may change in any release. It is built on the pre-1.0 [`h3`](https://docs.rs/h3) crate and [quinn](https://docs.rs/quinn).

With the `http3` feature, the server also listens on a UDP port and serves the same router over QUIC. Routes, extractors, middleware, state and error handling are shared with HTTP/1.1 and HTTP/2; nothing in your handlers changes. HTTP/3 needs TLS and reuses the certificate from `App::tls`.

```rust
use zapline::prelude::*;
use zapline::http3::Http3Config;
use zapline::tls::TlsConfig;

fn main() -> std::io::Result<()> {
    App::new()
        .get("/", || async { "hello over TCP and QUIC" })
        .tls(TlsConfig::from_pem_files("cert.pem", "key.pem")?)
        .http3(Http3Config::new())   // UDP 8443, next to TCP 8443
        .run("0.0.0.0:8443")
}
```

Browsers don't try HTTP/3 first. They learn about it from the `Alt-Svc` header, and switch to QUIC on their next connection. zapline adds `Alt-Svc: h3=":8443"; ma=86400` to every HTTP/1.1 and HTTP/2 response (including errors and responses from middleware) unless the handler set its own. The header comes from a middleware that is installed only when HTTP/3 is configured, so apps without it pay nothing per request.

| `Http3Config` method | Default | Purpose |
|---|---|---|
| `port(u16)` | the TCP port | UDP port to listen on |
| `advertise(bool)` | `true` | Send `Alt-Svc` on HTTP/1.1 and HTTP/2 responses. Turn it off when a CDN advertises for you. |
| `alt_svc_max_age(Duration)` | 24h | The `ma` parameter: how long clients remember the advertisement |
| `max_concurrent_streams(u32)` | 100 | Concurrent requests per QUIC connection |
| `max_idle_timeout(Duration)` | 30s | Close connections with no traffic for this long |

How it behaves:

- Requests have `version() == Version::HTTP_3` and `ConnInfo::secure == true`. Request bodies stream, so the body limit, `Json<T>` and `BodyStream` work as over TCP. Responses stream too (SSE, `StreamBody`, trailers), and `Content-Length` is set when the body size is known.
- `max_header_size` limits the HTTP/3 header section, `header_read_timeout` bounds how long a request stream may take to send its headers, `tls_handshake_timeout` bounds the QUIC handshake, and `max_connections` caps QUIC connections separately from TCP ones. QUIC has no kernel backlog, so connections over the limit are refused and clients fall back to TCP.
- Graceful shutdown sends `GOAWAY`: requests in flight finish, new ones are refused, and the UDP socket stops accepting connections. After `shutdown_timeout` the remaining connections are closed.
- With `App::run`, HTTP/3 runs on its own runtime (`workers` threads, named `zapline-h3`). With `App::serve`, it runs on your runtime and binds UDP on the listener's address. `App::run_unix` with HTTP/3 fails with `InvalidInput`.
- The startup line shows it: `zapline listening on https://0.0.0.0:8443 (..., HTTP/1.1 + HTTP/2, TLS + HTTP/3 on UDP 0.0.0.0:8443)`.

Open UDP on the port in your firewall or load balancer. Many cloud load balancers only pass TCP; HTTP/3 then has to terminate at the edge, and you can use `advertise(false)`.

Not supported yet: WebSockets over HTTP/3 (RFC 9220; they work over HTTP/1.1 and HTTP/2), and 0-RTT (early data can be replayed, so it stays off).

## Known limitations

- With `pipeline_flush` on (the default, needed for pipelined throughput), hyper 1.12 closes the connection on a malformed or oversized request head without sending the 400/414/431 response. Responses to earlier pipelined requests that were still buffered on that connection are lost with it. Set `pipeline_flush = false` if you need those responses. zapline can't send them itself: hyper drops its write buffer, and only it knows what was in it (see the [upstream issue draft](upstream-issues.md#hyper-automatic-error-responses-are-dropped-with-pipeline_flush)). The test `hyper_drops_automatic_error_responses_with_pipeline_flush` fails once a hyper release fixes it.
- Individual HTTP/2 streams aren't tracked by the drain: an HTTP/2 connection gets a GOAWAY and closes when its streams finish. With `App::run` the rest end when the workers stop.
- WebSockets over HTTP/2 (RFC 8441) aren't supported.
