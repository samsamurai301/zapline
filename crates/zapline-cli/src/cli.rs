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
  zapline dev                              Rebuild and restart on every change
  zapline dev -- --port 9000               Pass arguments to your app
  zapline generate resource post           Add src/routes/posts.rs and tests/posts.rs, mounted for you
  zapline g handler ping --method get      Add a single route
  zapline g middleware audit               Add a middleware function
  zapline routes                           Print the route table
  zapline openapi -o docs/openapi.json     Export the OpenAPI document
  zapline db new create_posts              Add a SQL migration
  zapline db migrate                       Apply pending migrations
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
  zapline new my-api --template full --path ~/code
  zapline new hello --template minimal --no-git")]
    New(NewArgs),

    /// Generate code in the current project
    #[command(visible_alias = "g")]
    Generate(GenerateArgs),

    /// Rebuild and restart the app whenever files change
    #[command(after_long_help = "\
Watches src/, config/, migrations/, Cargo.toml and build.rs (ignoring target/ and editor
temp files), rebuilds after the changes settle, and restarts the binary. The old process
gets SIGTERM and is killed after 5 seconds if it does not exit. If a build fails, the
previous version keeps running.

Examples:
  zapline dev
  zapline dev --bin server
  zapline dev --client web/src/api.ts   Also keep a typed TypeScript client up to date
  zapline dev -- --port 9000")]
    Dev(DevArgs),

    /// Print the app's route table
    #[command(after_long_help = "\
Runs the app with ZAPLINE_PRINT_ROUTES=1; `App::run` prints its routes and exits.

Examples:
  zapline routes
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

    /// Generate a typed TypeScript client for the app's JSON API
    #[command(after_long_help = "\
Exports the OpenAPI document (like `zapline openapi`) and turns it into one dependency-free
TypeScript file: an interface per schema, and `createClient()` with one typed method per
operation, named after its operationId. Non-2xx responses throw `ApiError` (status, code,
details). Routes that answer with HTML are skipped. The file is only rewritten when it changes.

Examples:
  zapline client -o web/src/api.ts
  zapline dev --client web/src/api.ts      Regenerate after every rebuild")]
    Client(ClientArgs),

    /// Create and apply SQL migrations
    #[command(
        subcommand,
        after_long_help = "\
Examples:
  zapline db new create_posts      Add migrations/<timestamp>_create_posts.sql
  zapline db migrate               Apply pending migrations using the app's DATABASE_URL"
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
  zapline check --clippy")]
    Check {
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
  zapline test users::")]
    Test {
        /// Only run tests whose name contains this
        filter: Option<String>,
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

    /// Serve zapline's tools to AI assistants (Model Context Protocol, stdio)
    #[command(after_long_help = "\
Speaks MCP over stdin/stdout. Tools: docs (cheatsheet or the guide section you ask for),
new_project, generate_resource, generate_handler, generate_middleware, db_new_migration,
db_migrate, check, test, routes, openapi. Start it from the project directory.

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
}

/// Arguments of `zapline new`.
#[derive(Debug, Args)]
pub struct NewArgs {
    /// Project name, also used as the crate name and directory name
    pub name: String,

    /// Project layout
    #[arg(short, long, value_enum, default_value = "api")]
    pub template: Template,

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
database-backed: a SQL migration, sqlx queries, a JSON API, an htmx page when
templates/base.html exists, and tests. Field types: string, text, int, float, bool;
append `?` for optional.

Examples:
  zapline generate resource post
  zapline g resource post title body:text published:bool
  zapline g resource product name price:float stock:int notes:text?
  zapline g resource blog-post
  zapline g resource person --plural people")]
    Resource {
        /// Resource name, e.g. `post` or `blog-post`
        name: String,
        /// Columns for a database-backed resource: `title`, `body:text`, `views:int`,
        /// `price:float`, `done:bool`; add `?` for optional (`bio:text?`)
        #[arg(value_name = "FIELD:TYPE")]
        fields: Vec<String>,
        /// Plural form, for irregular nouns the built-in rules get wrong
        #[arg(long, value_name = "WORD")]
        plural: Option<String>,
        /// Use an in-memory store even in a project with a database
        #[arg(long)]
        memory: bool,
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

    /// Tailwind CSS without Node: styles/app.css compiled to static/app.css
    #[command(after_long_help = "\
Creates styles/app.css (`@import \"tailwindcss\"` plus the CSS that was in static/app.css)
and builds static/app.css with the standalone Tailwind binary, downloaded once and
checked against its SHA-256. From then on `zapline dev` rebuilds it as you edit
templates, and `zapline css` builds it on demand.

Examples:
  zapline g tailwind")]
    Tailwind,
}

/// Arguments of `zapline dev`.
#[derive(Debug, Args)]
pub struct DevArgs {
    /// Binary to run [default: the package's only binary]
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Build in release mode
    #[arg(long)]
    pub release: bool,

    /// Keep a TypeScript client of the API up to date (see `zapline client`)
    #[arg(long, value_name = "FILE")]
    pub client: Option<PathBuf>,

    /// How long (ms) files must stay quiet before rebuilding
    #[arg(long, value_name = "MS", default_value_t = 300, hide = true)]
    pub debounce_ms: u64,

    /// Arguments passed to the app (after `--`)
    #[arg(last = true, value_name = "APP_ARGS")]
    pub app_args: Vec<String>,
}

/// Arguments of `zapline routes`.
#[derive(Debug, Args)]
pub struct RoutesArgs {
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
    /// Where to write the client
    #[arg(short, long, value_name = "FILE", default_value = "client.ts")]
    pub output: PathBuf,

    /// Binary to run [default: the package's only binary]
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,
}

/// `zapline db` subcommands.
#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Create an empty migration file in migrations/
    New {
        /// What the migration does, e.g. `create_posts`
        name: String,
    },
    /// Apply pending migrations (runs the app with ZAPLINE_MIGRATE_ONLY=1)
    Migrate {
        /// Binary to run [default: the package's only binary]
        #[arg(long, value_name = "NAME")]
        bin: Option<String>,
    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
        assert_eq!(a.template, Template::Api);
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
        assert_eq!(a.template, Template::Full);
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
