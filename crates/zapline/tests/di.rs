//! Dependency injection: providers, trait objects, async factories,
//! overrides for tests, `Inject`, and request-scoped `Cached<T>`.
#![cfg(feature = "json")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use zapline::di::{Cached, Dep, Inject};
use zapline::extract::FromRequestParts;
use zapline::prelude::*;

// ---- a small layered app: repository -> service -> controller -------------

trait UserRepo: Send + Sync {
    fn find(&self, id: u64) -> Option<String>;
}

struct MemRepo;
impl UserRepo for MemRepo {
    fn find(&self, id: u64) -> Option<String> {
        (id == 1).then(|| "ada".to_owned())
    }
}

struct MockRepo;
impl UserRepo for MockRepo {
    fn find(&self, _id: u64) -> Option<String> {
        Some("mock".to_owned())
    }
}

struct UserService {
    repo: Arc<dyn UserRepo>,
}

impl UserService {
    fn name(&self, id: u64) -> Result<String> {
        self.repo.find(id).ok_or_else(Error::not_found)
    }
}

async fn get_user(users: Dep<UserService>, Path(id): Path<u64>) -> Result<String> {
    users.name(id)
}

fn layered_app(factory_runs: Arc<AtomicUsize>) -> App {
    App::new()
        .provide_dyn_with(move |_res: Resources| async move {
            factory_runs.fetch_add(1, Ordering::SeqCst);
            let repo: Arc<dyn UserRepo> = Arc::new(MemRepo);
            Ok(repo)
        })
        .provide_with(|res: Resources| async move {
            Ok(UserService {
                repo: res.dep::<dyn UserRepo>()?,
            })
        })
        .get("/users/{id}", get_user)
}

#[tokio::test]
async fn layered_app_resolves_service_and_trait_object() {
    let runs = Arc::new(AtomicUsize::new(0));
    let c = layered_app(runs.clone()).test();

    let r = c.get("/users/1").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.text(), "ada");
    assert_eq!(c.get("/users/2").send().await.status(), 404);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "factories run once");
}

#[tokio::test]
async fn override_provider_replaces_pending_factory() {
    let runs = Arc::new(AtomicUsize::new(0));
    let c = layered_app(runs.clone())
        .override_provider::<dyn UserRepo>(Arc::new(MockRepo))
        .test();

    assert_eq!(c.get("/users/7").send().await.text(), "mock");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "overridden factory never runs"
    );
}

