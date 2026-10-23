//! Telling a process supervisor when the server is ready, for zero-downtime
//! deploys: start the new version next to the old one on the same port
//! (`SO_REUSEPORT`), wait until it says it is ready, then stop the old one.
//!
//! - With `NOTIFY_SOCKET` set (systemd `Type=notify`), [`App::run`] sends
//!   `READY=1` once the startup hooks have run and the listeners are bound,
//!   and `STOPPING=1` when it starts draining. `systemctl start` returns
//!   only then, and fails if the process exits first.
//! - With [`ENV_READY_PATH`] set, the server first requests that path from
//!   itself, in-process (so it is this process that answers, not whichever
//!   instance owns the port), until it answers below 400. Startup fails if
//!   it never does within [`ENV_READY_TIMEOUT`] seconds (default 30), and
//!   the port is never bound.
//! - With [`ENV_REUSE_PORT`] set, the multi-threaded runtime also binds with
//!   `SO_REUSEPORT` (the default thread-per-core runtime always does on
//!   Linux), so two instances can share the port during the swap.
//!
//! `zapline ship` sets all three in the systemd unit it installs.
//!
//! [`App::run`]: crate::App::run

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::app::AppInner;
use crate::body::ReqBody;
use crate::request::ConnInfo;

/// A path the server requests from itself before it binds its port and
/// reports ready (e.g. `/health/ready`). Unset: no self-check.
///
/// ```
/// assert_eq!(zapline::server::ENV_READY_PATH, "ZAPLINE_READY_PATH");
/// ```
pub const ENV_READY_PATH: &str = "ZAPLINE_READY_PATH";

/// Seconds the [`ENV_READY_PATH`] self-check may take (default 30).
///
/// ```
/// assert_eq!(zapline::server::ENV_READY_TIMEOUT, "ZAPLINE_READY_TIMEOUT");
/// ```
pub const ENV_READY_TIMEOUT: &str = "ZAPLINE_READY_TIMEOUT";

/// When set (to anything but `0`), every TCP listener uses `SO_REUSEPORT`,
/// so a new instance can bind the port while the old one still serves.
///
/// ```
/// assert_eq!(zapline::server::ENV_REUSE_PORT, "ZAPLINE_REUSE_PORT");
/// ```
pub const ENV_REUSE_PORT: &str = "ZAPLINE_REUSE_PORT";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_millis(200);

/// Whether [`ENV_REUSE_PORT`] asks for `SO_REUSEPORT`.
pub(crate) fn reuse_port_requested() -> bool {
    std::env::var_os(ENV_REUSE_PORT).is_some_and(|v| v != "0" && !v.is_empty())
}

/// Request [`ENV_READY_PATH`] from `app` until it answers below 400.
pub(crate) async fn self_check(app: &AppInner) -> io::Result<()> {
    let Ok(path) = std::env::var(ENV_READY_PATH) else {
        return Ok(());
    };
    let path = path.trim();
    if path.is_empty() {
        return Ok(());
    }
    if !path.starts_with('/') {
        return Err(io::Error::other(format!(
            "{ENV_READY_PATH} must be a path starting with `/`, got {path:?}"
        )));
    }
    let timeout = std::env::var(ENV_READY_TIMEOUT)
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s > 0.0)
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs_f64);
    let deadline = Instant::now() + timeout;
    loop {
        let req = http::Request::get(path)
            .header(http::header::HOST, "localhost")
            .header(http::header::USER_AGENT, "zapline-ready")
            .body(ReqBody::Full(bytes::Bytes::new()))
            .map_err(|e| io::Error::other(format!("{ENV_READY_PATH}: {e}")))?;
        let conn = ConnInfo {
            remote_addr: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            local_addr: None,
            secure: false,
        };
        let status = match app.handle(req, conn).await {
            Ok(res) => res.status(),
            Err(never) => match never {},
        };
        if status.as_u16() < 400 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "readiness check failed: GET {path} answered {status} for {timeout:?}"
            )));
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Send `state` (`READY=1`, `STOPPING=1`) to the service manager when
/// `NOTIFY_SOCKET` is set. Best effort: a supervisor that doesn't listen
/// must not stop the server.
#[cfg(unix)]
pub(crate) fn notify(state: &str) {
    use std::os::unix::net::UnixDatagram;

    let Some(target) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(socket) = UnixDatagram::unbound() else {
        return;
    };
    let bytes = target.as_encoded_bytes();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(name) = bytes.strip_prefix(b"@") {
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt;
        if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name) {
            let _ = socket.send_to_addr(state.as_bytes(), &addr);
        }
        return;
    }
    if bytes.first() == Some(&b'/') {
        let _ = socket.send_to(state.as_bytes(), &target);
    }
}

#[cfg(not(unix))]
pub(crate) fn notify(_state: &str) {}
