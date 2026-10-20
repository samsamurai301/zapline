# Dependency injection

Register a service once with `App::provide`, receive it in any handler with `Dep<T>`:

```rust
use zapline::di::Dep;
use zapline::prelude::*;

struct Greeter {
    greeting: String,
}

#[get("/hello/{name}")]
async fn hello(greeter: Dep<Greeter>, Path(name): Path<String>) -> String {
    format!("{}, {name}!", greeter.greeting)
}

fn app() -> App {
    App::new()
        .provide(Greeter { greeting: "Hello".into() })
        .mount(hello)
}
```

Providers are **app-wide singletons** stored behind an `Arc`. `Dep<T>` derefs to `T`, and extracting it costs one lookup plus a reference-count increment. `T` may be a trait object (`Dep<dyn UserRepo>`), which is what makes layers swappable in tests. `Dep<T>` is a parts extractor, so it can appear in any argument position.

If nothing provides `T`, the extractor fails with a `500` and the code `missing_dependency`.

## Registering providers

| Method | What it registers |
|---|---|
| `provide(value)` | A ready value of type `T`, wrapped in an `Arc` |
| `provide_arc(arc)` | An `Arc<T>` you already hold, for example one shared with code outside the app |
| `provide_dyn::<dyn Trait>(arc)` | A trait object, extracted as `Dep<dyn Trait>` |
| `provide_with(\|res\| async { Ok(value) })` | A value built asynchronously at startup (connect to a database, load a file) |
| `provide_dyn_with(\|res\| async { Ok(arc_dyn) })` | The same for a trait object: the factory returns `Arc<dyn Trait>` |
| `provide_injected::<T>()` | A value built by `T: Inject` at startup |
| `override_provider::<T>(arc)` | Replaces whatever provides `T`. Meant for tests. |

Registering the same type again **replaces** the earlier provider, including a factory that has not run yet. The last registration wins, whichever kind it was.

## Layers: controller → service → repository

A Spring-style application keeps HTTP concerns in thin controllers, business rules in services, and storage behind a repository trait:

```rust
use std::sync::Arc;
use zapline::di::Dep;
use zapline::prelude::*;

// Repository layer: a trait, so tests can swap in a mock.
trait UserRepo: Send + Sync {
    fn name(&self, id: u64) -> Option<String>;
}

struct InMemoryUsers;

impl UserRepo for InMemoryUsers {
    fn name(&self, id: u64) -> Option<String> {
        (id == 1).then(|| "ada".to_owned())
    }
}

// Service layer: business rules; knows nothing about HTTP except `Error`.
struct UserService {
    repo: Arc<dyn UserRepo>,
}

impl UserService {
    fn greeting(&self, id: u64) -> Result<String> {
        let name = self.repo.name(id).ok_or_else(Error::not_found)?;
        Ok(format!("hello, {name}"))
    }
}

// Controller layer: thin handlers.
#[get("/users/{id}/greeting")]
async fn greeting(users: Dep<UserService>, Path(id): Path<u64>) -> Result<String> {
    users.greeting(id)
}

fn app() -> App {
    App::new()
        .provide_dyn::<dyn UserRepo>(Arc::new(InMemoryUsers))
        .provide_with(|res: Resources| async move {
            Ok(UserService { repo: res.dep::<dyn UserRepo>()? })
        })
        .mount(greeting)
}
```

The factory receives `Resources`, and `res.dep::<T>()` resolves a provider. Factories run at startup, in registration order (interleaved with `on_startup` hooks), before the server accepts connections:

- A factory can resolve every value registered with `provide`, `provide_arc` or `provide_dyn` (those are available immediately) and every factory registered **before** it.
- A factory that fails aborts startup. `App::run` returns an error that names the type (`dependency `...` failed to initialize: ...`), and the port is never bound.
- A factory returns `zapline::Result<T>`, so `?` works on anything that converts into `zapline::Error`.

For a real database, the pool is just another factory:

```rust
use std::sync::Arc;
use zapline::di::Dep;
use zapline::prelude::*;

struct Pool;

impl Pool {
    async fn connect(_url: &str) -> Result<Pool> {
        Ok(Pool)
    }
}

struct OrderService {
    pool: Arc<Pool>,
}

fn app() -> App {
    App::new()
        .provide_with(|_res: Resources| async move { Pool::connect("postgres://localhost/app").await })
        .provide_with(|res: Resources| async move {
            Ok(OrderService { pool: res.dep::<Pool>()? })
        })
}
```

To return a trait object from a factory, use `provide_dyn_with`. Annotate the `Arc<dyn Trait>` so the compiler knows which trait object you mean:

