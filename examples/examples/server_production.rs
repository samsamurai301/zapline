//! A production-style server: graceful shutdown, connection limits,
//! timeouts and HTTP/2 (h2c) next to HTTP/1.1.
//!
//!     cargo run --release -p zapline-examples --example server_production
//!
//!     curl localhost:8080/                          # HTTP/1.1
//!     curl --http2-prior-knowledge localhost:8080/  # HTTP/2 without TLS
//!
//! Graceful shutdown: start a slow request, then press Ctrl-C (or send
//! SIGTERM). The server stops accepting, the slow request still completes,
//! then the shutdown hooks run. Press Ctrl-C twice to skip the wait.
//!
//!     curl localhost:8080/slow & sleep 0.5; kill -TERM $(pgrep -f server_production)
//!
//! `MAX_UPTIME_SECS=60` additionally stops the server after a minute, showing
//! how to combine your own shutdown trigger with the OS signals.

use std::time::Duration;

use zapline::prelude::*;
use zapline::server::{shutdown_signal, RuntimeMode};

async fn hello(req: Request) -> String {
    format!("hello over {:?}\n", req.head().version)
}

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_secs(3)).await;
    "finished, even if shutdown started meanwhile\n"
}

fn main() -> std::io::Result<()> {
    let mut app = App::new()
        .get("/", hello)
        .get("/slow", slow)
        .on_shutdown(|_res: Resources| async {
            eprintln!("shutdown hooks: closing pools, flushing metrics...");
        })
        // Every knob has a sensible default; these are the common ones.
        .runtime(RuntimeMode::PerCore)
        .http2(true)
        .max_connections(10_000)
        .shutdown_timeout(Duration::from_secs(10));

    // The rest of `ServerConfig` is available for fine-tuning.
    let config = app.server_config_mut();
    config.header_read_timeout = Some(Duration::from_secs(5));
    config.max_header_size = Some(16 * 1024);
    config.http2_max_concurrent_streams = Some(100);
    config.http2_keep_alive_interval = Some(Duration::from_secs(30));

    let max_uptime = std::env::var("MAX_UPTIME_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs);

    app.run_with_shutdown("0.0.0.0:8080", async move {
        match max_uptime {
            Some(uptime) => tokio::select! {
                _ = shutdown_signal() => {}
                _ = tokio::time::sleep(uptime) => eprintln!("max uptime reached"),
            },
            None => shutdown_signal().await,
        }
    })
}
