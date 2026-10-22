# Cheatsheet

One runnable file showing the canonical way to do each common task: models with validation, handlers, errors, dependency injection, middleware, route groups, OpenAPI, health checks and in-process tests. When there are several ways to do something, this is the one the guide, the CLI generators and `AGENTS.md` use.

```rust,ignore
{{#include ../../examples/examples/cheatsheet.rs}}
```

Run it with `cargo run -p zapline-examples --example cheatsheet` and open <http://localhost:8080/docs>.

## Everything else in one line each

| Task | How |
|---|---|
| Typed path params | `Path(id): Path<u64>`, `Path((org, repo)): Path<(String, String)>` |
| Query string / form | `Query(q): Query<T>`, `Form(f): Form<T>` |
| Validated input | `Valid(Json(body)): Valid<Json<T>>` (also `Valid<Query<T>>`, `Valid<Form<T>>`) |
| Plain shared state | `App::state(value)` + `State(v): State<T>` |
| Services / DI | `App::provide(value)` or `provide_dyn::<dyn Trait>(arc)` + `Dep<T>` |
| Errors | `Error::bad_request(msg)`, `not_found()`, `conflict(msg)`, `unauthorized()`, `forbidden()`, `.with_code("code")` |
| Domain errors | `impl From<MyError> for zapline::Error`, then `?` |
| HTML / redirect | `Html(string)`, `Redirect::to("/path")` |
| Route group + prefix | `App::nest("/api", Routes::new().mount((a, b)))` |
| Group middleware / auth | `Routes::wrap(mw)`, `.require_roles(["admin"])`, `.wrap(Authenticate::jwt())` |
| Config | `Config::load()?` reads `config/default.toml`, `config/{profile}.toml`, `APP__*` env and `.env` |
| Startup / shutdown | `App::on_startup(\|res\| async move { res.insert(pool); Ok(()) })` (then `State<Pool>`), `App::on_shutdown(..)` |
| Background work | `BackgroundTasks` extractor, `App::schedule(Schedule::every(dur), job)` |
| Realtime | `WebSocketUpgrade` (`ws` feature), `Sse` |
| Static files | `ServeDir::new("public")` (`fs` feature) |
| Database | `App::database(Database::from_env())` + `db: Db`, then `sqlx::query_as(..).fetch_one(&*db)` ([Databases](database.md)) |
| Migrations | `zapline db new create_posts`, applied at startup or with `zapline db migrate` |
| HTML + htmx | `App::views("templates")` + `views: Views, hx: HxRequest`, then `views.page_or_block(&hx, "page.html", "block", context!{..})` ([Views](views.md)) |
| Dev console | `App::dev_console()`: routes, recent requests, jobs, mail at `/_dev` (debug builds, localhost only) |
| Metrics | `.plugin(Metrics::new())` serves Prometheus `/metrics` |
| Tests | `app().test().get("/x").bearer(token).send().await` then `.status()`, `.json::<T>()`, `.text()` |
| CLI | `zapline new <app> --template fullstack`, `dev`, `generate resource post title body:text done:bool`, `g auth` (accounts + password reset), `g job <name>` (durable job), `db new/migrate`, `routes --json`, `openapi`, `client -o web/api.ts` (typed TS client), `check`, `test`, `mcp` (MCP server for AI assistants) |
| HTML forms | `#[serde(default, deserialize_with = "zapline::extract::empty_as_none")]` for optional inputs, `zapline::extract::checkbox` for checkboxes |
