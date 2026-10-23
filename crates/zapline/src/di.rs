//! Dependency injection: register services once, receive them in handlers
//! with [`Dep<T>`].
//!
//! Providers are app-wide singletons stored behind an `Arc`. Register a ready
//! value with [`App::provide`], a trait object with [`App::provide_dyn`], or
//! build one asynchronously at startup with [`App::provide_with`] (factories
//! run in registration order and see everything registered before them).
//!
//! A Spring-style layered app (controller -> service -> repository):
//!
//! ```no_run
//! use std::sync::Arc;
//! use zapline::di::Dep;
//! use zapline::prelude::*;
//!
//! // Repository layer: a trait so tests can swap in a mock.
//! trait UserRepo: Send + Sync {
//!     fn name(&self, id: u64) -> Option<String>;
//! }
//!
//! struct InMemoryUsers;
//! impl UserRepo for InMemoryUsers {
//!     fn name(&self, id: u64) -> Option<String> {
//!         (id == 1).then(|| "ada".to_string())
//!     }
//! }
//!
//! // Service layer: business logic, depends on the repository.
//! struct UserService {
//!     repo: Arc<dyn UserRepo>,
//! }
//! impl UserService {
//!     fn greeting(&self, id: u64) -> Result<String> {
//!         let name = self.repo.name(id).ok_or_else(Error::not_found)?;
//!         Ok(format!("hello, {name}"))
//!     }
//! }
//!
//! // Controller layer: thin handlers.
//! #[get("/users/{id}/greeting")]
//! async fn greeting(users: Dep<UserService>, Path(id): Path<u64>) -> Result<String> {
//!     users.greeting(id)
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .provide_dyn::<dyn UserRepo>(Arc::new(InMemoryUsers))
//!         .provide_with(|res: Resources| async move {
//!             Ok(UserService { repo: res.dep::<dyn UserRepo>()? })
//!         })
//!         .mount(greeting)
//!         .run("0.0.0.0:8080")
//! }
//! ```
//!
//! Registering the same type again replaces the earlier provider, including a
//! pending factory, so tests can build the real app and override a piece:
//! `app().override_provider::<dyn UserRepo>(Arc::new(MockRepo))`.
//!
//! Every provider is also visible as `State<Arc<T>>` and through
//! [`Resources::dep`], so startup hooks, jobs and middleware can resolve it.

use std::any::{type_name, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::extract::FromRequestParts;
use crate::{App, Error, Request, Resources, Result};

// ---------------------------------------------------------------------------
// Dep<T>

/// Extractor for a provider registered with [`App::provide`] (or any of its
/// siblings). Holds an `Arc<T>`, so extraction is one lookup plus a refcount
/// increment, and `T` may be a trait object (`Dep<dyn Repo>`).
///
/// Fails with a 500 (code `missing_dependency`) if nothing provides `T`.
/// Use it through `Deref` (`users.find(id)`); [`arc`](Self::arc) and
/// [`into_inner`](Self::into_inner) give the shared handle.
///
/// ```
/// use zapline::di::Dep;
/// use zapline::prelude::*;
///
/// struct Clock;
/// impl Clock {
///     fn now(&self) -> u64 { 42 }
/// }
///
/// async fn now(clock: Dep<Clock>) -> String {
///     clock.now().to_string()
/// }
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let client = App::new().provide(Clock).get("/now", now).test();
/// assert_eq!(client.get("/now").send().await.text(), "42");
/// # }
/// ```
pub struct Dep<T: ?Sized>(Arc<T>);

impl<T: ?Sized> Dep<T> {
    /// Wrap an existing handle.
    pub fn new(value: Arc<T>) -> Self {
        Dep(value)
    }

    /// The shared handle.
    pub fn into_inner(self) -> Arc<T> {
        self.0
    }

    /// Borrow the shared handle (e.g. to clone it into a spawned task).
    pub fn arc(&self) -> &Arc<T> {
        &self.0
    }
}

impl<T: ?Sized> Clone for Dep<T> {
    #[inline]
    fn clone(&self) -> Self {
        Dep(self.0.clone())
    }
}

impl<T: ?Sized> Deref for Dep<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: ?Sized> AsRef<T> for Dep<T> {
    fn as_ref(&self) -> &T {
        &self.0
    }
}

impl<T: ?Sized> From<Arc<T>> for Dep<T> {
    fn from(value: Arc<T>) -> Self {
        Dep(value)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Dep<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Dep").field(&&*self.0).finish()
    }
}

impl<T: ?Sized + Send + Sync + 'static> FromRequestParts for Dep<T> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        match req.state::<Arc<T>>() {
            Some(v) => Ok(Dep(v.clone())),
            None => Err(missing::<T>()),
        }
    }
}

