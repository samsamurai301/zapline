# Project layout & the CLI

The `zapline` CLI scaffolds projects, generates code, runs a live-reloading dev server, and talks to your app to print its routes or export its OpenAPI document. Install it with `cargo install zapline-cli`. You can call it as `zapline <cmd>` or `cargo zapline <cmd>`.

| Command | What it does |
|---|---|
| `zapline new <name> [-t minimal\|api\|full\|fullstack] [--path DIR]` | Create a project. `api` (the default) includes routes, models, validation, OpenAPI, Docker and CI. `full` adds layered config, DI, health and metrics probes, tracing and a hardened middleware stack. `fullstack` is server-rendered HTML with htmx, SQLite (zero setup) with migrations, and a JSON API. |
| `zapline generate resource <name> [field:type ...]` (`g resource`) | Add a resource and mount it automatically. In a project with `migrations/`: a SQL migration with `created_at`/`updated_at`, a sqlx JSON API (list, get, create, `PUT`, `PATCH`, delete), htmx index, show and edit pages (when `templates/base.html` exists) and tests. Otherwise: CRUD handlers over an in-memory store. Types: `string`, `text`, `int`, `float`, `bool`, `date`, `datetime`, `uuid`, `references`; `?` for optional ([details](#database-backed-resources)). |
| `zapline g migration <name> [field:type ...]` | A migration from its name: `add_<cols>_to_<table>` adds columns, `remove_<cols>_from_<table>` drops them, `create_<table>` creates a table ([details](#migrations-from-the-command-line)) |
| `zapline db new <name>` / `zapline db migrate` | Create a SQL migration / apply pending ones ([Databases](database.md)) |
| `zapline db seed` | Insert the app's [seed data](database.md#seed-data) (`App::seed`) after migrating |
| `zapline g handler <name> --method get --path /x` | Add a single route |
| `zapline g middleware <name>` | Add a middleware function |
| `zapline g job <name>` | A [durable job](jobs.md): `src/jobs/<name>.rs`, registered in `src/jobs/mod.rs` and `app()`, plus a test |
| `zapline g auth` | Add accounts: users migration, sign-up/log-in/log-out, password reset by email (JSON API and htmx pages), database sessions, a `CurrentUser` extractor and tests ([Accounts](auth.md#accounts-in-one-command-zapline-g-auth)) |
| `zapline g deploy fly\|railway\|render\|compose` | [Hosting config](deployment.md#hosting-configs-zapline-g-deploy) for the project's Dockerfile, with volumes, databases, health checks and secrets filled in |
| `zapline g tailwind` | [Tailwind CSS without Node](files.md#tailwind-css-without-node): `styles/app.css` compiled to `static/app.css` by the standalone Tailwind binary |
| `zapline dev [--bin X] [-- app args]` | Rebuild and restart on changes to `src/`, `config/`, `migrations/` and `Cargo.toml` (templates reload without a restart). The old process gets SIGTERM, then a kill after 5 seconds. Sets `ZAPLINE_DEV=1`, so 500 responses show the real error. With Tailwind, also rebuilds `static/app.css` as you save. |
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
| `author:references` | `author_id: i64` | `BIGINT REFERENCES authors(id) ON DELETE CASCADE` | same | `<select>` of authors |

Add `?` for an optional field: a nullable column and an `Option<T>`; an optional reference uses `ON DELETE SET NULL`. Dates, times and UUIDs come from `zapline::chrono` and `zapline::uuid`; the generator turns on zapline's `chrono` (and `uuid`) feature in `Cargo.toml`, so you don't add those crates yourself.

A reference also gets a `?author_id=` filter on the list, `GET /api/authors/{id}/posts` (the author's posts, paged like the main list; `404` for an unknown author), and an author `<select>` in the forms that shows each author's `name` (or `title`, `email`, the first required text column, or the id: read from the authors migration). Rows link to the author's page when it has one. Generate the parent first: the generator warns when no migration creates its table, since Postgres won't create a foreign key to a missing table. Inserting a post for an author that doesn't exist answers `409` (`foreign_key_violation`).

`PUT` replaces every field; `PATCH` takes any subset (`{"title": "New"}`) and leaves the rest. Both set `updated_at`.

## Migrations from the command line

`zapline g migration` writes the SQL for the project's dialect from the migration's name, with the same field syntax:

```sh
zapline g migration add_published_at_to_posts published_at:datetime?
zapline g migration add_email_to_users                 # the column from the name: email TEXT
zapline g migration add_category_to_posts category:references?
zapline g migration remove_legacy_flag_from_posts
zapline g migration create_tags name slug              # id, fields, created_at, updated_at
```

```sql
-- migrations/20261023101500_add_published_at_to_posts.sql
ALTER TABLE posts ADD COLUMN published_at TIMESTAMPTZ;
```

A required column added to a table that has rows needs a value for them: text gets `DEFAULT ''`, numbers `DEFAULT 0`, booleans `FALSE`. Dates, UUIDs and references have no sensible default, so add them as optional (`due:date?`), fill them in, then tighten the column in a later migration. Any other name with fields creates that table; without fields you get an empty migration (like `zapline db new`). The generator doesn't edit your Rust code: when `src/routes/<table>.rs` exists it prints the fields to add to the model and its queries.

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
| `generate_resource`, `generate_migration`, `generate_handler`, `generate_middleware`, `generate_auth` | The files created and edited |
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
