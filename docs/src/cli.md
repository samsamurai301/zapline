# Project layout & the CLI

The `zapline` CLI scaffolds projects, generates code, runs a live-reloading dev server, and talks to your app to print its routes or export its OpenAPI document. Install it with `cargo install zapline-cli`. You can call it as `zapline <cmd>` or `cargo zapline <cmd>`.

| Command | What it does |
|---|---|
| `zapline new <name>.rs` / `zapline run <file.rs>` | A [one-file app](one-file.md): a whole app in a single `.rs` file with no `Cargo.toml`. `dev`, `test`, `check` and `routes` take the file too |
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
| `zapline dev [--bin X] [--no-proxy] [-- app args]` | The [dev loop](#the-dev-loop-zapline-dev): template, translation and static file changes reload open pages without a restart, CSS is swapped in place, Rust changes rebuild in the background and swap in the new binary with no downtime, and compile errors show as an overlay in the browser. Sets `ZAPLINE_DEV=1`, so 500 responses show the real error. With Tailwind, also rebuilds `static/app.css` as you save. |
| `zapline css [--minify\|--watch]` | Build `static/app.css` from `styles/app.css` with Tailwind |
| `zapline check [--clippy]` | Type-check (or lint) all targets and print one line per error or warning: location, code, message, the span label and the first suggested fix |
| `zapline test [filter]` | Run the tests and print only the failures (with the start of each panic message) and a summary line. `filter` is a test-name filter, or a `tests/<file>` name |
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

## The dev loop: `zapline dev`

`zapline dev` is a Vite-style loop for a Rust app. Run it in the project and open the address your app prints (`http://localhost:8080` for the templates):

```console
$ zapline dev
    Watching src/, config/, migrations/, Cargo.toml, build.rs, .env
    Watching templates/, static/ (browser reload, no rebuild)
    Building todo
       Proxy http://localhost:8080 with live reload
     Running `target/debug/todo` (pid 4242) at http://localhost:8080
```

What each kind of change does:

| You change | What happens | Rust process |
|---|---|---|
| `templates/`, `locales/`, `i18n/`, files in `static/`, `public/`, `assets/` | Open pages reload, keeping their scroll position | Keeps running: templates and translations are read again on the next request |
| a `.css` file in `static/`, `public/`, `assets/` (including Tailwind's `static/app.css`) | The stylesheet is swapped in place, no page reload | Keeps running |
| `src/`, `config/`, `migrations/`, `Cargo.toml`, `build.rs`, `.env` | Rebuild in the background; pages show a small "Rebuilding" badge | The old version keeps serving until the new one answers, then gets SIGTERM |
| ...and the build fails | Pages show the compiler errors (file, line, rustc's message) as an overlay | The old version keeps serving |

The swap has no downtime: while cargo compiles, every request is still answered by the previous version. The new binary starts next to it, and requests move over only once it answers HTTP; the old process then finishes its in-flight requests and exits (SIGTERM, then a kill after 5 seconds). If the new version crashes during startup, the old one keeps serving and the overlay says so.

The overlay appears in every open page and in pages you open while the build is broken. Press Esc to hide it; the next good build clears it and reloads the page. A page opened before the very first build finishes shows "Building..." and reloads by itself once the app is up.

`zapline dev` sets `ZAPLINE_DEV=1` for the app, so 500 responses show the real error, templates and asset hashes are read again even in a `--release` build, and translations loaded with `I18n::load` are re-read when a file changes. `--no-proxy` turns all of this browser side off and runs the app directly: rebuild, stop the old process, start the new one.

### How it works

```text
browser ──▶ zapline dev proxy (the app's address, e.g. :8080)
              │  /__zapline/events   server-sent events: reload, css, problem, ...
              │  /__zapline/client.js
              └─▶ app on a free loopback port (127.0.0.1:41234)
```

1. `zapline dev` starts the app with `ZAPLINE_DEV_PROXY=<file>`. `App::run` then listens on a free loopback port instead of the address you gave it and writes both addresses to that file (`zapline::dev::ENV_DEV_PROXY`). Nothing else in the app changes, and without `ZAPLINE_DEV=1` the variable is ignored.
2. The CLI listens on the app's own address and forwards every request to the app. HTML pages get one tag before `</head>`: `<script src="/__zapline/client.js" defer>` (fragments such as htmx partials are left alone). An external script, so a `script-src 'self'` policy allows it; with a nonce-based Content-Security-Policy the page's nonce is reused. Pages are fetched uncompressed so the tag can be added; everything else (JSON, assets, streams, server-sent events, WebSocket upgrades) passes through untouched.
3. The client opens an `EventSource` on `/__zapline/events`. The CLI classifies each file change (Rust or asset), and pushes `reload`, `css`, `building`, `problem` (the parsed `cargo build --message-format=json` errors) or `clear`.
4. Each forwarded request opens its own connection to the app, so pointing the proxy at a new version takes effect on the very next request.

The cost in production is zero: the proxy is part of the CLI, and the app side is one environment check in `App::run` at startup. Some limits:

- Apps served with TLS (`ServerConfig::tls`), on a Unix socket, or with `App::serve` on your own listener are run directly (no proxy), as with `--no-proxy`.
- The browser talks HTTP/1.1 to the proxy; the app may still serve HTTP/2 to other clients.
- Each open tab holds one event-stream connection; browsers allow about six HTTP/1.1 connections per host, so keep fewer than that many tabs of the app open.
- The proxy remembers the app's address from the last run, so the overlay also works when the first build of a session fails. On the very first run in a project it learns the address when the app first starts.

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

When the app does not compile, `routes`, `openapi` and `db_migrate` answer with the compile errors (as `check` would) instead of "build failed", and when the app exits with an error, its last stderr lines are included.

Register it from the project directory:

```sh
claude mcp add zapline -- zapline mcp          # Claude Code
```

```json
{ "mcpServers": { "zapline": { "command": "zapline", "args": ["mcp"] } } }
```

The second form is the `mcp.json` / settings entry most other clients use.
