//! HTTP/3 over QUIC (**experimental**, feature `http3`).
//!
//! Next to HTTPS on TCP, the server listens on a UDP port and serves the
//! same router over QUIC with [quinn](https://docs.rs/quinn) and
//! [h3](https://docs.rs/h3). Handlers, extractors and middleware are shared:
//! a request is a request, whatever the transport. HTTP/3 needs TLS
//! ([`App::tls`]), and reuses its certificate.
//!
//! ```no_run
//! use zapline::prelude::*;
//! use zapline::http3::Http3Config;
//! use zapline::tls::TlsConfig;
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .get("/", || async { "hello over TCP and QUIC" })
//!         .tls(TlsConfig::from_pem_files("cert.pem", "key.pem")?)
//!         .http3(Http3Config::new()) // UDP 8443, next to TCP 8443
//!         .run("0.0.0.0:8443")
//! }
//! ```
//!
//! Browsers discover HTTP/3 through the `Alt-Svc` header: every HTTP/1.1
//! and HTTP/2 response then carries `Alt-Svc: h3=":8443"; ma=86400`
//! (unless the handler set its own), and the client switches to QUIC on its
//! next connection. The header is added by a middleware that is installed
//! only when HTTP/3 is configured, so apps without it pay nothing. Turn it
//! off with [`Http3Config::advertise`].
//!
//! Requests are marked [`Version::HTTP_3`](http::Version::HTTP_3) and
//! [`ConnInfo::secure`](crate::ConnInfo::secure). Request bodies stream
//! (`BodyStream`, `Json<T>`, the body limit all work as usual), and so do
//! responses (SSE, `StreamBody`, trailers).
//!
//! # Experimental
//!
//! The `h3` crate is pre-1.0, and so is this integration. Not supported yet:
//! WebSockets over HTTP/3 (RFC 9220; WebSockets work over HTTP/1.1 and
//! HTTP/2), 0-RTT (early data can be replayed, so it stays off), and
//! [`App::run_unix`] (HTTP/3 needs a UDP port). With [`App::serve`] the UDP
//! socket binds to the listener's address. `max_connections` counts QUIC
//! connections separately from TCP ones, and connections over the limit are
//! refused (QUIC has no kernel backlog), so clients fall back to TCP.
//!
//! [`App::serve`]: crate::App::serve
//! [`App::run_unix`]: crate::App::run_unix

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use http::header::ALT_SVC;
use http::{HeaderValue, Version};

use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::App;

/// HTTP/3 settings. Pass to [`App::http3`].
///
/// ```
/// use std::time::Duration;
/// use zapline::http3::Http3Config;
///
/// let h3 = Http3Config::new()
///     .port(443)                                   // UDP port (default: the TCP port)
///     .alt_svc_max_age(Duration::from_secs(3600))  // how long clients remember it
///     .max_concurrent_streams(256)
///     .max_idle_timeout(Duration::from_secs(60));
/// ```
#[derive(Clone, Debug)]
pub struct Http3Config {
    pub(crate) port: Option<u16>,
    pub(crate) advertise: bool,
    pub(crate) alt_svc_max_age: Duration,
    pub(crate) max_concurrent_streams: u32,
    pub(crate) max_idle_timeout: Duration,
}

impl Default for Http3Config {
    fn default() -> Self {
        Http3Config {
            port: None,
            advertise: true,
            alt_svc_max_age: Duration::from_secs(86_400),
            max_concurrent_streams: 100,
            max_idle_timeout: Duration::from_secs(30),
        }
    }
}

impl Http3Config {
    /// The defaults: the TCP port, advertised with `Alt-Svc` for a day,
    /// 100 concurrent requests per connection, 30s idle timeout.
    ///
    /// ```
    /// let h3 = zapline::http3::Http3Config::new();
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// The UDP port to listen on. Default: the same port as TCP (`0` there
    /// means the port the TCP listener got).
    ///
    /// ```
    /// let h3 = zapline::http3::Http3Config::new().port(4433);
    /// ```
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Add `Alt-Svc` to HTTP/1.1 and HTTP/2 responses so clients find
    /// HTTP/3 (default `true`). Turn it off when a proxy or CDN in front
    /// advertises it for you.
    ///
    /// ```
    /// let h3 = zapline::http3::Http3Config::new().advertise(false);
    /// ```
    pub fn advertise(mut self, enabled: bool) -> Self {
        self.advertise = enabled;
        self
    }

