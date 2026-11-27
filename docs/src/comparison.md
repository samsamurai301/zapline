# Coming from other frameworks

## FastAPI

| FastAPI | zapline |
|---|---|
| `@app.get("/items/{id}")` | `#[get("/items/{id}")]` |
| `def read(id: int, q: str \| None = None)` | `async fn read(Path(id): Path<i64>, Query(q): Query<Params>)` |
| Pydantic model | `#[model] struct` (derives serde, `JsonSchema` and `Validate`) |
| Automatic 422 validation errors | `Valid<Json<T>>` → 422 with per-field `details` |
| `/docs`, `/openapi.json` | `App::openapi(OpenApi::new("API", "1.0"))` |
| `Depends(get_db)` | `Dep<T>`, `State<T>`, custom extractors, `Cached<T>` |
| `BackgroundTasks` | `BackgroundTasks` |
| `HTTPException(404)` | `Err(Error::not_found())` |
| `TestClient(app)` | `app.test()` |
| `uvicorn --reload` | `zapline dev` |
| Lifespan events | `on_startup` / `on_shutdown` |

## Spring Boot / ASP.NET Core / NestJS

| Concept | zapline |
|---|---|
| Controllers | Modules of `#[get]`/`#[post]` handlers mounted with `.mount((...))`, or `Routes` groups |
| Beans and services, constructor injection | `App::provide`, `App::provide_dyn::<dyn Trait>`, async `provide_with` factories, the `Dep<T>` extractor |
| `application.yml` profiles | `Config::load()`: `config/default.toml` + `config/{profile}.toml` + `APP__*` env vars + `.env` |
| Filters, interceptors | Middleware: `App::wrap` (pre-routing), `App::wrap_route` / `Routes::wrap` (post-routing) |
| Guards, `@PreAuthorize` | `.require_roles([...])`, `.require_scopes([...])`, `Guard` |
| Actuator (health, metrics) | `Health` and `Metrics` plugins |
| `@Scheduled` | `App::schedule(Schedule::every(..), job)` |
| Exception handlers | `App::error_handler` |
| Modules / starters | `Plugin`s |

## axum

zapline will feel familiar. Extractors, `IntoResponse` and `State` work the same way. The differences:

- `App::run(addr)` gives you a tuned thread-per-core server, with no `#[tokio::main]` and no `serve` glue.
- There's no tower `Service`/`Layer` stack. Middleware is `async fn(Request, Next) -> Response`.
- State is keyed by type, so you can register many types and the router has no `S` generic parameter.
- OpenAPI, validation, DI, configuration, auth, sessions, metrics and health checks are built in.

## actix-web

- Route macros look the same (`#[get("/")]`), and they also document the route in OpenAPI.
- Handlers and state are `Send`, so any Tokio-ecosystem library works.
- Both use thread-per-core runtimes. zapline uses `SO_REUSEPORT` listeners and hyper's HTTP stack.
