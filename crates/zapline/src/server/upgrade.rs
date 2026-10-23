//! Accounting for upgraded (WebSocket) connections.
//!
//! hyper ends the HTTP connection future when it hands the socket to an
//! upgrade, which releases that connection's [`Slot`] and permit. So the
//! upgrade takes its own, from the handler, while the HTTP connection is
//! still alive: there is no moment where the socket is uncounted or
//! untracked by the drain. Nothing here touches plain HTTP connections, so
//! the request hot path is unchanged (see `server::conn`).
//!
//! Each accept loop puts an [`Upgrades`] in its worker's `Shared`, which is
//! how `WebSocketUpgrade` finds it without a field on `Request`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;

use super::limit::{Limit, Permit};
use super::shutdown::{GracefulShutdown, Signal, Slot, Watched, RUNNING};

/// The upgrade accounting of one accept loop.
#[derive(Clone)]
pub(crate) struct Upgrades {
    signal: Signal,
    limit: UpgradeLimit,
}

#[derive(Clone)]
enum UpgradeLimit {
    /// No limit.
    None,
    /// Count against `max_connections`. An HTTP/1.1 upgrade takes over the
    /// slot of the connection it came from, so it is never refused. An
    /// HTTP/2 WebSocket is one stream of a connection that stays open and
    /// keeps its slot, so it is not counted again (the streams of one
    /// connection are bounded by `http2_max_concurrent_streams`).
    Connections(Arc<Limit>),
    /// `max_upgraded`: a separate limit; upgrades beyond it are refused.
    Own(Arc<Limit>),
}

/// Why an upgrade was refused.
pub(crate) enum Refused {
    /// `ServerConfig::max_upgraded` upgraded connections are open.
    Full,
    /// The server is shutting down.
    ShuttingDown,
}

impl Upgrades {
    pub(crate) fn new(
        signal: Signal,
        connections: Option<Arc<Limit>>,
        max_upgraded: Option<usize>,
    ) -> Upgrades {
        let limit = match (max_upgraded, connections) {
            (Some(n), _) => UpgradeLimit::Own(Limit::new(n)),
            (None, Some(l)) => UpgradeLimit::Connections(l),
            (None, None) => UpgradeLimit::None,
        };
        Upgrades { signal, limit }
    }

    /// Count and track one upgrade. Call it while the HTTP connection is
    /// still alive (from the handler). `h2`: an HTTP/2 extended CONNECT
    /// stream (RFC 8441), whose connection keeps its own permit.
    pub(crate) fn admit(&self, h2: bool) -> Result<Admission, Refused> {
        if self.signal.state() != RUNNING {
            return Err(Refused::ShuttingDown);
        }
        let permit = match &self.limit {
            UpgradeLimit::None => None,
            // The HTTP/2 connection's permit is held until its last stream
            // ends; taking another one per stream would let one client fill
            // `max_connections` with streams on a single socket.
            UpgradeLimit::Connections(_) if h2 => None,
            UpgradeLimit::Connections(l) => Some(l.acquire_over()),
            UpgradeLimit::Own(l) => Some(l.try_acquire().ok_or(Refused::Full)?),
        };
        Ok(Admission {
            slot: self.signal.register(),
            permit,
            watch: ShutdownWatch(self.signal.clone()),
        })
    }
}

/// An admitted upgrade: its drain slot and its connection permit.
pub(crate) struct Admission {
    slot: Slot,
    permit: Option<Permit>,
    watch: ShutdownWatch,
}

impl Admission {
    /// Tells the upgraded connection when shutdown starts.
    pub(crate) fn watch(&self) -> ShutdownWatch {
        self.watch.clone()
    }

    /// Run the upgraded connection as a tracked task: the drain waits for it
    /// (up to `shutdown_timeout`) and then drops it.
    pub(crate) fn spawn<F>(self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(Watched::new(Detached { task }, self.slot, self.permit));
    }
}

/// Whether the server has started shutting down: one atomic load.
#[derive(Clone)]
pub(crate) struct ShutdownWatch(Signal);

impl ShutdownWatch {
    #[inline]
    pub(crate) fn stopping(&self) -> bool {
        self.0.state() != RUNNING
    }
}

pin_project! {
    /// An upgraded connection's task. It has no graceful shutdown of its own:
    /// the WebSocket sees the drain through its [`ShutdownWatch`] when the
    /// task wakes, and sends the close frame itself.
    struct Detached<F> {
        #[pin]
        task: F,
    }
}

impl<F: Future<Output = ()>> Future for Detached<F> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.project().task.poll(cx)
    }
}

impl<F> GracefulShutdown for Detached<F> {
    fn graceful_shutdown(self: Pin<&mut Self>) {}
}
