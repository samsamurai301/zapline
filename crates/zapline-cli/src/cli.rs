//! Command-line definition (clap).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::generate::HandlerMethod;
use crate::templates::Template;

const ABOUT: &str = "Project scaffolding, code generation and dev tooling for zapline";

const LONG_ABOUT: &str = "\
Project scaffolding, code generation and dev tooling for zapline.

Create a project, run it with live reload, add resources, inspect the route table
and export the OpenAPI document, all from one tool.";

const EXAMPLES: &str = "\
Examples:
  zapline new my-api                       Create an API project (docs, validation, Docker, CI)
  zapline new hello --template minimal     Create a tiny single-route project
  zapline new shop --template full         Add config layers, DI, metrics, tracing, hardened middleware
  zapline new todo --template fullstack    HTML + htmx + SQLite + JSON API, zero setup
  zapline new hello.rs                     A whole app in one file, no Cargo.toml
  zapline run hello.rs                     Build (cached) and run a one-file app
  zapline dev hello.rs                     Live reload for a one-file app
  zapline dev                              Rebuild, swap and live-reload on every change
  zapline dev -- --port 9000               Pass arguments to your app
  zapline dev --record                     Record requests to turn into tests
  zapline snap signup                      Save the recording as tests/snapshots/signup.json
  zapline generate resource post           Add src/routes/posts.rs and tests/posts.rs, mounted for you
  zapline g handler ping --method get      Add a single route
  zapline g middleware audit               Add a middleware function
  zapline add ws redis                     Enable zapline features in Cargo.toml
  zapline routes                         Print the route table
  zapline openapi -o docs/openapi.json     Export the OpenAPI document
  zapline db new create_posts              Add a SQL migration
  zapline db migrate                       Apply pending migrations
  zapline explain ZL0002                   What an error code means and the code that fixes it
  zapline ship deploy@example.com          Build, upload and swap in a release with zero downtime
  zapline doctor                           Check your toolchain
  zapline bench http://localhost:8080/     Load test with wrk

Also available as a cargo subcommand: cargo zapline <command>";

/// The `zapline` command.
#[derive(Debug, Parser)]
#[command(
    name = "zapline",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_long_help = EXAMPLES,
    arg_required_else_help = true,
    propagate_version = true
)]
pub struct Cli {
    /// Run as if started in DIR (like `git -C`)
    #[arg(short = 'C', long, global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    #[allow(missing_docs)]
    #[command(subcommand)]
    pub command: Command,
}

/// All subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new project
    #[command(after_long_help = "\
Templates:
  minimal  Hello-world route with an in-process test
  api      Structured REST API: routes, models, validation, OpenAPI docs, Dockerfile, CI (default)
  full     api plus layered config, DI, health probes, Prometheus metrics, tracing,
           security headers, request IDs, CORS, rate limiting, background tasks
  fullstack  Server-rendered HTML with htmx, SQL database with migrations (SQLite, zero
           setup), JSON API with OpenAPI docs, Dockerfile, CI

Examples:
  zapline new my-app --template fullstack
  zapline new my-api
  zapline new hello.rs                     One-file app: a single .rs file, no Cargo.toml
  zapline new my-api --template full --path ~/code
  zapline new hello --template minimal --no-git")]
    New(NewArgs),

    /// Run a one-file app: a single .rs file, no Cargo.toml
    #[command(after_long_help = "\
Dependencies beyond zapline, serde, serde_json and tokio go in a `cargo` block in the
file's leading `//!` comment, in Cargo.toml syntax:

  //! ```cargo
  //! [dependencies]
  //! uuid = { version = \"1\", features = [\"v4\"] }
  //! ```

The file becomes a hidden project in the cache directory ($XDG_CACHE_HOME/zapline or
~/.cache/zapline), and all scripts share one target directory, so dependencies compile
once. An unchanged script starts without running cargo. The app runs in the current
directory and exits with its own exit code; everything after the file goes to the app.
With `#!/usr/bin/env -S zapline run` as its first line, the file runs as `./app.rs`.

Examples:
  zapline run app.rs
  zapline run app.rs -- --port 9000
  zapline run --release app.rs
  zapline run --print-manifest app.rs      Path of the hidden Cargo.toml, for editors")]
    Run(RunArgs),

    /// Generate code in the current project
    #[command(visible_alias = "g")]
    Generate(GenerateArgs),

    /// Enable zapline features (ws, views, sqlite, redis...) in Cargo.toml
    #[command(after_long_help = "\
Adds the features to the `zapline` dependency in Cargo.toml, whatever its shape
(`zapline = \"0.1\"`, an inline table with `version` or `path`, or a
[dependencies.zapline] table), keeping the list sorted and the rest of the file as it
was. Features already on (listed, on by default, or implied by another) are reported
and left alone. A misspelled name gets a suggestion.

`zapline check` and `zapline test` print `hint: enable the `ws` feature: zapline add ws`
when an error comes from a feature that is off.

Examples:
  zapline add ws
  zapline add sqlite views fs
  zapline add --list")]
    Add(AddArgs),

    /// Rebuild, swap and live-reload the app whenever files change
    #[command(after_long_help = "\