fn missing<T: ?Sized>() -> Error {
    let name = type_name::<T>();
    Error::internal(format!("no provider registered for `{name}`"))
        .with_code("missing_dependency")
        .diagnosed("ZL0002", Some(name.to_owned()))
}

impl Resources {
    /// Resolve a provider registered with [`App::provide`] and friends.
    /// Use it inside factories, startup hooks and jobs.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use zapline::prelude::*;
    /// struct Db;
    /// struct Repo { db: Arc<Db> }
    ///
    /// let app = App::new()
    ///     .provide(Db)
    ///     .provide_with(|res: Resources| async move {
    ///         Ok(Repo { db: res.dep::<Db>()? })
    ///     });
    /// ```
    pub fn dep<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.get_arc::<Arc<T>>()
            .map(|outer| Arc::clone(&*outer))
            .ok_or_else(missing::<T>)
    }
}

// ---------------------------------------------------------------------------
// Registration

/// Types that can build themselves from already-registered [`Resources`].
/// Register with [`App::provide_injected`].
///
/// ```
/// # use std::sync::Arc;
/// use zapline::di::Inject;
/// use zapline::prelude::*;
///
/// struct Db;
/// struct Repo { db: Arc<Db> }
///
/// impl Inject for Repo {
///     fn inject(res: &Resources) -> Result<Self> {
///         Ok(Repo { db: res.dep::<Db>()? })
///     }
/// }
///
/// let app = App::new().provide(Db).provide_injected::<Repo>();
/// ```
pub trait Inject: Sized + Send + Sync + 'static {
    /// Build `Self`, typically from providers resolved with [`Resources::dep`].
    fn inject(res: &Resources) -> Result<Self>;
}

/// Tracks the latest registration per provider key so a later `provide*`
/// call supersedes an earlier (possibly still pending) factory.
#[derive(Clone, Default)]
struct Registry(Arc<Mutex<HashMap<TypeId, u64>>>);

static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(1);

impl Registry {
    fn of(app: &mut App) -> Registry {
        app.extensions_mut()
            .get_or_insert_default::<Registry>()
            .clone()
    }

    fn register(&self, key: TypeId) -> u64 {
        let id = NEXT_REGISTRATION.fetch_add(1, Ordering::Relaxed);
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, id);
        id
    }

    fn is_current(&self, key: TypeId, id: u64) -> bool {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&key) == Some(&id)
    }
}

fn init_failed<T: ?Sized>(e: Error) -> Error {
    let mut msg = format!(
        "dependency `{}` failed to initialize: {}",
        type_name::<T>(),
        e.message()
    );
    if let Some(src) = e.source_error() {
        msg.push_str(&format!(" ({src})"));
    }
    Error::internal(msg)
        .with_code("dependency_init_failed")
        .diagnosed("ZL0003", Some(type_name::<T>().to_owned()))
}