#[tokio::test]
async fn later_factory_replaces_earlier_value() {
    struct Greeting(&'static str);
    let c = App::new()
        .provide(Greeting("value"))
        .provide_with(|_res: Resources| async { Ok(Greeting("factory")) })
        .get("/", |g: Dep<Greeting>| async move { g.0 })
        .test();
    assert_eq!(c.get("/").send().await.text(), "factory");

    let c = App::new()
        .provide_with(|_res: Resources| async { Ok(Greeting("factory")) })
        .provide(Greeting("value"))
        .get("/", |g: Dep<Greeting>| async move { g.0 })
        .test();
    assert_eq!(c.get("/").send().await.text(), "value");
}

#[tokio::test]
async fn concrete_provider_is_shared_and_visible_as_state_arc() {
    struct Counter(AtomicUsize);
    let c = App::new()
        .provide(Counter(AtomicUsize::new(0)))
        .get("/inc", |c: Dep<Counter>| async move {
            (c.0.fetch_add(1, Ordering::SeqCst) + 1).to_string()
        })
        .get("/state", |State(c): State<Arc<Counter>>| async move {
            c.0.load(Ordering::SeqCst).to_string()
        })
        .test();

    c.get("/inc").send().await;
    assert_eq!(c.get("/inc").send().await.text(), "2");
    assert_eq!(c.get("/state").send().await.text(), "2");

    let res = c.resources().await;
    assert_eq!(res.dep::<Counter>().unwrap().0.load(Ordering::SeqCst), 2);
    assert!(res.contains::<Arc<Counter>>());
}

#[tokio::test]
async fn provide_arc_shares_an_existing_handle() {
    struct Cache(Mutex<Vec<&'static str>>);
    let cache = Arc::new(Cache(Mutex::new(Vec::new())));
    let c = App::new()
        .provide_arc(cache.clone())
        .post("/", |c: Dep<Cache>| async move {
            c.0.lock().unwrap().push("hit");
        })
        .test();
    c.post("/").send().await;
    assert_eq!(*cache.0.lock().unwrap(), vec!["hit"]);
}

#[tokio::test]
async fn missing_provider_is_a_500_with_code() {
    struct NotRegistered;
    let c = App::new()
        .get("/", |_d: Dep<NotRegistered>| async { "unreachable" })
        .get("/dyn", |_d: Dep<dyn UserRepo>| async { "unreachable" })
        .test();

    let r = c.get("/").send().await;
    assert_eq!(r.status(), 500);
    let body: serde_json::Value = r.json();
    assert_eq!(body["code"], "missing_dependency");
    // The type name is logged for operators, never sent to clients.
    assert_eq!(body["error"], "internal server error");

    let r = c.get("/dyn").send().await;
    assert_eq!(r.status(), 500);
    assert!(!r.text().contains("UserRepo"));
}

#[tokio::test]
async fn optional_dep_extracts_none_when_missing() {
    struct Missing;
    let c = App::new()
        .get("/", |d: Option<Dep<Missing>>| async move {
            if d.is_none() {
                "none"
            } else {
                "some"
            }
        })
        .test();
    assert_eq!(c.get("/").send().await.text(), "none");
}

#[tokio::test]
async fn factories_run_in_order_with_startup_hooks() {
    #[derive(Clone)]
    struct Url(&'static str);
    struct Pool(String);
    struct Service(String);

    let c = App::new()
        .on_startup(|res: Resources| async move {
            res.insert(Url("db://primary"));
            Ok(())
        })
        .provide_with(|res: Resources| async move { Ok(Pool(res.require::<Url>()?.0.to_owned())) })
        .provide_with(|res: Resources| async move {
            Ok(Service(format!("svc({})", res.dep::<Pool>()?.0)))
        })
        .get("/", |s: Dep<Service>| async move { s.0.clone() })
        .test();
    assert_eq!(c.get("/").send().await.text(), "svc(db://primary)");
}

#[tokio::test]
#[should_panic(expected = "Service` failed to initialize: no provider registered for")]
async fn failing_factory_aborts_startup_naming_the_type() {
    struct Pool;
    struct Service;
    let c = App::new()
        .provide_with(|res: Resources| async move {
            let _pool = res.dep::<Pool>()?; // never registered
            Ok(Service)
        })
        .test();
    c.start().await;
}

#[tokio::test]
async fn failing_factory_fails_serve() {
    struct Service;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = App::new()
        .provide_with(|_res: Resources| async {
            Err::<Service, _>(Error::internal("connection refused"))
        })
        .serve(listener)
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("startup failed"), "{msg}");
    assert!(msg.contains("connection refused"), "{msg}");
}

// ---- Inject -----------------------------------------------------------------

struct Settings {
    greeting: &'static str,
}

struct Greeter {
    settings: Arc<Settings>,
}

impl Inject for Greeter {
    fn inject(res: &Resources) -> Result<Self> {
        Ok(Greeter {
            settings: res.dep::<Settings>()?,
        })
    }
}

#[tokio::test]
async fn provide_injected_builds_from_resources() {
    let c = App::new()
        .provide(Settings { greeting: "hi" })
        .provide_injected::<Greeter>()
        .get("/", |g: Dep<Greeter>| async move { g.settings.greeting })
        .test();
    assert_eq!(c.get("/").send().await.text(), "hi");
}

#[tokio::test]
#[should_panic(expected = "no provider registered")]
async fn provide_injected_reports_missing_dependency() {
    let c = App::new().provide_injected::<Greeter>().test();
    c.start().await;
}

// ---- Dep ergonomics -----------------------------------------------------------

#[test]
fn dep_is_cheap_to_clone_and_derefs() {
    let d: Dep<str> = Dep::new(Arc::from("hello"));
    let d2 = d.clone();
    assert!(Arc::ptr_eq(d.arc(), d2.arc()));
    assert_eq!(d.len(), 5);
    assert_eq!(&*d2.into_inner(), "hello");
    assert_eq!(format!("{d:?}"), "Dep(\"hello\")");
}

// ---- request scope: Cached<T> -------------------------------------------------

#[derive(Clone)]
struct CurrentUser(String);

static LOOKUPS: AtomicUsize = AtomicUsize::new(0);

impl FromRequestParts for CurrentUser {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        LOOKUPS.fetch_add(1, Ordering::SeqCst);
        let name = req
            .headers()
            .get("x-user")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(Error::unauthorized)?;
        Ok(CurrentUser(name.to_owned()))
    }
}

#[tokio::test]
async fn cached_extractor_runs_once_per_request() {
    let c = App::new()
        .get(
            "/me",
            |Cached(a): Cached<CurrentUser>, Cached(b): Cached<CurrentUser>| async move {
                format!("{} {}", a.0, b.0)
            },
        )
        .wrap_route(|mut req: Request, next: Next| async move {
            match Cached::<CurrentUser>::resolve(&mut req).await {
                Ok(_) => next.run(req).await,
                Err(e) => e.into_response(),
            }
        })
        .test();

    let before = LOOKUPS.load(Ordering::SeqCst);
    let r = c.get("/me").header("x-user", "ada").send().await;
    assert_eq!(r.text(), "ada ada");
    assert_eq!(LOOKUPS.load(Ordering::SeqCst) - before, 1);

    // A new request computes again, and failures are not cached.
    let before = LOOKUPS.load(Ordering::SeqCst);
    assert_eq!(c.get("/me").send().await.status(), 401);
    assert_eq!(
        c.get("/me").header("x-user", "bob").send().await.text(),
        "bob bob"
    );
    assert_eq!(LOOKUPS.load(Ordering::SeqCst) - before, 2);
}