Rust changes (src/, config/, migrations/, Cargo.toml, build.rs, .env) rebuild in the
background while the previous version keeps serving; the new binary takes over once it
answers, then the old one gets SIGTERM (and a kill after 5 seconds). A failed build keeps
the previous version running and shows the compiler errors as an overlay in the browser.

Asset changes (templates/, static/, public/, assets/, locales/, i18n/) reload open pages
without restarting anything; a CSS change swaps the stylesheet in place.

A small proxy on the app's own address makes this work: the app listens on a free
loopback port, and the proxy adds one <script> to HTML pages. --no-proxy runs the app
directly instead.

Given a one-file app (see `zapline run --help`), watches just that file and runs the
app in the current directory.

Examples:
  zapline dev
  zapline dev app.rs
  zapline dev --bin server
  zapline dev --client web/src/api.ts   Also keep a typed TypeScript client up to date
  zapline dev --record                  Record requests for `zapline snap` (ZAPLINE_RECORD=1)
  zapline dev -- --port 9000

Typed clients: with a frontend/ directory, frontend/src/api.ts is regenerated after every
build (only rewritten when it changes, so vite or tsc --watch flag type errors at once).
Configure other files in zapline.toml (or [package.metadata.zapline.client] in Cargo.toml):
  [client]
  ts = \"web/src/api.ts\"     # TypeScript client
  js = \"static/api.js\"      # ES module with JSDoc types, for pages without a build step")]
    Dev(DevArgs),

    /// Save the requests the running app recorded as snapshot tests
    #[command(after_long_help = "\
Asks the running app's dev console (debug builds with `.dev_console()`) to write what it
recorded to tests/snapshots/<NAME>.json, and tests/snapshots.rs, which replays every
snapshot with `app().test().replay_snapshots(\"tests/snapshots\")`, if there is none yet.
The recording is then emptied. Each replayed response must keep its status and body;
timestamps, UUIDs, tokens and ids may differ. Authorization and cookie values are never
recorded.

Record with `zapline dev --record` (or the Start button at /_dev), use the app, then:

Examples:
  zapline snap signup
  zapline snap checkout --url http://127.0.0.1:3000
  ZAPLINE_UPDATE_SNAPSHOTS=1 cargo test --test snapshots   Accept changed responses")]
    Snap(SnapArgs),

    /// Print the app's route table
    #[command(after_long_help = "\
Runs the app with ZAPLINE_PRINT_ROUTES=1; `App::run` prints its routes and exits.

Examples:
  zapline routes
  zapline routes app.rs                  A one-file app
  zapline routes --json | jq '.[].path'")]
    Routes(RoutesArgs),

    /// Export the app's OpenAPI document
    #[command(after_long_help = "\
Runs the app with ZAPLINE_EXPORT_OPENAPI=<path>; the OpenAPI plugin writes the document
and the app exits. The file is only replaced when the export succeeds.

Examples:
  zapline openapi
  zapline openapi -o docs/openapi.json")]
    Openapi(OpenapiArgs),

    /// Generate a typed TypeScript (or JavaScript) client for the app's JSON API
    #[command(after_long_help = "\
Exports the OpenAPI document (like `zapline openapi`) and turns it into one dependency-free
file: an interface per schema, and `createClient()` with one typed method per operation,
named after its operationId. Non-2xx responses throw `ApiError` (status, code, details); a 422
with field errors throws `ValidationError`, typed by the body's field names. A `.js`/`.mjs`
output gets the same client as an ES module with JSDoc types. Routes that answer with HTML are
skipped. Files are only rewritten when they change.

Without -o, writes the clients configured in zapline.toml ([client] ts/js), else
frontend/src/api.ts when there is a frontend/ directory, else client.ts.

Examples:
  zapline client -o web/src/api.ts
  zapline client -o static/api.js          JavaScript module with JSDoc types
  zapline dev --client web/src/api.ts      Regenerate after every rebuild")]
    Client(ClientArgs),

    /// Create, apply, inspect and roll back SQL migrations
    #[command(
        subcommand,
        after_long_help = "\
Every command but `new` runs the app with ZAPLINE_MIGRATE=<command>, so it uses the app's
own DATABASE_URL and exits instead of serving.

Examples:
  zapline db new create_posts      Add migrations/<timestamp>_create_posts.up.sql + .down.sql
  zapline db migrate               Apply pending migrations
  zapline db status                What is applied, what is pending, what was edited
  zapline db rollback --steps 2    Revert the last two migrations
  zapline db redo                  Revert the last migration and apply it again
  zapline db reset                 Rebuild the schema and seed it (development only)"
    )]
    Db(DbCommand),

    /// Type-check the project and print one line per error or warning
    #[command(after_long_help = "\
Runs `cargo check --all-targets` (or clippy) and prints each diagnostic as
`level[code] file:line:col: message`, with the span label and the first suggested fix.
No progress output, no ASCII art: a fraction of cargo's output, built for AI assistants
and quick terminal reads. Exits non-zero when the build fails.

Examples:
  zapline check
  zapline check app.rs                   A one-file app
  zapline check --clippy")]
    Check {
        /// A one-file app to check instead of the project
        #[arg(value_name = "FILE.rs")]
        file: Option<PathBuf>,
        /// Run clippy instead of `cargo check`
        #[arg(long)]
        clippy: bool,
    },

    /// Run the tests and print only the totals and the failures
    #[command(after_long_help = "\