impl App {
    /// Register `value` as an app-wide singleton, extracted with [`Dep<T>`].
    /// Calling it again for the same `T` replaces the earlier provider.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// struct Mailer { from: String }
    /// let app = App::new().provide(Mailer { from: "noreply@example.com".into() });
    /// ```
    pub fn provide<T: Send + Sync + 'static>(self, value: T) -> Self {
        self.provide_arc(Arc::new(value))
    }

    /// Register a provider that is already behind an `Arc` (e.g. shared with
    /// code outside the app).
    pub fn provide_arc<T: ?Sized + Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        Registry::of(&mut self).register(TypeId::of::<Arc<T>>());
        self.state_arc(Arc::new(value))
    }

    /// Register a trait object, extracted with `Dep<dyn Trait>`.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use zapline::prelude::*;
    /// trait Repo: Send + Sync {}
    /// struct PgRepo;
    /// impl Repo for PgRepo {}
    ///
    /// let app = App::new().provide_dyn::<dyn Repo>(Arc::new(PgRepo));
    /// ```
    pub fn provide_dyn<T: ?Sized + Send + Sync + 'static>(self, value: Arc<T>) -> Self {
        self.provide_arc(value)
    }

    /// Replace whatever currently provides `T` (a value or a pending factory).
    /// Meant for tests: build the real app, then swap in a mock.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use zapline::prelude::*;
    /// trait Repo: Send + Sync {}
    /// struct PgRepo;
    /// impl Repo for PgRepo {}
    /// struct MockRepo;
    /// impl Repo for MockRepo {}
    ///
    /// fn app() -> App {
    ///     App::new().provide_dyn_with(|_res: Resources| async move {
    ///         let repo: Arc<dyn Repo> = Arc::new(PgRepo); // connects in real life
    ///         Ok(repo)
    ///     })
    /// }
    ///
    /// let under_test = app().override_provider::<dyn Repo>(Arc::new(MockRepo));
    /// ```
    pub fn override_provider<T: ?Sized + Send + Sync + 'static>(self, value: Arc<T>) -> Self {
        self.provide_arc(value)
    }

    /// Build a provider asynchronously at startup. Factories run in
    /// registration order, interleaved with `on_startup` hooks, and can
    /// resolve anything registered before them. A failing factory aborts
    /// startup with an error naming the type.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use zapline::prelude::*;
    /// struct Pool;
    /// impl Pool { async fn connect(_url: &str) -> Result<Pool> { Ok(Pool) } }
    /// struct UserService { pool: Arc<Pool> }
    ///
    /// let app = App::new()
    ///     .provide_with(|_res: Resources| async move { Pool::connect("postgres://...").await })
    ///     .provide_with(|res: Resources| async move {
    ///         Ok(UserService { pool: res.dep::<Pool>()? })
    ///     });
    /// ```
    pub fn provide_with<T, F, Fut>(self, factory: F) -> Self
    where
        T: Send + Sync + 'static,
        F: FnOnce(Resources) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        self.provide_dyn_with(move |res: Resources| async move { factory(res).await.map(Arc::new) })
    }

    /// Like [`provide_with`](Self::provide_with) for trait objects: the
    /// factory returns `Arc<dyn Trait>`. The trait object type is usually
    /// inferred from the factory's return type.
    pub fn provide_dyn_with<T, F, Fut>(mut self, factory: F) -> Self
    where
        T: ?Sized + Send + Sync + 'static,
        F: FnOnce(Resources) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Arc<T>>> + Send + 'static,
    {
        let registry = Registry::of(&mut self);
        let key = TypeId::of::<Arc<T>>();
        let id = registry.register(key);
        self.on_startup(move |res: Resources| async move {
            if !registry.is_current(key, id) {
                return Ok(()); // superseded by a later registration
            }
            let value = factory(res.clone()).await.map_err(init_failed::<T>)?;
            res.insert_arc::<Arc<T>>(Arc::new(value));
            Ok(())
        })
    }

    /// Register `T` built by its [`Inject`] implementation at startup.
    pub fn provide_injected<T: Inject>(self) -> Self {
        self.provide_with(|res: Resources| async move { T::inject(&res) })
    }
}

// ---------------------------------------------------------------------------
// Request scope

/// Runs extractor `T` at most once per request and caches the result in the
/// request extensions. Later `Cached<T>` extractions in the same request (in
/// route middleware, then the handler) get a clone.
///
/// This is the request-scoped counterpart of [`Dep`]: implement
/// [`FromRequestParts`] for a per-request value (current user, tenant,
/// transaction) and extract it as `Cached<T>`.
///
/// ```
/// use zapline::di::Cached;
/// use zapline::extract::FromRequestParts;
/// use zapline::prelude::*;
///
/// #[derive(Clone)]
/// struct CurrentUser(String);
///
/// impl FromRequestParts for CurrentUser {
///     async fn from_request_parts(req: &mut Request) -> Result<Self> {
///         // e.g. a database lookup; runs once per request thanks to Cached
///         let token = req.headers().get("x-user").ok_or_else(Error::unauthorized)?;
///         Ok(CurrentUser(token.to_str().map_err(|_| Error::unauthorized())?.to_owned()))
///     }
/// }
///
/// async fn me(Cached(user): Cached<CurrentUser>) -> String {
///     user.0
/// }
///
/// let app = App::new()
///     .wrap_route(|mut req: Request, next: Next| async move {
///         match Cached::<CurrentUser>::resolve(&mut req).await {
///             Ok(_) => next.run(req).await,
///             Err(e) => e.into_response(),
///         }
///     })
///     .get("/me", me);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cached<T>(pub T);

#[derive(Clone)]
struct CacheSlot<T>(T);

impl<T> Cached<T> {
    /// The extracted value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: FromRequestParts + Clone + Sync + 'static> Cached<T> {
    /// Extract `T` through the per-request cache, for use in middleware.
    pub async fn resolve(req: &mut Request) -> Result<T> {
        if let Some(CacheSlot(v)) = req.extensions().get::<CacheSlot<T>>() {
            return Ok(v.clone());
        }
        let v = T::from_request_parts(req).await?;
        req.extensions_mut().insert(CacheSlot(v.clone()));
        Ok(v)
    }
}

impl<T> Deref for Cached<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: FromRequestParts + Clone + Sync + 'static> FromRequestParts for Cached<T> {
    #[inline]
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        Self::resolve(req).await.map(Cached)
    }
}