    /// The `ma` (max age) of the `Alt-Svc` advertisement: how long clients
    /// may keep using HTTP/3 without seeing it again. Default: 24 hours.
    ///
    /// ```
    /// use std::time::Duration;
    /// let h3 = zapline::http3::Http3Config::new().alt_svc_max_age(Duration::from_secs(600));
    /// ```
    pub fn alt_svc_max_age(mut self, max_age: Duration) -> Self {
        self.alt_svc_max_age = max_age;
        self
    }

    /// Maximum concurrent requests (bidirectional streams) per connection.
    /// Default: 100. At least 1.
    ///
    /// ```
    /// let h3 = zapline::http3::Http3Config::new().max_concurrent_streams(500);
    /// ```
    pub fn max_concurrent_streams(mut self, n: u32) -> Self {
        self.max_concurrent_streams = n.max(1);
        self
    }

    /// Close connections that exchange no packets for this long (QUIC's
    /// `max_idle_timeout`; the lower of the two peers' values wins).
    /// Default: 30s.
    ///
    /// ```
    /// use std::time::Duration;
    /// let h3 = zapline::http3::Http3Config::new().max_idle_timeout(Duration::from_secs(120));
    /// ```
    pub fn max_idle_timeout(mut self, timeout: Duration) -> Self {
        self.max_idle_timeout = timeout;
        self
    }

    /// The `Alt-Svc` value for `port`.
    pub(crate) fn alt_svc(&self, port: u16) -> Option<HeaderValue> {
        let max_age = self.alt_svc_max_age.as_secs();
        HeaderValue::from_str(&format!("h3=\":{port}\"; ma={max_age}")).ok()
    }
}

impl App {
    /// Serve HTTP/3 over QUIC next to HTTPS (**experimental**, feature
    /// `http3`; see [`crate::http3`]). Requires [`App::tls`]; `run` fails
    /// with `InvalidInput` without it.
    ///
    /// ```no_run
    /// use zapline::prelude::*;
    /// use zapline::http3::Http3Config;
    /// use zapline::tls::TlsConfig;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// App::new()
    ///     .get("/", || async { "h3" })
    ///     .tls(TlsConfig::from_pem_files("cert.pem", "key.pem")?)
    ///     .http3(Http3Config::new())
    ///     .run("0.0.0.0:8443")
    /// # }
    /// ```
    pub fn http3(mut self, config: Http3Config) -> Self {
        self.config.http3 = Some(config);
        self
    }
}

/// Where the server publishes the `Alt-Svc` value once the UDP port is
/// bound.
pub(crate) type AltSvcSlot = Arc<OnceLock<HeaderValue>>;

/// Install the `Alt-Svc` middleware when HTTP/3 is configured and
/// advertised. Runs once, before the app is prepared; returns the slot the
/// server fills in.
pub(crate) fn install(app: &mut App) -> Option<AltSvcSlot> {
    if !app.config.http3.as_ref().is_some_and(|h3| h3.advertise) {
        return None;
    }
    let slot = AltSvcSlot::default();
    // Outermost, so responses from every other middleware get it too.
    app.middleware
        .insert(0, Arc::new(AdvertiseH3(slot.clone())));
    Some(slot)
}

/// Adds `Alt-Svc` to responses sent over HTTP/1.1 and HTTP/2.
struct AdvertiseH3(AltSvcSlot);

impl Middleware for AdvertiseH3 {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        if req.head().version == Version::HTTP_3 {
            return next.run(req);
        }
        let slot = self.0.clone();
        Box::pin(async move {
            let mut res = next.run(req).await;
            if let Some(value) = slot.get() {
                if !res.headers().contains_key(ALT_SVC) {
                    res.headers_mut().insert(ALT_SVC, value.clone());
                }
            }
            res
        })
    }
}