Compiles the tests first and, if they do not build, prints the condensed compile errors.
Otherwise runs `cargo test` and prints each failing test with the start of its panic
message, then one summary line. Exits non-zero when anything fails.

Examples:
  zapline test
  zapline test users::
  zapline test app.rs                    The #[cfg(test)] tests of a one-file app
  zapline test app.rs notes")]
    Test {
        /// A one-file app (a path ending in .rs), then the filter; or just the filter
        #[arg(value_name = "FILE.rs|FILTER")]
        first: Option<String>,
        /// Only run tests whose name contains this
        #[arg(value_name = "FILTER")]
        filter: Option<String>,
    },

    /// Fuzz every route from its OpenAPI schemas (runs tests/fuzz.rs)
    #[command(after_long_help = "\
Runs `cargo test --test fuzz`: the test calls `app().test().fuzz()`, which sends typical,
boundary and invalid requests generated from each operation's parameter and body schemas,
then random ones. It fails on panics, 5xx responses, timeouts, statuses the OpenAPI document
does not declare and 2xx JSON bodies that break their schema, and prints a minimal request
that reproduces each failure. Projects from `zapline new` include tests/fuzz.rs.

The same seed sends the same requests; `--seed` and `--cases` override the test's defaults
(through ZAPLINE_FUZZ_SEED and ZAPLINE_FUZZ_CASES) unless it sets them itself.

Examples:
  zapline fuzz
  zapline fuzz --cases 1000
  zapline fuzz --seed 7")]
    Fuzz {
        /// Seed for the generated requests [default: the test's, or 24301]
        #[arg(long, value_name = "N")]
        seed: Option<u64>,
        /// Requests per operation [default: the test's, or 100]
        #[arg(long, value_name = "N")]
        cases: Option<u64>,
    },

    /// Compile styles/app.css to static/app.css with Tailwind (see `zapline g tailwind`)
    Css {
        /// Minify the output
        #[arg(long)]
        minify: bool,
        /// Rebuild whenever templates or styles change
        #[arg(long, conflicts_with = "minify")]
        watch: bool,
    },

    /// Explain a diagnostic code (ZL0002): its cause and the code that fixes it
    #[command(after_long_help = "\
zapline's startup errors, panics on invalid setup, dev error pages and dev console name
the mistake with a stable code such as ZL0002. Without a code, lists them all.

Examples:
  zapline explain ZL0002
  zapline explain 10
  zapline explain")]
    Explain {
        /// The code, e.g. ZL0002 (or just 2)
        code: Option<String>,
    },

    /// Serve zapline's tools to AI assistants (Model Context Protocol, stdio)
    #[command(after_long_help = "\
Speaks MCP over stdin/stdout. Tools: docs (cheatsheet or the guide section you ask for),
new_project, add_feature, generate_resource, generate_handler, generate_middleware,
generate_migration, generate_auth, generate_job, generate_mailer, db_new_migration,
db_migrate, db_status, db_rollback, db_redo, db_reset (the last three need confirm=true),
db_seed, check, test, routes, openapi, typescript_client, explain_error; and, against the
running dev app, recent_requests, recent_errors, call_route and last_results. Start it
from the project directory.

Setup:
  Claude Code   claude mcp add zapline -- zapline mcp
  Cursor/others {\"mcpServers\": {\"zapline\": {\"command\": \"zapline\", \"args\": [\"mcp\"]}}}")]
    Mcp,

    /// Check that the tools zapline relies on are installed
    Doctor,

    /// Load-test a URL with wrk
    #[command(after_long_help = "\
Benchmark a release build for meaningful numbers: `cargo run --release`.

Examples:
  zapline bench http://localhost:8080/
  zapline bench http://localhost:8080/users -c 128 -d 30s -t 2")]
    Bench(BenchArgs),

    /// Build a release binary and swap it in on a Linux server over ssh
    #[command(after_long_help = "\
Builds `cargo build --release`, copies the binary over scp and starts it next to the
running release on the same port (SO_REUSEPORT). The old release stops only after the
new one answers its health check; if anything fails first, the old one keeps serving.
The first run creates a system user, /etc/<app>/env (with generated secrets) and the
systemd unit <app>@.service. Needs ssh access as root or a user with sudo, and systemd.

Examples:
  zapline ship deploy@example.com --dry-run      Print every command and file, run nothing
  zapline ship root@203.0.113.7                  Build, upload, swap
  zapline ship web1 --static                     A static musl binary (any Linux)
  zapline ship web1 --env SMTP_HOST=mail.local   Also set a value in /etc/<app>/env
  zapline ship web1 --rollback                   Start the previous release again")]
    Ship(ShipArgs),
}

/// Arguments of `zapline new`.
#[derive(Debug, Args)]
pub struct NewArgs {
    /// Project name, also used as the crate name and directory name; a name ending in
    /// `.rs` creates a one-file app instead
    pub name: String,

    /// Project layout [default: api]
    #[arg(short, long, value_enum)]
    pub template: Option<Template>,

    /// Parent directory in which to create the project
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub path: PathBuf,

