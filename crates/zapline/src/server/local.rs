//! Values owned by one thread-per-core worker.
//!
//! I/O resources (database connections, clients) are registered with the
//! runtime that created them, and are woken by that runtime's I/O driver.
//! Shared between per-core workers, every wake-up crosses threads, and on a
//! single-threaded runtime it waits for the owning thread to poll its driver.
//! Giving each worker its own instance keeps that traffic on one core.

use std::any::Any;
use std::cell::{Cell, RefCell};

thread_local! {
    /// `(index, count)` on a per-core worker thread.
    static WORKER: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
    static LOCALS: RefCell<Vec<(u64, Box<dyn Any>)>> = const { RefCell::new(Vec::new()) };
}

/// Mark the current thread as per-core worker `index` of `count`.
pub(crate) fn enter(index: usize, count: usize) {
    WORKER.set(Some((index, count)));
}

/// Drop this worker's values. Call it inside the worker's runtime, before
/// the runtime shuts down, so they can clean up on it.
pub(crate) fn clear() {
    let locals = LOCALS.take();
    drop(locals);
    WORKER.set(None);
}

/// A unique key for [`get_or_init`].
pub(crate) fn key() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// This worker's value for `key`, created with `init(worker_count)` on first
/// use. `None` outside per-core workers (multi-threaded runtime, `App::serve`,
/// tests), where callers fall back to a shared value.
pub(crate) fn get_or_init<T: Clone + 'static>(
    key: u64,
    init: impl FnOnce(usize) -> T,
) -> Option<T> {
    let (_, count) = WORKER.get()?;
    let found = LOCALS.with_borrow(|locals| {
        locals
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.downcast_ref::<T>().cloned())
    });
    if found.is_some() {
        return found;
    }
    // Not under the borrow: `init` may use other worker locals.
    let value = init(count);
    LOCALS.with_borrow_mut(|locals| locals.push((key, Box::new(value.clone()))));
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_per_worker_and_absent_elsewhere() {
        let k = key();
        assert_eq!(get_or_init(k, |_| 1), None);
        std::thread::spawn(move || {
            enter(0, 4);
            assert_eq!(get_or_init(k, |n| n * 10), Some(40));
            assert_eq!(get_or_init(k, |_| 0usize), Some(40));
            clear();
            assert_eq!(get_or_init(k, |_| 1), None);
        })
        .join()
        .unwrap();
    }
}
