# Project layout & the CLI

The `zapline` CLI scaffolds projects, generates code, runs a live-reloading dev server, and talks to your app to print its routes or export its OpenAPI document. Install it with `cargo install zapline-cli`. You can call it as `zapline <cmd>` or `cargo zapline <cmd>`.

| Command | What it does |
|---|---|
| `zapline new <name> [-t minimal\|api\|full\|fullstack] [--path DIR]` | Create a project. `api` (the default) includes routes, models, validation, OpenAPI, Docker and CI. `full` adds layered config, DI, health and metrics probes, tracing and a hardened middleware stack. `fullstack` is server-rendered HTML with htmx, SQLite (zero setup) with migrations, and a JSON API. |
| `zapline generate resource <name> [field:type ...]` (`g resource`) | Add a resource and mount it automatically. In a project with `migrations/`: a SQL migration, sqlx CRUD API, an htmx page (when `templates/base.html` exists) and tests. Otherwise: CRUD handlers over an in-memory store. Types: `string`, `text`, `int`, `float`, `bool`; `?` for optional. |
| `zapline db new <name>` / `zapline db migrate` | Create a SQL migration / apply pending ones ([Databases](database.md)) |
| `zapline db seed` | Insert the app's [seed data](database.md#seed-data) (`App::seed`) after migrating |
| `zapline g handler <name> --method get --path /x` | Add a single route |
| `zapline g middleware <name>` | Add a middleware function |
| `zapline g job <name>` | A [durable job](jobs.md): `src/jobs/<name>.rs`, registered in `src/jobs/mod.rs` and `app()`, plus a test |
| `zapline g auth` | Add accounts: users migration, sign-up/log-in/log-out, password reset by email (JSON API and htmx pages), database sessions, a `CurrentUser` extractor and tests ([Accounts](auth.md#accounts-in-one-command-zapline-g-auth)) |
| `zapline g deploy fly\|railway\|render\|compose` | [Hosting config](deployment.md#hosting-configs-zapline-g-deploy) for the project's Dockerfile, with volumes, databases, health checks and secrets filled in |
| `zapline g tailwind` | [Tailwind CSS without Node](files.md#tailwind-css-without-node): `styles/app.css` compiled to `static/app.css` by the standalone Tailwind binary |
| `zapline dev [--bin X] [-- app args]` | Rebuild and restart on changes to `src/`, `config/`, `migrations/` and `Cargo.toml` (templates reload without a restart). The old process gets SIGTERM, then a kill after 5 seconds. Sets `ZAPLINE_DEV=1`, so 500 responses show the real error on the [dev error page](errors.md#the-dev-error-page), and records the app's address in `target/zapline-dev.addr` for `zapline mcp`. With Tailwind, also rebuilds `static/app.css` as you save. |
| `zapline css [--minify\|--watch]` | Build `static/app.css` from `styles/app.css` with Tailwind |
| `zapline check [--clippy]` | Type-check (or lint) all targets and print one line per error or warning: location, code, message, the span label and the first suggested fix |
| `zapline test [filter]` | Run the tests and print only the failures (with the start of each panic message) and a summary line. `filter` is a test-name filter, or a `tests/<file>` name |
| `zapline explain [ZL0002]` | What a [diagnostic code](errors.md#diagnostic-codes) means and the code that fixes it; without a code, the list |
| `zapline mcp` | Serve these tools to AI assistants over the Model Context Protocol ([below](#ai-assistants-zapline-mcp)) |
| `zapline routes [--json]` | Print the route table |
| `zapline openapi [-o openapi.json]` | Export the OpenAPI document |
| `zapline client [-o client.ts]` | Generate a typed TypeScript client for the JSON API ([below](#typescript-client-zapline-client)). `zapline dev --client <file>` keeps it up to date |
| `zapline doctor` | Check rustc and cargo versions, git, docker and wrk |
| `zapline bench <url> [-c 256 -d 10s -t N]` | Load-test with wrk |

`-C <dir>` runs any command as if it had been started in `<dir>`.

## Generated layout (`api` template)

```text
my-api/
├── Cargo.toml
├── src/
│   ├── main.rs          # thin: builds the app and runs it
│   ├── lib.rs           # pub fn app() -> App  (tests use it too)
│   ├── models.rs
│   └── routes/
│       ├── mod.rs       # install(app): plugins between zapline:routes markers
│       ├── health.rs
│       └── users.rs     # pub fn mount(app: App) -> App
├── tests/               # in-process tests using App::test()
├── Dockerfile, .dockerignore, .env.example
└── .github/workflows/ci.yml
```

Each route module exposes `pub fn mount(app: App) -> App`, which makes it a [plugin](lifecycle.md#plugins). `routes/mod.rs` installs the modules between `// zapline:routes:start` and `// zapline:routes:end`. `zapline generate resource` inserts new modules there.

## How `routes` and `openapi` work

Both commands run your app with an environment variable. The app prints or writes what was asked for and exits before binding a port or running startup hooks:

- `ZAPLINE_PRINT_ROUTES=1`: `App::run` prints the route table as JSON and exits.
- `ZAPLINE_EXPORT_OPENAPI=<path>`: the OpenAPI plugin writes the spec and exits.

So the commands are safe to run without databases or other services, for example in CI to check a committed `openapi.json`.

## TypeScript client: `zapline client`

`zapline client -o web/src/api.ts` turns the app's OpenAPI document into one TypeScript file with no dependencies (it uses `fetch`, so it runs in browsers, Node 18+, Deno and Bun):

- an `interface` per schema, with the doc comments of your Rust types;
- `createClient({ baseUrl?, headers?, fetch? })`, returning one method per operation, named after its `operationId` (the handler name, prefixed with its module when two modules share a name: `usersList`, `postsList`). Path parameters come first, then the body, then a `query` object;
- `ApiError` for non-2xx responses, with `status`, and zapline's `code` and `details`.

```ts
import { createClient, ApiError } from "./api";

const api = createClient({ headers: () => ({ authorization: `Bearer ${token}` }) });
const user = await api.createUser({ name: "Ada", email: "ada@example.com" });
try {
  await api.getUser(999);
} catch (e) {
  if (e instanceof ApiError && e.status === 404) { /* ... */ }
}
```

Routes that answer with HTML (htmx pages and fragments) are left out. The file passes `tsc --strict --exactOptionalPropertyTypes --noUncheckedIndexedAccess` and is only rewritten when it changes.

With `zapline dev --client web/src/api.ts`, the client is regenerated after every successful build, before the app restarts. Change a Rust struct, save, and your frontend's type checker sees the change: end-to-end types without a shared schema language.

## AI assistants: `zapline mcp`

`zapline mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server on stdin/stdout. It lets Claude Code, Cursor, Zed, VS Code and other MCP clients call zapline directly, and every tool answers in as few tokens as it can:

| Tool | Returns |
|---|---|
| `docs` | No topic: the cheatsheet and the list of guide pages. With a topic (`"database migrations"`, `"htmx"`, `"errors"`): only the best-matching guide sections, from a copy of the guide built into the binary (no network) |
| `new_project` | Creates a project from a template |
| `generate_resource`, `generate_handler`, `generate_middleware`, `generate_auth` | The files created and edited |
| `db_new_migration` | Creates a migration, with its SQL if given |
| `db_migrate` | Applies pending migrations |
| `check`, `test` | The condensed output of `zapline check` / `zapline test` |
| `routes` | One `METHOD path` line per route |
| `openapi` | The OpenAPI document, compact |
| `typescript_client` | Writes the TypeScript client |
| `explain_error` | A [diagnostic code](errors.md#diagnostic-codes)'s cause and fix |
| `last_results` | The latest `test` and `check` results with their age, without running anything again |

When the app does not compile, `routes`, `openapi` and `db_migrate` answer with the compile errors (as `check` would) instead of "build failed", and when the app exits with an error, its last stderr lines are included. Any answer that mentions a diagnostic code (`ZL0004` in a startup error, a panic in a test) ends with that code's fix.

### Live tools: debugging the running app

While the app runs under `zapline dev` with the [dev console](dev-console.md), the assistant can look at it directly instead of asking you to copy logs:

| Tool | Returns |
|---|---|
| `recent_errors` | The latest errors, newest first: code, status, request, the real message, the panic location and the fix. `ZL0002 500 GET /orders: no provider registered for `app::Repo`` then `  fix: .provide(value) // value: app::Repo; ...` |
| `recent_requests` | One `500 GET /orders 0.8ms ZL0002` line per request |
| `call_route` | Sends a request (`method`, `path`, `body`, `headers`) and returns the status line, the headers worth reading (content type, location, cookies without their values, `x-zapline-diagnostic`) and the body, JSON compacted and cut at 2,000 bytes. A 500 comes back as problem+json with its code and fix |

They find the app through `target/zapline-dev.addr`, which `zapline dev` makes the app write (or `$ZAPLINE_URL`, `$ADDR`/`$PORT`, then `http://127.0.0.1:8080`; each tool also takes a `url`). The console only answers requests from this machine, and nothing here exists in a release build. A typical loop: the assistant edits a handler, `zapline dev` restarts the app, `call_route` shows a 500 with `ZL0044`, the fix says `.database(Database::from_env())`, the assistant adds it, and `call_route` returns 200.

Register it from the project directory:

```sh
claude mcp add zapline -- zapline mcp          # Claude Code
```

```json
{ "mcpServers": { "zapline": { "command": "zapline", "args": ["mcp"] } } }
```

The second form is the `mcp.json` / settings entry most other clients use.