    /// Depend on a local checkout of zapline (the `crates/zapline` directory) instead of crates.io
    #[arg(long, value_name = "PATH")]
    pub zapline_path: Option<PathBuf>,

    /// Do not run `git init`
    #[arg(long)]
    pub no_git: bool,
}

/// Arguments of `zapline generate`.
#[derive(Debug, Args)]
pub struct GenerateArgs {
    #[allow(missing_docs)]
    #[command(subcommand)]
    pub kind: GenerateKind,

    /// Overwrite files that already exist
    #[arg(short, long, global = true)]
    pub force: bool,
}

/// What to generate.
#[derive(Debug, Subcommand)]
pub enum GenerateKind {
    /// A resource: model, CRUD handlers, in-memory store and tests
    #[command(after_long_help = "\
Creates src/routes/<plural>.rs and tests/<plural>.rs. When src/routes/mod.rs contains the
`// zapline:routes:start` / `// zapline:routes:end` markers (the api and full templates do),
the module is declared and mounted automatically; otherwise you get instructions.

The name can be singular or plural and in any case style: post, Posts, blog-post, BlogPosts.

In a project with a migrations/ directory (the fullstack template), the resource is
database-backed: a SQL migration (with created_at/updated_at), sqlx queries, a JSON API
(list, get, create, PUT, PATCH, delete), htmx pages when templates/base.html exists
(index, show, edit) and tests. Field types: string, text, int, float, bool, date,
datetime, uuid and references; append `?` for optional.

`author:references` adds `author_id`, a foreign key to `authors(id)` with an index, a
`?author_id=` filter, `GET /api/authors/{id}/posts`, and a <select> of authors in forms.

Examples:
  zapline generate resource post
  zapline g resource post title body:text published:bool
  zapline g resource product name price:float stock:int notes:text?
  zapline g resource event title starts_at:datetime day:date token:uuid
  zapline g resource author name
  zapline g resource post title body:text published_on:date author:references
  zapline g resource blog-post
  zapline g resource person --plural people")]
    Resource {
        /// Resource name, e.g. `post` or `blog-post`
        name: String,
        /// Columns for a database-backed resource: `title`, `body:text`, `views:int`,
        /// `price:float`, `done:bool`, `due:date`, `at:datetime`, `key:uuid`,
        /// `author:references`; add `?` for optional (`bio:text?`)
        #[arg(value_name = "FIELD:TYPE")]
        fields: Vec<String>,
        /// Plural form, for irregular nouns the built-in rules get wrong
        #[arg(long, value_name = "WORD")]
        plural: Option<String>,
        /// Use an in-memory store even in a project with a database
        #[arg(long)]
        memory: bool,
    },

    /// A SQL migration with its down file: add or remove columns (and update the resource), or create a table
    #[command(after_long_help = "\
Writes migrations/<timestamp>_<name>.up.sql and .down.sql for the project's database
(SQLite or Postgres). The name decides what they do (up / down):
  add_<cols>_to_<table>        ALTER TABLE <table> ADD COLUMN / DROP COLUMN for each field
  remove_<cols>_from_<table>   ALTER TABLE <table> DROP COLUMN / ADD COLUMN for each field
  create_<table>               CREATE TABLE with id, the fields, created_at and updated_at
                               / DROP TABLE
Without fields, the column comes from the name (`add_email_to_users` adds `email` text).
Any other name with fields creates that table; without fields you get an empty pair.
Field types are the ones `generate resource` takes. A required column added to an existing
table gets a default: the one after `=` (`views:int=0`, `due:date=2026-01-01`), else '' or
0; without `=`, dates, uuids and references must be optional (`?`) there.

When `generate resource` made the table, src/routes/<table>.rs, its pages and its test are
updated too: they become what `generate resource` would write with the new fields, with your
own edits kept. A file edited where the new code goes is left alone, and the lines to change
are printed; if that file is the module, nothing is changed.

Examples:
  zapline g migration add_published_at_to_posts published_at:datetime?
  zapline g migration add_views_to_posts views:int=0
  zapline g migration add_email_to_users
  zapline g migration add_category_to_posts category:references?
  zapline g migration remove_legacy_flag_from_posts
  zapline g migration create_tags name slug")]
    Migration {
        /// Migration name, e.g. `add_email_to_users`
        name: String,
        /// Columns: `name:type`, as for `generate resource`; `name:type=value` sets the default
        #[arg(value_name = "FIELD:TYPE")]
        fields: Vec<String>,
    },

    /// A single route handler in src/routes/<name>.rs
    #[command(after_long_help = "\
Examples:
  zapline generate handler ping
  zapline g handler send_email --method post --path /emails
  zapline g handler get_item --path /items/{id}")]
    Handler {
        /// Handler name, e.g. `ping`
        name: String,
        /// HTTP method
        #[arg(short, long, value_enum, default_value = "get")]
        method: HandlerMethod,
        /// Route path [default: /<name in kebab-case>]
        #[arg(short, long, value_name = "PATH")]
        path: Option<String>,
    },

    /// A middleware function in src/middleware/<name>.rs
    Middleware {
        /// Middleware name, e.g. `audit`
        name: String,
    },

