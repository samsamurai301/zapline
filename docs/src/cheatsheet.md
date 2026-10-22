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
| Live updates | `App::live(Live::new())` + `live: Live`; `live.publish("topic", "event", html)`; handler returns `live.sse("topic").into_response()`; htmx: `hx-ext="sse" sse-connect="/live/topic" sse-swap="event"` ([Live updates](live.md)) |
| Realtime | `WebSocketUpgrade` (`ws` feature), `Sse` |
| Static files | `ServeDir::new("public")` (`fs` feature) |
| Database | `App::database(Database::from_env())` + `db: Db`, then `sqlx::query_as(..).fetch_one(&*db)` ([Databases](database.md)) |
| List endpoints | `params: ListParams` + `Listing::new("posts", "id, title").sortable(["title"]).search(["title"]).filter_bool("done").fetch(&db, &params).await?` returns `Paged<T>` (`?limit=&offset=&sort=-id&q=&done=true`) |
| Migrations | `zapline db new create_posts`, applied at startup or with `zapline db migrate` |
| HTML + htmx | `App::views("templates")` + `views: Views, hx: HxRequest`, then `views.page_or_block(&hx, "page.html", "block", context!{..})` ([Views](views.md)) |
| Admin panel | `App::admin(\|admin\| admin)`: every table at `/admin` (debug + localhost); production: `.protect(middleware)`; `.exclude([..])`, `.hide(["users.email"])`, `.read_only()` ([Admin panel](admin.md)) |
| Translations | `App::i18n(I18n::load("locales")?)` (`locales/en.toml`: `hello = "Hi, {name}!"`) + `t: Locale`, then `t.t_with("hello", &[("name", &n)])`, `t.plural("items", n, &[])`; templates: `context! { t }`, `{{ t("hello", name=n) }}`; language from `?lang=`, cookie, `Accept-Language` ([Translations](i18n.md)) |
| Several instances | `let redis = Redis::from_env()?` (`REDIS_URL`); `Cache::redis(redis.clone())`, `Live::new().redis(..)`, `RateLimit::per_ip(..).redis(..)`, `SessionLayer::new(RedisStore::new(..))`; `redis.cmd(["INCR", "k"]).await?.int()` ([Several instances](scaling.md)) |
| OpenTelemetry | feature `otlp`, then `OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4318` (or `Telemetry::new().otlp(url)`); requests become spans, `traceparent` is continued ([Observability](observability.md#traces-to-opentelemetry-otlp)) |
| Caching | `App::cache(Cache::memory())` + `cache: Cache`, then `cache.get_or_set("key", ttl, \|\| async { Ok(v) })`; pages: `Routes::wrap(ResponseCache::new(ttl))`; `App::wrap(ETag)` ([Caching](caching.md)) |
| Files | `App::storage(Storage::from_env()?)` + `storage: Storage`, then `put(key, bytes)`, `get`, `delete`, `url(key, ttl)` (local or S3, [File storage](storage.md)) |
| Dev console | `App::dev_console()`: routes, recent requests, jobs, mail at `/_dev` (debug builds, localhost only) |
| Metrics | `.plugin(Metrics::new())` serves Prometheus `/metrics` |
| Tests | `app().test().get("/x").bearer(token).send().await` then `.status()`, `.json::<T>()`, `.text()` |
| CLI | `zapline new <app> --template fullstack`, `dev`, `generate resource post title body:text done:bool`, `g auth` (accounts, password reset, GitHub/Google login), `g job <name>` (durable job), `db new/migrate`, `routes --json`, `openapi`, `client -o web/api.ts` (typed TS client), `check`, `test`, `mcp` (MCP server for AI assistants) |
| HTML forms | `#[serde(default, deserialize_with = "zapline::extract::empty_as_none")]` for optional inputs, `zapline::extract::checkbox` for checkboxes |
