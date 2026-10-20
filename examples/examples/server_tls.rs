//! HTTPS with HTTP/2 (negotiated with ALPN `h2`) and HTTP/1.1.
//!
//! Point it at a PEM certificate chain and private key, as arguments or via
//! `TLS_CERT` / `TLS_KEY`. For local testing, create a self-signed pair:
//!
//!     openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
//!         -days 7 -subj /CN=localhost -keyout key.pem -out cert.pem
//!     cargo run -p zapline-examples --example server_tls -- cert.pem key.pem
//!
//!     curl -k https://localhost:8443/              # HTTP/2 (curl prefers h2)
//!     curl -k --http1.1 https://localhost:8443/    # HTTP/1.1
//!
//! In production, use your CA-issued chain (e.g. Let's Encrypt's
//! `fullchain.pem` and `privkey.pem`).

use std::time::Duration;

use zapline::prelude::*;
use zapline::tls::TlsConfig;

async fn hello(conn: ConnInfo, req: Request) -> String {
    format!(
        "hello from {:?}, secure={} (via {:?})\n",
        conn.remote_addr,
        conn.secure,
        req.head().version
    )
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let cert = args
        .next()
        .or_else(|| std::env::var("TLS_CERT").ok())
        .unwrap_or_else(|| "cert.pem".to_owned());
    let key = args
        .next()
        .or_else(|| std::env::var("TLS_KEY").ok())
        .unwrap_or_else(|| "key.pem".to_owned());

    let tls = TlsConfig::from_pem_files(&cert, &key).inspect_err(|e| {
        eprintln!("{e}\nusage: server_tls <cert.pem> <key.pem> (see the example's docs)");
    })?;

    let mut app = App::new()
        .get("/", hello)
        .tls(tls)
        .shutdown_timeout(Duration::from_secs(15));
    app.server_config_mut().tls_handshake_timeout = Duration::from_secs(5);
    app.run("0.0.0.0:8443")
}