    /// Accounts: sign-up, log-in, log-out, sessions and a CurrentUser extractor
    #[command(after_long_help = "\
Needs a project with a database (migrations/), like the fullstack template. Generates:
  migrations/<ts>_create_users.sql   users table (email, Argon2id password hash)
  src/routes/auth.rs                 JSON API under /api/auth, CurrentUser extractor,
                                     sessions stored in the database
  templates/auth/*.html              sign-up and log-in pages (when templates/base.html exists)
  tests/auth.rs                      end-to-end tests
and enables zapline's `cookies` and `password` features in Cargo.toml.

Protect a handler by taking `CurrentUser`. Set SESSION_SECRET (32+ random bytes) in production.

Examples:
  zapline g auth")]
    Auth,

    /// A durable background job, stored in the database and retried on failure
    #[command(after_long_help = "\
Needs a project with a database (migrations/), like the fullstack template. Generates:
  src/jobs/<name>.rs      the job: payload struct + `impl Job`
  src/jobs/mod.rs         declares and registers it (`jobs::register`)
  tests/<name>_job.rs     runs the job once
and adds `.jobs(jobs::register)` to `app()` in src/lib.rs.

Enqueue from a handler that takes `jobs: Jobs`: `jobs.enqueue(MyJob { .. }).await?`.

Examples:
  zapline g job send_welcome
  zapline g job nightly-report")]
    Job {
        /// Job name, e.g. `send_welcome`
        name: String,
    },

    /// An email rendered from templates, with a test
    #[command(after_long_help = "\
Generates:
  src/mail/<name>.rs             a function building the Email from the templates
  src/mail/mod.rs                declares it (and `pub mod mail;` is added to src/lib.rs)
  templates/mail/<name>.html     the HTML body; its `{% block subject %}` is the subject
  templates/mail/<name>.txt      the plain-text body
  tests/<name>_email.rs          renders it and checks subject, recipient and body
and enables zapline's `mail` and `views` features in Cargo.toml.

Send it from a handler that takes `views: Views, mail: Mailer`:
  mail.send(crate::mail::welcome::welcome(&views, &user.email)?).await?;
While developing, sent mail shows up at /_dev/mail.

Examples:
  zapline g mailer welcome
  zapline g mailer password-reset")]
    Mailer {
        /// Email name, e.g. `welcome`
        name: String,
    },

    /// Tailwind CSS without Node: styles/app.css compiled to static/app.css
    #[command(after_long_help = "\
Creates styles/app.css (`@import \"tailwindcss\"` plus the CSS that was in static/app.css)
and builds static/app.css with the standalone Tailwind binary, downloaded once and
checked against its SHA-256. From then on `zapline dev` rebuilds it as you edit
templates, and `zapline css` builds it on demand.

Examples:
  zapline g tailwind")]
    Tailwind,

    /// Hosting config: fly.toml, railway.json, render.yaml or compose.yaml
    #[command(after_long_help = "\
Writes the target's config for the project's Dockerfile (adding one if missing), with a
volume for a SQLite file, Postgres and Redis when zapline's features use them, health
checks when the Health plugin is installed, and the settings from .env.example. Prints
the commands that remain (creating the app, setting secrets).

Examples:
  zapline g deploy fly
  zapline g deploy compose")]
    Deploy {
        /// Where to deploy
        #[arg(value_enum)]
        target: crate::generate::deploy::Target,
    },
}

/// Arguments of `zapline run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Build in release mode
    #[arg(long)]
    pub release: bool,

    /// Only write the hidden project and print the path of its Cargo.toml
    #[arg(long)]
    pub print_manifest: bool,

    /// The one-file app, then the arguments passed to it (`--` before them is optional)
    #[arg(
        value_name = "FILE.rs [APP_ARGS]",
        required = true,
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub command: Vec<String>,
}

impl RunArgs {
    /// The one-file app.
    pub fn file(&self) -> PathBuf {
        PathBuf::from(self.command.first().map_or("", String::as_str))
    }

    /// The arguments for the app: everything after the file, minus a leading `--`.
    pub fn app_args(&self) -> Vec<String> {
        let rest = self.command.get(1..).unwrap_or_default();
        match rest.split_first() {
            Some((dashes, after)) if dashes == "--" => after.to_vec(),
            _ => rest.to_vec(),
        }
    }
}

/// Arguments of `zapline add`.
#[derive(Debug, Args)]
pub struct AddArgs {
    /// Features to enable, e.g. `ws redis`
    #[arg(value_name = "FEATURE", required_unless_present = "list")]
    pub features: Vec<String>,

    /// List every feature with a one-line description
    #[arg(long, conflicts_with = "features")]
    pub list: bool,
}

/// Arguments of `zapline dev`.
#[derive(Debug, Args)]
pub struct DevArgs {
    /// A one-file app to run instead of the project
    #[arg(value_name = "FILE.rs", conflicts_with = "bin")]
    pub file: Option<PathBuf>,

    /// Binary to run [default: the package's only binary]
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Build in release mode
    #[arg(long)]
    pub release: bool,

    /// Keep a typed client of the API up to date (see `zapline client`)
    #[arg(long, value_name = "FILE", conflicts_with = "no_client")]
    pub client: Option<PathBuf>,

    /// Run the app directly: no proxy, no browser live reload, no error
    /// overlay, and restarts stop the old process before the new one starts
    #[arg(long)]
    pub no_proxy: bool,
    /// Record requests and responses from startup, for `zapline snap`
    #[arg(long)]
    pub record: bool,
    /// Don't regenerate typed clients, even when zapline.toml or frontend/ asks for one
    #[arg(long)]
    pub no_client: bool,

    /// How long (ms) files must stay quiet before rebuilding
    #[arg(long, value_name = "MS", default_value_t = 300, hide = true)]
    pub debounce_ms: u64,

    /// Arguments passed to the app (after `--`)
    #[arg(last = true, value_name = "APP_ARGS")]
    pub app_args: Vec<String>,
}

/// Arguments of `zapline snap`.
#[derive(Debug, Args)]
pub struct SnapArgs {
    /// Snapshot name: tests/snapshots/<NAME>.json (letters, digits, `-`, `_`)
    pub name: String,

    /// Where the app listens [default: from ADDR or PORT like the app's main, else
    /// http://127.0.0.1:8080]
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,
}

/// Arguments of `zapline routes`.
#[derive(Debug, Args)]
pub struct RoutesArgs {
    /// A one-file app instead of the project
    #[arg(value_name = "FILE.rs", conflicts_with = "bin")]
    pub file: Option<PathBuf>,

    /// Binary to run [default: the package's only binary]
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Print the raw JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

/// Arguments of `zapline openapi`.
#[derive(Debug, Args)]
pub struct OpenapiArgs {
    /// File to write
    #[arg(short, long, value_name = "FILE", default_value = "openapi.json")]
    pub output: PathBuf,

    /// Binary to run [default: the package's only binary]
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,
}

/// Arguments of `zapline client`.
#[derive(Debug, Args)]
pub struct ClientArgs {
    /// Where to write the client (`.ts`, or `.js`/`.mjs` for JavaScript) [default: from
    /// zapline.toml, else frontend/src/api.ts with a frontend/ directory, else client.ts]
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Binary to run [default: the package's only binary]
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,
}

/// `zapline db` subcommands.
#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Create an empty migration in migrations/: an .up.sql + .down.sql pair
    #[command(after_long_help = "\
Writes migrations/<timestamp>_<name>.up.sql and .down.sql, so `zapline db rollback` can
undo it. In a project whose migrations are all one-way .sql files (made before pairs were
the default) it writes a one-way file, unless you pass --reversible.

Examples:
  zapline db new create_posts
  zapline db new add_email_to_users --one-way")]
    New {
        /// What the migration does, e.g. `create_posts`
        name: String,
        /// Write an .up.sql + .down.sql pair (the default for new projects)
        #[arg(long, conflicts_with = "one_way")]
        reversible: bool,
        /// Write a single one-way .sql file
        #[arg(long)]
        one_way: bool,
    },
    /// Apply pending migrations (runs the app with ZAPLINE_MIGRATE=up)
    Migrate {
        /// Binary to run [default: the package's only binary]
        #[arg(long, value_name = "NAME")]
        bin: Option<String>,
    },
    /// Show every migration: version, name, when it was applied; fails if an applied one was edited or deleted
    Status {
        /// Binary to run [default: the package's only binary]
        #[arg(long, value_name = "NAME")]
        bin: Option<String>,
    },
    /// Revert the last applied migration(s) with their .down.sql files
    #[command(after_long_help = "\
Refused against a release build (--release) or when APP_ENV/ZAPLINE_ENV is production,
unless --force is given. Nothing runs unless every migration to revert has a down file.

Examples:
  zapline db rollback
  zapline db rollback --steps 3")]
    Rollback {
        /// How many migrations to revert, newest first
        #[arg(long, value_name = "N", default_value_t = 1)]
        steps: u32,
        #[allow(missing_docs)]
        #[command(flatten)]
        target: DestructiveArgs,
    },
    /// Revert the last migration and apply it again (while writing it)
    Redo {
        #[allow(missing_docs)]
        #[command(flatten)]
        target: DestructiveArgs,
    },
    /// Revert every migration, apply them all again and insert the seed data
    #[command(after_long_help = "\
For development: rebuilds the schema from migrations/ (every applied migration needs a
down file), then runs `App::seed` if the app has one. Refused against a release build or
the production profile unless --force is given.")]
    Reset {
        #[allow(missing_docs)]
        #[command(flatten)]
        target: DestructiveArgs,
    },
    /// Insert the app's seed data: migrate, then run `App::seed` once (ZAPLINE_SEED=1)
    Seed {
        /// Binary to run [default: the package's only binary]
        #[arg(long, value_name = "NAME")]
        bin: Option<String>,
    },
}

/// Options of the `zapline db` commands that revert migrations.
#[derive(Debug, Args)]
pub struct DestructiveArgs {
    /// Binary to run [default: the package's only binary]
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Build and run the release profile (needs --force)
    #[arg(long)]
    pub release: bool,

    /// Run even against a release build or the production profile
    #[arg(long)]
    pub force: bool,
}

/// Arguments of `zapline bench`.
#[derive(Debug, Args)]
pub struct BenchArgs {
    /// URL to benchmark, e.g. http://localhost:8080/
    pub url: String,

    /// Open connections
    #[arg(short, long, default_value_t = 256)]
    pub connections: usize,

    /// Test duration: 10s, 2m, 1h
    #[arg(short, long, default_value = "10s")]
    pub duration: String,

    /// wrk threads [default: CPU count, at most 4]
    #[arg(short, long)]
    pub threads: Option<usize>,

    /// Print wrk's own report instead of the summary
    #[arg(long)]
    pub raw: bool,
}

/// Arguments of `zapline ship`.
#[derive(Debug, Args)]
pub struct ShipArgs {
    /// The server: user@host, or a Host from ~/.ssh/config
    #[arg(value_name = "USER@HOST")]
    pub host: String,

    /// Binary target to ship [default: the package's binary]
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Service name on the server [default: the package name]
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Port the app listens on (written to the settings file on the first deploy)
    #[arg(long, default_value_t = 8080)]
    pub port: u16,

    /// Health check path [default: /health/ready with the Health plugin, else /]
    #[arg(long, value_name = "PATH")]
    pub health: Option<String>,

    /// Cargo target triple to build for
    #[arg(long, value_name = "TRIPLE", conflicts_with = "static_build")]
    pub target: Option<String>,

    /// Build a static binary (the musl target for the server's CPU)
    #[arg(long = "static")]
    pub static_build: bool,

    /// ssh port
    #[arg(long, value_name = "PORT")]
    pub ssh_port: Option<u16>,

    /// ssh identity (private key) file
    #[arg(short, long, value_name = "FILE")]
    pub identity: Option<PathBuf>,

    /// Set KEY=VALUE in the server's settings file (repeatable)
    #[arg(long, value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Releases kept on the server, including the live one
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..))]
    pub keep: u16,

    /// Print every command and generated file without running anything
    #[arg(long)]
    pub dry_run: bool,

    /// Start the previous release again instead of shipping a new one
    #[arg(long, conflicts_with_all = ["target", "static_build", "bin"])]
    pub rollback: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn ship_parses() {
        let Command::Ship(s) = parse(&[
            "ship",
            "deploy@h",
            "--static",
            "--env",
            "A=1",
            "--env",
            "B=2",
            "--dry-run",
            "--ssh-port",
            "2222",
            "-i",
            "k",
            "--keep",
            "5",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(s.host, "deploy@h");
        assert!(s.static_build && s.dry_run && !s.rollback);
        assert_eq!(s.env, ["A=1", "B=2"]);
        assert_eq!((s.ssh_port, s.keep, s.port), (Some(2222), 5, 8080));
        assert_eq!(s.identity, Some(PathBuf::from("k")));
        assert!(Cli::try_parse_from(["zapline", "ship", "h", "--rollback", "--static"]).is_err());
        assert!(Cli::try_parse_from(["zapline", "ship", "h", "--keep", "0"]).is_err());
        assert!(Cli::try_parse_from(["zapline", "ship"]).is_err());
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("zapline").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn new_defaults() {
        let Command::New(a) = parse(&["new", "my-app"]).command else {
            panic!()
        };
        assert_eq!(a.name, "my-app");
        assert_eq!(a.template, None);
        assert_eq!(a.path, PathBuf::from("."));
        assert!(!a.no_git);
        assert!(a.zapline_path.is_none());
    }

    #[test]
    fn new_with_options() {
        let Command::New(a) = parse(&[
            "new",
            "x",
            "--template",
            "full",
            "--path",
            "/tmp",
            "--zapline-path",
            "/z",
            "--no-git",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(a.template, Some(Template::Full));
        assert_eq!(a.path, PathBuf::from("/tmp"));
        assert_eq!(a.zapline_path, Some(PathBuf::from("/z")));
        assert!(a.no_git);
        assert!(Cli::try_parse_from(["zapline", "new", "x", "-t", "bogus"]).is_err());
    }

    #[test]
    fn generate_has_an_alias_and_nested_commands() {
        for head in ["generate", "g"] {
            let cli = parse(&[head, "resource", "post", "--plural", "posts", "--force"]);
            let Command::Generate(g) = cli.command else {
                panic!()
            };
            assert!(g.force);
            let GenerateKind::Resource { name, plural, .. } = g.kind else {
                panic!()
            };
            assert_eq!(name, "post");
            assert_eq!(plural.as_deref(), Some("posts"));
        }
        let Command::Generate(g) =
            parse(&["g", "handler", "ping", "-m", "post", "-p", "/x"]).command
        else {
            panic!()
        };
        let GenerateKind::Handler { name, method, path } = g.kind else {
            panic!()
        };
        assert_eq!(
            (name.as_str(), method, path.as_deref()),
            ("ping", HandlerMethod::Post, Some("/x"))
        );
        let Command::Generate(g) = parse(&["g", "middleware", "audit"]).command else {
            panic!()
        };
        assert!(matches!(g.kind, GenerateKind::Middleware { .. }));
    }

    #[test]
    fn dev_passes_arguments_after_double_dash() {
        let Command::Dev(d) = parse(&["dev", "--bin", "server", "--", "--port", "9000"]).command
        else {
            panic!()
        };
        assert_eq!(d.bin.as_deref(), Some("server"));
        assert_eq!(d.app_args, ["--port", "9000"]);
        assert_eq!(d.debounce_ms, 300);
        assert!(!d.record);
        let Command::Dev(d) = parse(&["dev", "--record"]).command else {
            panic!()
        };
        assert!(d.record);
    }

    #[test]
    fn snap_takes_a_name_and_an_optional_url() {
        let Command::Snap(s) = parse(&["snap", "signup"]).command else {
            panic!()
        };
        assert_eq!((s.name.as_str(), s.url), ("signup", None));
        let Command::Snap(s) = parse(&["snap", "x", "--url", "http://127.0.0.1:3000"]).command
        else {
            panic!()
        };
        assert_eq!(s.url.as_deref(), Some("http://127.0.0.1:3000"));
        assert!(Cli::try_parse_from(["zapline", "snap"]).is_err());
    }

    #[test]
    fn run_passes_everything_after_the_file_to_the_app() {
        for args in [
            &["run", "app.rs", "--port", "9000"][..],
            &["run", "app.rs", "--", "--port", "9000"][..],
        ] {
            let Command::Run(r) = parse(args).command else {
                panic!()
            };
            assert_eq!(r.file(), PathBuf::from("app.rs"));
            assert_eq!(r.app_args(), ["--port", "9000"], "{args:?}");
            assert!(!r.release);
        }
        let Command::Run(r) = parse(&["run", "--release", "--print-manifest", "a.rs"]).command
        else {
            panic!()
        };
        assert!(r.release && r.print_manifest && r.app_args().is_empty());
        assert_eq!(r.file(), PathBuf::from("a.rs"));
        // Options after the file belong to the app.
        let Command::Run(r) = parse(&["run", "a.rs", "--release"]).command else {
            panic!()
        };
        assert!(!r.release);
        assert_eq!(r.app_args(), ["--release"]);
        // A later `--` is the app's.
        let Command::Run(r) = parse(&["run", "a.rs", "x", "--", "y"]).command else {
            panic!()
        };
        assert_eq!(r.app_args(), ["x", "--", "y"]);
        assert!(Cli::try_parse_from(["zapline", "run"]).is_err());
    }

    #[test]
    fn dev_routes_check_and_test_take_a_one_file_app() {
        let Command::Dev(d) = parse(&["dev", "app.rs", "--", "--port", "1"]).command else {
            panic!()
        };
        assert_eq!(d.file, Some(PathBuf::from("app.rs")));
        assert_eq!(d.app_args, ["--port", "1"]);
        assert!(Cli::try_parse_from(["zapline", "dev", "app.rs", "--bin", "x"]).is_err());
        let Command::Routes(r) = parse(&["routes", "app.rs", "--json"]).command else {
            panic!()
        };
        assert_eq!(r.file, Some(PathBuf::from("app.rs")));
        let Command::Check { file, clippy } = parse(&["check", "app.rs"]).command else {
            panic!()
        };
        assert_eq!((file, clippy), (Some(PathBuf::from("app.rs")), false));
        let Command::Test { first, filter } = parse(&["test", "app.rs", "notes"]).command else {
            panic!()
        };
        assert_eq!(
            (first.as_deref(), filter.as_deref()),
            (Some("app.rs"), Some("notes"))
        );
    }

    #[test]
    fn other_commands_parse() {
        let Command::Routes(r) = parse(&["routes", "--json", "--bin", "a"]).command else {
            panic!()
        };
        assert!(r.json);
        assert_eq!(r.bin.as_deref(), Some("a"));
        let Command::Openapi(o) = parse(&["openapi"]).command else {
            panic!()
        };
        assert_eq!(o.output, PathBuf::from("openapi.json"));
        let Command::Openapi(o) = parse(&["openapi", "-o", "x/spec.json"]).command else {
            panic!()
        };
        assert_eq!(o.output, PathBuf::from("x/spec.json"));
        let Command::Bench(b) =
            parse(&["bench", "http://x", "-c", "10", "-d", "5s", "-t", "2"]).command
        else {
            panic!()
        };
        assert_eq!(
            (b.connections, b.duration.as_str(), b.threads),
            (10, "5s", Some(2))
        );
        assert!(matches!(parse(&["doctor"]).command, Command::Doctor));
    }

    #[test]
    fn add_takes_features_or_list() {
        let Command::Add(a) = parse(&["add", "ws", "redis"]).command else {
            panic!()
        };
        assert_eq!(a.features, ["ws", "redis"]);
        assert!(!a.list);
        let Command::Add(a) = parse(&["add", "--list"]).command else {
            panic!()
        };
        assert!(a.list && a.features.is_empty());
        assert!(Cli::try_parse_from(["zapline", "add"]).is_err());
        assert!(Cli::try_parse_from(["zapline", "add", "ws", "--list"]).is_err());
    }

    #[test]
    fn global_dir_option_works_anywhere() {
        assert_eq!(
            parse(&["-C", "/p", "routes"]).dir,
            Some(PathBuf::from("/p"))
        );
        assert_eq!(
            parse(&["routes", "-C", "/p"]).dir,
            Some(PathBuf::from("/p"))
        );
    }

    #[test]
    fn no_arguments_shows_help() {
        let err = Cli::try_parse_from(["zapline"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn version_is_available() {
        let err = Cli::try_parse_from(["zapline", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
    }
}
