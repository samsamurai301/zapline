# State, lifecycle & plugins

## State

Register any `Clone + Send + Sync + 'static` value and extract it with `State<T>`:

```rust
#[derive(Clone)]
struct AppConfig { greeting: String }

let app = App::new()
    .state(AppConfig { greeting: "hi".into() })
    .get("/", |State(cfg): State<AppConfig>| async move { cfg.greeting });
```

State is keyed by type, so you can register as many types as you want. `State<T>` clones `T` for each request, so wrap large or mutable data in an `Arc` (connection pools are already cheap to clone). For trait objects and services, prefer [`Dep<T>`](di.md).

## Startup and shutdown hooks

Resources that need async setup, such as database pools, caches, clients or warmed data, belong in a startup hook. Hooks run in registration order before the server accepts connections. Anything a hook inserts into `Resources` becomes available as `State<T>`:

```rust
App::new()
    .on_startup(|res: Resources| async move {
        let url = res.require::<Settings>()?.database_url;
        res.insert(Db::connect(&url).await?);      // -> State<Db>
        Ok(())
    })
    .on_shutdown(|res: Resources| async move {
        if let Some(db) = res.get::<Db>() { db.close().await; }
    })
```

- If a startup hook fails, `App::run` returns the error and never binds the port.
- Shutdown hooks run in **reverse** order, after the server has drained its connections.
- Hooks run on a small long-lived *system runtime*. Resources created there, like pool connections and background jobs, keep being driven for the life of the server.
- In tests, `App::test()` runs startup hooks lazily on the first request. Call `client.start().await` to run them eagerly, and `client.shutdown().await` to run the shutdown hooks.

## Plugins

A plugin is a reusable bundle of routes, middleware, state and hooks. Any function `FnOnce(App) -> App` is a plugin:

```rust
pub fn admin_console(app: App) -> App {
    app.nest("/admin", admin_routes())
       .on_startup(|res| async move { /* ... */ Ok(()) })
}

App::new().plugin(admin_console);
```

For configurable plugins, implement the `Plugin` trait on a struct. The built-in `OpenApi`, `Health` and `Metrics` plugins all do this.

### Build hooks

`App::on_build(|routes: &[&'static RouteInfo]| -> Routes)` runs once the route table is final. A plugin can inspect every route and its metadata, then add more routes. This is how `/openapi.json` is generated once at startup instead of on each request:

```rust
App::new().on_build(|routes| {
    let listing: Vec<String> = routes.iter().map(|r| r.path().to_owned()).collect();
    Routes::new().get("/_routes", move || {
        let body = listing.join("\n");
        async move { body }
    })
})
```

### Builder-time storage

`App::extensions_mut()` is a typed map that plugins can use to share configuration while the app is being assembled.
