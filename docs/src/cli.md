# Project layout & the CLI

The `zapline` CLI scaffolds projects, generates code, runs a live-reloading dev server, and talks to your app to print its routes or export its OpenAPI document. Install it with `cargo install zapline-cli`. You can call it as `zapline <cmd>` or `cargo zapline <cmd>`.

| Command | What it does |
|---|---|
| `zapline new <name>.rs` / `zapline run <file.rs>` | A [one-file app](one-file.md): a whole app in a single `.rs` file with no `Cargo.toml`. `dev`, `test`, `check` and `routes` take the file too |
| `zapline new <name> [-t minimal\|api\|full\|fullstack] [--path DIR]` | Create a project. `api` (the default) includes routes, models, validation, OpenAPI, Docker and CI. `full` adds layered config, DI, health and metrics probes, tracing and a hardened middleware stack. `fullstack` is server-rendered HTML with htmx, SQLite (zero setup) with migrations, database sessions with flash messages, and a JSON API. |
| `zapline generate resource <name> [field:type ...]` (`g resource`) | Add a resource and mount it automatically. In a project with `migrations/`: a SQL migration with `created_at`/`updated_at`, a sqlx JSON API (list, get, create, `PUT`, `PATCH`, delete), htmx index, show and edit pages (when `templates/base.html` exists; rejected posts come back with a message per field, saves set a flash message) and tests. Otherwise: CRUD handlers over an in-memory store. Types: `string`, `text`, `int`, `float`, `bool`, `date`, `datetime`, `uuid`, `references`; `?` for optional ([details](#database-backed-resources)). |
| `zapline g migration <name> [field:type[=default] ...]` | A reversible migration (`.up.sql` + `.down.sql`) from its name: `add_<cols>_to_<table>` adds columns, `remove_<cols>_from_<table>` drops them, `create_<table>` creates a table. Adding or removing columns of a `g resource` table updates its module, pages and test too ([details](#migrations-from-the-command-line)) |
| `zapline db new <name> [--reversible\|--one-way]` / `zapline db migrate` | Create a SQL migration (an `.up.sql` + `.down.sql` pair) / apply pending ones ([Databases](database.md#migrations)) |
| `zapline db status` | Every migration: version, name, when it was applied, whether its file changed since, and whether it has a down file. Fails, naming the file to restore, when an applied migration was edited or deleted |
| `zapline db rollback [--steps N]` / `zapline db redo` | Revert the last N migrations with their `.down.sql` files / revert the last one and apply it again |
| `zapline db reset` | Revert every migration, apply them all and insert the seed data. Development only: `rollback`, `redo` and `reset` refuse `--release` and `APP_ENV=production` unless `--force` ([guard rails](database.md#guard-rails)) |
| `zapline db seed` | Insert the app's [seed data](database.md#seed-data) (`App::seed`) after migrating |
| `zapline g handler <name> --method get --path /x` | Add a single route |
| `zapline g middleware <name>` | Add a middleware function |
| `zapline g job <name>` | A [durable job](jobs.md): `src/jobs/<name>.rs`, registered in `src/jobs/mod.rs` and `app()`, plus a test |
| `zapline g mailer <name>` | An [email from templates](mail.md#templated-email-zapline-g-mailer): `src/mail/<name>.rs` (declared in `src/mail/mod.rs`), `templates/mail/<name>.html` and `.txt`, and a test against the outbox mailer; enables the `mail` and `views` features |
| `zapline g auth` | Add accounts: users migration, sign-up/log-in/log-out, password reset by email (JSON API and htmx pages), database sessions, a `CurrentUser` extractor and tests ([Accounts](auth.md#accounts-in-one-command-zapline-g-auth)) |
| `zapline g deploy fly\|railway\|render\|compose` | [Hosting config](deployment.md#hosting-configs-zapline-g-deploy) for the project's Dockerfile, with volumes, databases, health checks and secrets filled in |
| `zapline add <feature>...` / `zapline add --list` | Turn on zapline [features](features.md) in `Cargo.toml` (`zapline add ws redis`), whatever shape the `zapline` dependency has. The list stays sorted, the rest of the file is untouched, features already on are reported and skipped, and a misspelled name gets a suggestion (`wss`: did you mean `ws`?). `--list` prints every feature with a one-line description and marks the ones the project has |
| `zapline g tailwind` | [Tailwind CSS without Node](files.md#tailwind-css-without-node): `styles/app.css` compiled to `static/app.css` by the standalone Tailwind binary |
| `zapline dev [--bin X] [--no-proxy] [-- app args]` | The [dev loop](#the-dev-loop-zapline-dev): template, translation and static file changes reload open pages without a restart, CSS is swapped in place, Rust changes rebuild in the background and swap in the new binary with no downtime, and compile errors show as an overlay in the browser. Sets `ZAPLINE_DEV=1`, so 500 responses show the real error on the [dev error page](errors.md#the-dev-error-page), and records the app's address in `target/zapline-dev.addr` for `zapline mcp`. With Tailwind, also rebuilds `static/app.css` as you save. |
| `zapline ship <user@host> [--dry-run] [--static] [--rollback]` | [Build a release binary, copy it over ssh and swap it in](deployment.md#one-binary-one-command) next to the running one on the same port, with systemd. The first run creates the user, `/etc/<app>/env` (with generated secrets) and the unit. Any failure leaves the old release serving. `--env K=V` sets a setting, `--keep N` keeps N releases |
| `zapline css [--minify\|--watch]` | Build `static/app.css` from `styles/app.css` with Tailwind |
| `zapline check [--clippy]` | Type-check (or lint) all targets and print one line per error or warning: location, code, message, the span label and the first suggested fix. When an error comes from a zapline feature the project has not enabled, it says which ([below](#missing-features)) |
| `zapline test [filter]` | Run the tests and print only the failures (with the start of each panic message) and a summary line. `filter` is a test-name filter, or a `tests/<file>` name |
| `zapline fuzz [--seed N] [--cases N]` | Run `tests/fuzz.rs`, which [fuzzes every route](testing.md#fuzz-every-route) from its OpenAPI schemas, and print the report: each panic, 5xx, timeout, undeclared status or response that breaks its schema, with a minimal reproducing request. The report is kept for the MCP `last_results` tool |
| `zapline explain [ZL0002]` | What a [diagnostic code](errors.md#diagnostic-codes) means and the code that fixes it; without a code, the list |
| `zapline mcp` | Serve these tools to AI assistants over the Model Context Protocol ([below](#ai-assistants-zapline-mcp)) |
| `zapline routes [--json]` | Print the route table |
| `zapline openapi [-o openapi.json]` | Export the OpenAPI document |
| `zapline dev --record` | Same, with `ZAPLINE_RECORD=1`: the dev console records requests for `zapline snap` |
| `zapline snap <name> [--url URL]` | Save what the running app recorded as `tests/snapshots/<name>.json`, plus `tests/snapshots.rs` the first time ([snapshot tests](testing.md#snapshot-tests-from-real-traffic)). The URL defaults to the app's `ADDR`/`PORT`, else `http://127.0.0.1:8080` |
| `zapline client [-o FILE]` | Generate a typed TypeScript (`.ts`) or JavaScript (`.js`) client for the JSON API ([below](#typed-clients-zapline-client)). `zapline dev` keeps it up to date |
| `zapline doctor` | Check rustc and cargo versions, git, docker and wrk |
| `zapline bench <url> [-c 256 -d 10s -t N]` | Load-test with wrk |

`-C <dir>` runs any command as if it had been started in `<dir>`.

## Missing features

Most of zapline is opt-in, so code copied from the guide can need a feature the project does not have yet. Plain `cargo check` only says `` could not find `ws` in `zapline` ``. `zapline check` and `zapline test` add the fix:

```text
error[E0432] src/routes/chat.rs:1:14: unresolved import `zapline::ws`
    could not find `ws` in `zapline`
    hint: enable the `ws` feature: zapline add ws
error[E0599] src/lib.rs:12:10: no method named `views` found for struct `zapline::App` in the current scope
    method not found in `zapline::App`
    hint: enable the `views` feature: zapline add views
error[E0425] src/routes/posts.rs:8:20: cannot find type `Db` in this scope
    not found in this scope
    hint: enable the `sqlite` or `postgres` feature: zapline add sqlite
```

The hint covers unresolved imports and paths (`zapline::ws`, `zapline::auth::Jwt`), unknown types (`WebSocketUpgrade`, `Views`, `Db`, `JwtConfig`, `Cache`...) and builder methods that a feature adds (`App::views`, `App::database`, `Storage::s3`...). It only appears when none of the features that provide the name is on, so an error that has another cause gets no hint. The `check` and `test` MCP tools show the same hint, and `add_feature` applies it.

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
5. The proxy tells the app who the browser is with `X-Forwarded-For`, plus a random per-run key (`ZAPLINE_DEV_PROXY_KEY` in the app's environment, `x-zapline-dev-proxy` on each request). The app's local-only pages (the [dev console](dev-console.md) at `/_dev`, the dev inbox, dev error pages, an unguarded admin) trust that `X-Forwarded-For` only when the key matches, and answer only when it names a single loopback address. So they work through the proxy from your machine, while a client reaching a proxy bound to `0.0.0.0` from elsewhere, or anything else that sends a forwarding header, still gets a 404.

The cost in production is zero: the proxy is part of the CLI, and the app side is one environment check in `App::run` at startup. Some limits:

- Apps served with TLS (`ServerConfig::tls`), on a Unix socket, or with `App::serve` on your own listener are run directly (no proxy), as with `--no-proxy`.
- The browser talks HTTP/1.1 to the proxy; the app may still serve HTTP/2 to other clients.
- Each open tab holds one event-stream connection; browsers allow about six HTTP/1.1 connections per host, so keep fewer than that many tabs of the app open.
- The proxy remembers the app's address from the last run, so the overlay also works when the first build of a session fails. On the very first run in a project it learns the address when the app first starts.

## Database-backed resources

In a project with `migrations/` (the `fullstack` template), `zapline g resource` writes everything a Rails or Phoenix scaffold does, as plain code you own:

```sh
zapline g resource author name
zapline g resource post title body:text published_on:date author:references
```

| File | Contents |
|---|---|
| `migrations/<ts>_create_posts.sql` | The table with `id`, the fields, `created_at` and `updated_at` (defaulting to the current time), a foreign key and an index per reference |
| `src/routes/posts.rs` | `Post`, `NewPost` (create/replace, JSON or form) and `UpdatePost` (all fields optional, for `PATCH`); sqlx queries; a `listing()` with sorting, search and filters; the JSON API under `/api/posts`; the pages |
| `templates/posts/index.html`, `_row.html` | The list with an add form; each row has edit and delete buttons |
| `templates/posts/show.html`, `edit.html` | `GET /posts/{id}` and `GET /posts/{id}/edit`. Edit swaps the row for a row of inputs (htmx), and saving sends `PUT /posts/{id}`, which answers with the updated row. Visited directly, both are whole pages |
| `tests/posts.rs` | CRUD, `PATCH`, validation, the pages and the nested list, against an in-memory SQLite database |

| Type | Rust | SQLite | Postgres | Form input |
|---|---|---|---|---|
| `string` (default) | `String`, 1 to 200 chars | `TEXT` | `TEXT` | text |
| `text` | `String` | `TEXT` | `TEXT` | textarea |
| `int`, `float` | `i64`, `f64` | `BIGINT`, `DOUBLE PRECISION` | same | number |
| `bool` | `bool` | `BOOLEAN` | `BOOLEAN` | checkbox |
| `date` | `chrono::NaiveDate` | `TEXT` | `DATE` | `type=date` |
| `datetime` | `chrono::DateTime<Utc>` | `TEXT` | `TIMESTAMPTZ` | `type=datetime-local` (read as UTC) |
| `uuid` | `uuid::Uuid` | `BLOB` (how sqlx stores it) | `UUID` | text |
| `json` | `JsonValue` (`serde_json::Value`) | `TEXT` | `JSONB` | textarea holding JSON |
| `author:references` | `author_id: i64` | `BIGINT REFERENCES authors(id) ON DELETE CASCADE` | same | `<select>` of authors |
| `author:ref:users` | `author_id: i64` | `BIGINT REFERENCES users(id) ON DELETE CASCADE` | same | `<select>` of users |

Add `?` for an optional field: a nullable column and an `Option<T>`; an optional reference uses `ON DELETE SET NULL`. Dates, times and UUIDs come from `zapline::chrono` and `zapline::uuid`; the generator turns on zapline's `chrono` (and `uuid`) feature in `Cargo.toml`, so you don't add those crates yourself. A `json` field takes any JSON value in the API; in forms its textarea shows the stored value as JSON and [`zapline::extract::json_text`](database.md#json-columns) parses what comes back (malformed JSON is a `422` naming the field). JSON fields are not sortable or filterable.

### Resource fields

Modifiers go after the type, before or after the `?`:

```sh
zapline g resource user email:string:unique name
zapline g resource review body:text product:references rating:int:index
```

| Modifier | Migration | API |
|---|---|---|
| `:unique` (`email:string:unique`) | `CREATE UNIQUE INDEX users_email_key ON users (email)` | a duplicate is `409` with code `email_taken` |
| `:index` (`rating:int:index`) | `CREATE INDEX reviews_rating_idx ON reviews (rating)` | |

Unique columns get a unique index rather than an inline `UNIQUE`, so a later `zapline g migration remove_email_from_users` can drop the column on SQLite too. A `bool` can't be unique. References are always indexed.

A reference gets a `?author_id=` filter on the list, `GET /api/authors/{id}/posts` (the author's posts, paged like the main list; `404` for an unknown author), and an author `<select>` in the forms that shows each author's `name` (or `title`, `label`, `email`, `username`, the first required text column, or the id: read from the authors migration). Rows link to the author's page when it has one. Creating or updating a post for an author that doesn't exist answers `422` with code `author_not_found`, before the generic `409 foreign_key_violation` would.

- **Another table name**: `author:ref:users` names the column and the error code after `author` (`author_id`, `author_not_found`) and points the foreign key at `users`.
- **Generate the parent first**: when no migration creates the parent's table, the generator stops with `run zapline g resource author first` and writes nothing (Postgres won't create a foreign key to a missing table, and the migration must run after the parent's).
- **The same table**: an optional reference to the resource's own table works, `zapline g resource category name parent:ref:categories?` (or `category:references?`), with `GET /api/categories/{id}/categories` listing the children. A required one is refused: the first row would have nothing to point at.
- **Two references to one table** (`author:ref:users editor:ref:users?`) get no nested route, since `/api/users/{id}/posts` would be ambiguous; the generator says so, and `?author_id=` filters the list instead.

`PUT` replaces every field; `PATCH` takes any subset (`{"title": "New"}`) and leaves the rest. Both set `updated_at`.

## Migrations from the command line

`zapline g migration` writes the SQL for the project's dialect from the migration's name, with the same field syntax. Each migration is a reversible pair: the `.up.sql` file and a `.down.sql` file that undoes it (`DROP COLUMN`, `ADD COLUMN`, `DROP TABLE`), which [`zapline db rollback`](database.md#status-rollback-redo-reset) runs:

```sh
zapline g migration add_published_at_to_posts published_at:datetime?
zapline g migration add_views_to_posts views:int=0     # the default existing rows get
zapline g migration add_email_to_users                 # the column from the name: email TEXT
zapline g migration add_category_to_posts category:references?
zapline g migration remove_legacy_flag_from_posts
zapline g migration create_tags name slug              # id, fields, created_at, updated_at
```

```sql
-- migrations/20261023101500_add_published_at_to_posts.up.sql
ALTER TABLE posts ADD COLUMN published_at TIMESTAMPTZ;
-- migrations/20261023101500_add_published_at_to_posts.down.sql
ALTER TABLE posts DROP COLUMN published_at;
```

A required column added to a table that has rows needs a value for them: the one after `=` (`views:int=0`, `status=draft`, `due:date=2026-01-01`, `pinned:bool=true`), otherwise text gets `DEFAULT ''`, numbers `DEFAULT 0` (with a note naming the file, in case that is not what you want) and booleans `FALSE`. Dates, UUIDs and references have no zero value, so give one with `=` or add them as optional (`due:date?`), fill them in, then tighten the column in a later migration. On SQLite, `DROP COLUMN` needs 3.35 or newer (sqlx bundles one). Any other name with fields creates that table; without fields you get an empty pair (like `zapline db new`). Rolling back a `remove_` migration brings the columns back empty: optional, or with their defaults.

### The resource follows the table

When `zapline g resource` made the table, the module records what it was generated from on a `// zapline:resource post title:string ...` line near its top. `add_<cols>_to_<table>` and `remove_<cols>_from_<table>` then update `src/routes/<table>.rs`, its pages (`templates/<table>/*.html`) and its test (`tests/<table>.rs`): the structs, every query and its binds, `listing()`'s sort, search and filters, the forms, the table and the show page, the test bodies. The app keeps compiling, and `crud_round_trip` checks the new columns through POST, GET, the list, PUT and PATCH.

The update is a three-way merge, like `git merge-file`: the resource is generated with the old fields and with the new ones, and the difference is applied to your files, so your own edits stay. The result is what `g resource` would write today with the new fields, plus your changes. A file you edited right where the new code goes is left untouched, and the lines to add or remove are printed; if that file is the module, no file is changed, so the module, pages and test never disagree. A hand-written module without the `// zapline:resource` line (such as the `fullstack` template's `todos`, or one generated by an older zapline) gets the fields to add instead. Removing a column of a generated resource also gives the down file the column's real type.

## How `routes` and `openapi` work

Both commands run your app with an environment variable. The app prints or writes what was asked for and exits before binding a port or running startup hooks:

- `ZAPLINE_PRINT_ROUTES=1`: `App::run` prints the route table as JSON and exits.
- `ZAPLINE_EXPORT_OPENAPI=<path>`: the OpenAPI plugin writes the spec and exits.

So the commands are safe to run without databases or other services, for example in CI to check a committed `openapi.json`.

## Typed clients: `zapline client`

`zapline client -o web/src/api.ts` turns the app's OpenAPI document into one TypeScript file with no dependencies (it uses `fetch`, so it runs in browsers, Node 18+, Deno and Bun):

- an `interface` per schema, with the doc comments of your Rust types;
- `createClient({ baseUrl?, headers?, fetch?, credentials?, csrf? })` and a ready same-origin `api`, with one method per operation, named after its `operationId` (the handler name, prefixed with its module when two modules share a name: `usersList`, `postsList`). Path parameters come first, then the body, then a `query` object, then per-call options (`{ signal, headers }`);
- `ApiError` for non-2xx responses, with `status`, and zapline's `code` and `details`; a `ValidationError` for 422s, whose `fields` are typed by the request body.

```ts
import { api, ApiError, isValidationError } from "./api";

try {
  await api.createUser({ name: "", email: "ada@example.com" });
} catch (e) {
  if (isValidationError(e, "createUser")) e.fields.name;   // FieldError[] | undefined
  else if (e instanceof ApiError && e.status === 409) { /* ... */ }
}
```

A `.js` or `.mjs` output (`zapline client -o static/api.js`) gets the same client as an ES module with JSDoc types. Routes that answer with HTML (htmx pages and fragments) are left out. Files pass `tsc --strict --exactOptionalPropertyTypes --noUncheckedIndexedAccess` and are only rewritten when they change.

`zapline dev` regenerates clients after every successful build, before the app restarts: `frontend/src/api.ts` when the project has a `frontend/` directory, the `ts`/`js` files of a `[client]` table in `zapline.toml`, or `--client <file>`. Change a Rust struct, save, and your frontend's type checker sees the change: end-to-end types without a shared schema language. See [Typed frontends](frontend.md) for the whole workflow, including `/_zapline/api.js` for pages without a build step.

## AI assistants: `zapline mcp`

`zapline mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server on stdin/stdout. It lets Claude Code, Cursor, Zed, VS Code and other MCP clients call zapline directly, and every tool answers in as few tokens as it can:

| Tool | Returns |
|---|---|
| `docs` | No topic: the cheatsheet and the list of guide pages. With a topic (`"database migrations"`, `"htmx"`, `"errors"`): only the best-matching guide sections, from a copy of the guide built into the binary (no network) |
| `new_project` | Creates a project from a template |
| `add_feature` | Turns on zapline features in `Cargo.toml` (like `zapline add`); with no `features`, the list of features and what each one unlocks |
| `generate_resource`, `generate_migration`, `generate_handler`, `generate_middleware`, `generate_auth`, `generate_job`, `generate_mailer` | The files created and edited |
| `db_new_migration` | Creates a migration, with its up (`sql`) and down (`down_sql`) SQL if given |
| `db_migrate` | Applies pending migrations and names them |
| `db_status` | The `zapline db status` table, and the file to restore when an applied migration was edited or deleted |
| `db_rollback`, `db_redo`, `db_reset` | Revert migrations. They refuse to run without `confirm=true`, so the assistant asks you first |
| `check`, `test` | The condensed output of `zapline check` / `zapline test` |
| `routes` | One `METHOD path` line per route |
| `openapi` | The OpenAPI document, compact |
| `typescript_client` | Writes the TypeScript client |
| `explain_error` | A [diagnostic code](errors.md#diagnostic-codes)'s cause and fix |
| `last_results` | The latest `test`, `check` and `zapline fuzz` results with their age, without running anything again |

When the app does not compile, `routes`, `openapi` and the `db_*` tools answer with the compile errors (as `check` would) instead of "build failed", and when the app exits with an error, its last stderr lines are included. Any answer that mentions a diagnostic code (`ZL0004` in a startup error, a panic in a test) ends with that code's fix.

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