```rust
use std::sync::Arc;
use zapline::prelude::*;

trait Mailer: Send + Sync {
    fn send(&self, to: &str, body: &str);
}

struct SmtpMailer;

impl Mailer for SmtpMailer {
    fn send(&self, _to: &str, _body: &str) {}
}

fn app() -> App {
    App::new().provide_dyn_with(|_res: Resources| async move {
        let mailer: Arc<dyn Mailer> = Arc::new(SmtpMailer); // connect, authenticate, ...
        Ok(mailer)
    })
}
```

## Injecting by trait: `Inject`

When a service just collects other providers, implement `Inject` instead of writing a closure. The method is synchronous and runs at startup:

```rust
use std::sync::Arc;
use zapline::di::Inject;
use zapline::prelude::*;

struct Db;

struct Repo {
    db: Arc<Db>,
}

impl Inject for Repo {
    fn inject(res: &Resources) -> Result<Self> {
        Ok(Repo { db: res.dep::<Db>()? })
    }
}

fn app() -> App {
    App::new().provide(Db).provide_injected::<Repo>()
}
```

## Providers outside handlers

`Resources::dep::<T>()` resolves a provider anywhere you have `Resources`: startup hooks, shutdown hooks, scheduled jobs, background workers (see [Background tasks](tasks.md)) and plugins. In middleware, use `req.state::<Arc<T>>()`.

## Overriding providers in tests

Build the real app, then replace the pieces you don't want. Because the last registration wins, even a factory that has not run yet is replaced, and it never runs:

```rust
use std::sync::Arc;
use zapline::prelude::*;

trait UserRepo: Send + Sync {
    fn name(&self, id: u64) -> Option<String>;
}

struct MockRepo;

impl UserRepo for MockRepo {
    fn name(&self, _id: u64) -> Option<String> {
        Some("mock".to_owned())
    }
}

fn app() -> App {
    // The application, as built in production (see above).
    App::new().get("/users/{id}", |repo: zapline::di::Dep<dyn UserRepo>, Path(id): Path<u64>| async move {
        repo.name(id).ok_or_else(Error::not_found)
    })
}

#[tokio::test]
async fn greets_with_a_mock_repository() {
    let client = app()
        .override_provider::<dyn UserRepo>(Arc::new(MockRepo))
        .test();

    let res = client.get("/users/7").send().await;
    assert_eq!(res.text(), "mock");
}
```

`override_provider` is `provide_arc` with a name that says what you mean. See [Testing](testing.md) for the test client.

## Request scope: `Cached<T>`

Providers are singletons. For a value that is computed **per request** (the current user, the tenant, a database transaction), implement `FromRequestParts` for it and extract it as `Cached<T>`. The first extraction runs `T`'s extractor and stores the result in the request; later ones in the same request (in route middleware, then in the handler) get a clone:

```rust
use zapline::di::Cached;
use zapline::extract::FromRequestParts;
use zapline::prelude::*;

#[derive(Clone)]
struct Tenant(String);

impl FromRequestParts for Tenant {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        // Imagine a database lookup here: it runs once per request.
        let id = req.headers().get("x-tenant").and_then(|v| v.to_str().ok());
        id.map(|id| Tenant(id.to_owned()))
            .ok_or_else(|| Error::bad_request("missing x-tenant header"))
    }
}

/// Route middleware: reject unknown tenants early. The handler reuses the lookup.
async fn require_tenant(mut req: Request, next: Next) -> Response {
    match Cached::<Tenant>::resolve(&mut req).await {
        Ok(_) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

async fn dashboard(Cached(tenant): Cached<Tenant>) -> String {
    format!("dashboard of {}", tenant.0)
}

fn app() -> App {
    App::new().get("/dashboard", dashboard).wrap_route(require_tenant)
}
```

`T` must be `FromRequestParts + Clone + Sync + 'static`.

## `Dep<T>`, `State<T>` and `Resources`

All three read from the same typed registry (`Resources`), keyed by type:

| | Registered with | Extracted as | Notes |
|---|---|---|---|
| Plain state | `App::state(value)`, or `res.insert(value)` in a hook | `State<T>` | Cloned per request, so `T: Clone`. Fine for small configuration or handles that are already cheap to clone. |
| Provider | `App::provide*` | `Dep<T>` | Held as `Arc<T>`; `T` may be unsized (`dyn Trait`). Built by factories and replaceable in tests. |
| Typed settings | `App::config(settings)` | `State<T>` and `Dep<T>` | See [Configuration](config.md) |

A provider is stored under the key `Arc<T>`, which is why `State<Arc<T>>` reaches it too. Prefer `Dep<T>` for services and trait objects, `State<T>` for small `Clone` values, and `Resources` when you are outside a request.
