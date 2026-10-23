//! `max_connections` (and `max_upgraded`) accounting.
//!
//! A counter rather than a `tokio::sync::Semaphore`, because an upgraded
//! connection has to keep its place without a gap: a WebSocket takes a
//! permit with [`Limit::acquire_over`] while the HTTP connection it came from
//! still holds its own, and the HTTP permit is released when hyper hands the
//! socket over. The count is briefly one over the limit (the accept loop
//! just waits a little longer), never under it, so a new connection cannot
//! slip into the slot the WebSocket is about to occupy. (An HTTP/2
//! WebSocket takes no permit here: its connection keeps its own.)
//!
//! Only the accept loop waits for a permit, so there is a single waker.

use std::future::{poll_fn, Future};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

/// A connection limit shared by one accept loop and its connections.
pub(crate) struct Limit {
    max: usize,
    used: AtomicUsize,
    /// The accept loop, while it waits at the limit.
    waiter: Mutex<Option<Waker>>,
}

/// One counted connection. Gives its place back on drop.
pub(crate) struct Permit(Arc<Limit>);

impl Limit {
    pub(crate) fn new(max: usize) -> Arc<Limit> {
        Arc::new(Limit {
            max: max.max(1),
            used: AtomicUsize::new(0),
            waiter: Mutex::new(None),
        })
    }

    /// A permit if the limit has room.
    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<Permit> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max).then_some(n + 1)
            })
            .ok()
            .map(|_| Permit(self.clone()))
    }

    /// A permit even at the limit: for a connection that already holds one
    /// and is about to release it (an upgrade).
    #[cfg(any(feature = "ws", test))]
    pub(crate) fn acquire_over(self: &Arc<Self>) -> Permit {
        self.used.fetch_add(1, Ordering::AcqRel);
        Permit(self.clone())
    }

    /// Wait until the limit has room (the accept loop).
    pub(crate) fn acquire(self: &Arc<Self>) -> impl Future<Output = Permit> + '_ {
        poll_fn(move |cx| self.poll_acquire(cx))
    }

    fn poll_acquire(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<Permit> {
        if let Some(p) = self.try_acquire() {
            return Poll::Ready(p);
        }
        // Publish the waker, then look again: a permit released before the
        // waker was visible has already lowered the count.
        let mut waiter = self.waiter.lock().unwrap_or_else(PoisonError::into_inner);
        if !waiter.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
            *waiter = Some(cx.waker().clone());
        }
        drop(waiter);
        match self.try_acquire() {
            Some(p) => Poll::Ready(p),
            None => Poll::Pending,
        }
    }

    /// Connections counted right now.
    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let before = self.0.used.fetch_sub(1, Ordering::AcqRel);
        // Below the limit nobody waits, so the lock is only taken at it.
        if before >= self.0.max {
            let waker = self
                .0
                .waiter
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(w) = waker {
                w.wake();
            }
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
    fn waits_at_the_limit_and_wakes_on_release() {
        let limit = Limit::new(2);
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);

        let a = limit.try_acquire().unwrap();
        let _b = limit.try_acquire().unwrap();
        assert!(limit.try_acquire().is_none());
        assert!(limit.poll_acquire(&mut cx).is_pending());
        drop(a);
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert!(limit.poll_acquire(&mut cx).is_ready());
    }

    #[test]
    fn acquire_over_keeps_the_place_of_an_upgraded_connection() {
        let limit = Limit::new(1);
        let http = limit.try_acquire().unwrap();
        // The upgrade takes its permit while the HTTP connection holds one...
        let ws = limit.acquire_over();
        assert_eq!(limit.used(), 2);
        // ...so releasing the HTTP permit leaves no room for a new connection.
        drop(http);
        assert!(limit.try_acquire().is_none());
        drop(ws);
        assert!(limit.try_acquire().is_some());
    }
}
