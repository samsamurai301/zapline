//! Graceful shutdown.
//!
//! Each accept loop owns a [`Signal`]: a state word plus a registry of the
//! wakers of its live connections. A connection checks the state with one
//! atomic load per poll and only touches the registry when its waker
//! changes, so watching for shutdown costs nothing measurable per request.
//! (A `tokio::sync::watch`/`Notify` would take a mutex on every poll.)

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use super::limit::Permit;
use pin_project_lite::pin_project;

/// Serving normally.
pub(crate) const RUNNING: u8 = 0;
/// Stop accepting; connections finish their in-flight requests and close.
pub(crate) const DRAINING: u8 = 1;
/// Drop every remaining connection now.
pub(crate) const FORCED: u8 = 2;

/// Shutdown state for one accept loop and its connections.
#[derive(Clone, Default)]
pub(crate) struct Signal(Arc<Inner>);

#[derive(Default)]
struct Inner {
    state: AtomicU8,
    registry: Mutex<Registry>,
}

#[derive(Default)]
struct Registry {
    wakers: Vec<Option<Waker>>,
    free: Vec<usize>,
    live: usize,
    idle: Option<Waker>,
}

impl Signal {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.0
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    #[inline]
    pub(crate) fn state(&self) -> u8 {
        self.0.state.load(Ordering::Acquire)
    }

    /// Move to `state` (states only advance) and wake every live task.
    pub(crate) fn advance(&self, state: u8) {
        if self.0.state.fetch_max(state, Ordering::AcqRel) >= state {
            return;
        }
        let wakers: Vec<Waker> = self.registry().wakers.iter().flatten().cloned().collect();
        for w in wakers {
            w.wake();
        }
    }

    /// Track a new connection (or the accept loop itself).
    pub(crate) fn register(&self) -> Slot {
        let mut reg = self.registry();
        reg.live += 1;
        let index = match reg.free.pop() {
            Some(i) => i,
            None => {
                reg.wakers.push(None);
                reg.wakers.len() - 1
            }
        };
        Slot {
            signal: self.clone(),
            index,
            waker: None,
        }
    }

    /// Resolves once no [`Slot`] is alive.
    pub(crate) fn idle(&self) -> impl Future<Output = ()> + '_ {
        poll_fn(move |cx| {
            let mut reg = self.registry();
            if reg.live == 0 {
                reg.idle = None;
                Poll::Ready(())
            } else {
                reg.idle = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }

    /// Stop accepting, then wait up to `timeout` for connections to finish
    /// before dropping the rest.
    pub(crate) async fn drain(&self, timeout: std::time::Duration) {
        self.advance(DRAINING);
        if tokio::time::timeout(timeout, self.idle()).await.is_err() {
            self.advance(FORCED);
            self.idle().await;
        }
    }
}

/// One registered task. Deregisters on drop.
pub(crate) struct Slot {
    signal: Signal,
    index: usize,
    waker: Option<Waker>,
}

impl Slot {
    /// The current state; makes sure the task polling us is woken when it
    /// changes.
    #[inline]
    pub(crate) fn poll_state(&mut self, cx: &mut Context<'_>) -> u8 {
        if !self.waker.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
            self.set_waker(cx.waker());
        }
        self.signal.state()
    }

    #[cold]
    fn set_waker(&mut self, waker: &Waker) {
        // Publish under the lock, then re-read the state in `poll_state`:
        // either `advance` sees this waker or we see its new state.
        let old = self.signal.registry().wakers[self.index].replace(waker.clone());
        self.waker = Some(waker.clone());
        drop(old);
    }

    /// Resolves once shutdown has started (draining or forced).
    pub(crate) fn stopped(&mut self) -> impl Future<Output = ()> + '_ {
        poll_fn(move |cx| match self.poll_state(cx) {
            RUNNING => Poll::Pending,
            _ => Poll::Ready(()),
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut reg = self.signal.registry();
        let old = reg.wakers[self.index].take();
        reg.free.push(self.index);
        reg.live -= 1;
        let idle = if reg.live == 0 { reg.idle.take() } else { None };
        drop(reg);
        drop(old);
        if let Some(w) = idle {
            w.wake();
        }
    }
}

/// A connection future that can be asked to finish its in-flight work and
/// close (hyper's `graceful_shutdown`).
pub(crate) trait GracefulShutdown {
    fn graceful_shutdown(self: Pin<&mut Self>);
}

pin_project! {
    /// Drives a connection and applies shutdown: on [`DRAINING`] it asks the
    /// connection to close after in-flight requests, on [`FORCED`] it drops it.
    pub(crate) struct Watched<C> {
        #[pin]
        conn: C,
        slot: Slot,
        draining: bool,
        // A `max_connections` permit, released with the connection.
        permit: Option<Permit>,
    }
}

impl<C> Watched<C> {
    pub(crate) fn new(conn: C, slot: Slot, permit: Option<Permit>) -> Self {
        Watched {
            conn,
            slot,
            draining: false,
            permit,
        }
    }
}

impl<C, T> Future for Watched<C>
where
    C: Future<Output = T> + GracefulShutdown,
{
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut this = self.project();
        match this.slot.poll_state(cx) {
            RUNNING => {}
            DRAINING => {
                if !*this.draining {
                    *this.draining = true;
                    this.conn.as_mut().graceful_shutdown();
                }
            }
            _ => return Poll::Ready(()),
        }
        this.conn.poll(cx).map(drop)
    }
}

/// Fans a shutdown out to every accept loop of a server.
#[derive(Clone)]
pub(crate) struct Trigger(Arc<[Signal]>);

impl Trigger {
    pub(crate) fn new(signals: Vec<Signal>) -> Self {
        Trigger(signals.into())
    }

    /// Stop accepting everywhere and let connections finish.
    pub(crate) fn drain(&self) {
        for s in self.0.iter() {
            s.advance(DRAINING);
        }
    }

    /// Drop every remaining connection now.
    pub(crate) fn force(&self) {
        for s in self.0.iter() {
            s.advance(FORCED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;

    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn advance_wakes_registered_slots_once_per_state() {
        let signal = Signal::default();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);
        let mut slot = signal.register();
        assert_eq!(slot.poll_state(&mut cx), RUNNING);

        signal.advance(DRAINING);
        signal.advance(DRAINING); // no-op: state did not change
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert_eq!(slot.poll_state(&mut cx), DRAINING);

        signal.advance(FORCED);
        signal.advance(DRAINING); // states never go back
        assert_eq!(count.0.load(Ordering::SeqCst), 2);
        assert_eq!(signal.state(), FORCED);
    }

    #[test]
    fn idle_resolves_when_last_slot_drops_and_slots_are_reused() {
        let signal = Signal::default();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);

        let a = signal.register();
        let b = signal.register();
        let mut idle = Box::pin(signal.idle());
        assert!(idle.as_mut().poll(&mut cx).is_pending());
        drop(a);
        assert_eq!(count.0.load(Ordering::SeqCst), 0);
        drop(b);
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert!(idle.as_mut().poll(&mut cx).is_ready());

        let c = signal.register();
        assert_eq!(c.index, 1, "freed indexes are reused");
        assert_eq!(signal.registry().wakers.len(), 2);
    }
}
