//! Application lifecycle: startup and shutdown hooks plus the typed
//! [`Resources`] registry they share with request handlers.
//!
//! ```no_run
//! use zapline::prelude::*;
//!
//! #[derive(Clone)]
//! struct Db; // e.g. a connection pool
//!
//! App::new()
//!     .on_startup(|res: Resources| async move {
//!         res.insert(Db); // connect here; available as State<Db> in handlers
//!         Ok(())
//!     })
//!     .on_shutdown(|res: Resources| async move {
//!         let _db = res.get::<Db>(); // flush, close pools...
//!     });
//! ```

use std::any::{Any, TypeId};
use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::handler::BoxFuture;
use crate::request::StateEntry;

/// A typed, shared registry of application resources (state, services,
/// connection pools). Values registered here are what [`State<T>`] and
/// other dependency extractors resolve.
///
/// Cloning is cheap; all clones refer to the same registry.
///
/// [`State<T>`]: crate::extract::State
#[derive(Clone, Default)]
pub struct Resources {
    inner: Arc<Mutex<Vec<StateEntry>>>,
}

impl Resources {
    pub(crate) fn from_entries(entries: Vec<StateEntry>) -> Self {
        Resources {
            inner: Arc::new(Mutex::new(entries)),
        }
    }

    #[cfg(all(feature = "json", any(feature = "postgres", feature = "sqlite")))]
    /// A handle that doesn't keep the registry alive (for values stored in
    /// it that need to reach it again).
    pub(crate) fn downgrade(&self) -> WeakResources {
        WeakResources(Arc::downgrade(&self.inner))
    }

    pub(crate) fn snapshot(&self) -> Vec<StateEntry> {
        self.inner.lock().unwrap().clone()
    }

    /// Register (or replace) a value of type `T`.
    pub fn insert<T: Clone + Send + Sync + 'static>(&self, value: T) {
        self.insert_arc(Arc::new(value));
    }

    /// Register a pre-wrapped value. The registry key is `T`.
    pub fn insert_arc<T: Send + Sync + 'static>(&self, value: Arc<T>) {
        let id = TypeId::of::<T>();
        let mut v = self.inner.lock().unwrap();
        v.retain(|(t, _)| *t != id);
        v.push((id, value as Arc<dyn Any + Send + Sync>));
    }

    /// A clone of the registered `T`, if any.
    pub fn get<T: Clone + Send + Sync + 'static>(&self) -> Option<T> {
        self.get_arc::<T>().map(|a| (*a).clone())
    }

    /// The shared handle to the registered `T`, if any.
    pub fn get_arc<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        let id = TypeId::of::<T>();
        let v = self.inner.lock().unwrap();
        v.iter()
            .find(|(t, _)| *t == id)
            .and_then(|(_, a)| a.clone().downcast::<T>().ok())
    }

    /// Like [`get`](Self::get) but with a descriptive error when missing.
    pub fn require<T: Clone + Send + Sync + 'static>(&self) -> Result<T, crate::Error> {
        self.get::<T>().ok_or_else(|| {
            let name = std::any::type_name::<T>();
            crate::Error::internal(format!("resource `{name}` is not registered"))
                .diagnosed("ZL0004", Some(name.to_owned()))
        })
    }

    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        let id = TypeId::of::<T>();
        self.inner.lock().unwrap().iter().any(|(t, _)| *t == id)
    }
}

/// A non-owning [`Resources`] handle.
#[cfg(all(feature = "json", any(feature = "postgres", feature = "sqlite")))]
#[derive(Clone)]
pub(crate) struct WeakResources(std::sync::Weak<Mutex<Vec<StateEntry>>>);

#[cfg(all(feature = "json", any(feature = "postgres", feature = "sqlite")))]
impl WeakResources {
    pub(crate) fn upgrade(&self) -> Option<Resources> {
        self.0.upgrade().map(|inner| Resources { inner })
    }
}

pub(crate) type StartupHook = Box<dyn FnOnce(Resources) -> BoxFuture<crate::Result<()>> + Send>;
pub(crate) type ShutdownHook = Box<dyn FnOnce(Resources) -> BoxFuture<()> + Send>;

pub(crate) fn startup_hook<F, Fut>(f: F) -> StartupHook
where
    F: FnOnce(Resources) -> Fut + Send + 'static,
    Fut: Future<Output = crate::Result<()>> + Send + 'static,
{
    Box::new(move |res| Box::pin(f(res)))
}

pub(crate) fn shutdown_hook<F, Fut>(f: F) -> ShutdownHook
where
    F: FnOnce(Resources) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    Box::new(move |res| Box::pin(f(res)))
}

/// Hooks still to run, owned by a running app.
#[derive(Default)]
pub(crate) struct Lifecycle {
    pub(crate) startup: Vec<StartupHook>,
    pub(crate) shutdown: Vec<ShutdownHook>,
}

impl Lifecycle {
    /// Run startup hooks in registration order; stops at the first error.
    pub(crate) async fn run_startup(&mut self, res: &Resources) -> crate::Result<()> {
        for hook in self.startup.drain(..) {
            hook(res.clone()).await?;
        }
        Ok(())
    }

    /// Run shutdown hooks in reverse registration order.
    pub(crate) async fn run_shutdown(&mut self, res: &Resources) {
        while let Some(hook) = self.shutdown.pop() {
            hook(res.clone()).await;
        }
    }
}
