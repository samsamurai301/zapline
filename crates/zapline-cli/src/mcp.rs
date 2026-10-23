//! `zapline mcp`: a [Model Context Protocol] server over stdio, so AI coding
//! assistants (Claude Code, Cursor, Zed, VS Code...) can use zapline's tools
//! directly instead of shelling out and reading pages of output.
//!
//! Every tool answers in as few tokens as it can: the docs tool returns the
//! one section asked for rather than the whole guide, `check` and `test`
//! return one line per problem (see [`crate::brief`]), and generators list the
//! files they touched.
//!
//! The live tools (`recent_requests`, `recent_errors`, `call_route`) talk to
//! the running dev app (see [`crate::live`]), and any answer that mentions a
//! diagnostic code (`ZL0002`) ends with that code's fix, so the model never
//! has to look it up.
//!
//! The transport is newline-delimited JSON-RPC 2.0 on stdin/stdout. Nothing
//! else is written to stdout; logs and the app's own stderr go to stderr.
//!
//! [Model Context Protocol]: https://modelcontextprotocol.io

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::ValueEnum;
use serde_json::{json, Value};

use crate::generate::{self, HandlerMethod, Report};
use crate::project::Project;
use crate::templates::Template;
use crate::{brief, build, codes, db, live, new, openapi, routes};

/// The whole guide (docs/src/llms-full.txt, copied by scripts/gen-llms.sh).
pub const GUIDE: &str = include_str!("../assets/guide.md");

/// Protocol revisions this server speaks, newest first.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// What a client learns at `initialize`, before it reads any tool.
const INSTRUCTIONS: &str = "\
zapline is a Rust web framework. Before writing zapline code, call `docs` (no topic) for the \
cheatsheet, or `docs` with a topic (\"extractors\", \"database\", \"htmx\", \"errors\"...). \
Conventions: handlers are `async fn` with extractor arguments (Path, Query, Json, Valid<Json<T>>, \
State, Dep, Db); return `zapline::Result<T>`; errors are `Error::not_found()` etc.; no \
#[tokio::main] (App::run owns the runtime). Prefer the generators over writing boilerplate, and \
`check`/`test` over raw cargo: their output is condensed. While the app runs under `zapline dev`, \
debug with `recent_errors` (each error has a ZL code and its fix), `call_route` and \
`recent_requests`; `explain_error` explains any ZL code.";

/// Runs the server until stdin closes.
pub fn run() -> Result<()> {
    // The app's stderr would otherwise interleave with the client's logs
    // without ever reaching the model; keep it for error messages instead.
    build::set_echo_stderr(false);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.context("cannot read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg),
            Err(e) => Some(error_reply(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}").context("cannot write stdout")?;
            stdout.flush().context("cannot write stdout")?;
        }
    }
    Ok(())
}

/// Answers one JSON-RPC message; `None` for notifications.
pub fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    // Notifications (`notifications/initialized`, `notifications/cancelled`)
    // and stray responses get no reply.
    let id = id?;
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_list() })),
        "tools/call" => Ok(call(&params)),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error_reply(id, code, &message),
    })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "zapline", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

// ---------------------------------------------------------------------------
// Tools

fn tool_list() -> Value {
    let bin = json!({ "type": "string", "description": "Binary target; default: the package's only binary" });
    let url = json!({ "type": "string", "description": "The app's base URL; default: the address `zapline dev` recorded, else http://127.0.0.1:8080" });
    json!([
        {
            "name": "docs",
            "description": "zapline documentation. No topic: the cheatsheet (canonical code for common tasks) and the list of pages. With a topic: the best-matching guide sections.",
            "inputSchema": { "type": "object", "properties": {
                "topic": { "type": "string", "description": "A page name or keywords, e.g. \"extractors\", \"database migrations\", \"htmx partial\"" }
            }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "new_project",
            "description": "Create a zapline project. Templates: minimal, api (default), full, fullstack (htmx pages + SQLite + JSON API).",
            "inputSchema": { "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string", "description": "Package name, e.g. my-app" },
                "template": { "type": "string", "enum": ["minimal", "api", "full", "fullstack"] },
                "path": { "type": "string", "description": "Parent directory; default: the current directory" }
            }}
        },
        {
            "name": "generate_resource",
            "description": "Generate a CRUD resource: routes, tests and registration. In a project with migrations/ it is SQL-backed (migration, JSON API, htmx page if templates/base.html exists).",
            "inputSchema": { "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string", "description": "Singular, e.g. product" },
                "fields": { "type": "array", "items": { "type": "string" }, "description": "name:type with type string|text|int|float|bool, `?` suffix for optional, e.g. [\"name\", \"price:float\", \"notes:text?\"]" },
                "plural": { "type": "string" },
                "memory": { "type": "boolean", "description": "In-memory store even if the project has a database" },
                "force": { "type": "boolean", "description": "Overwrite existing files" }
            }}
        },
        {
            "name": "generate_handler",
            "description": "Generate one route handler module with a test and register it.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string" },
                "method": { "type": "string", "enum": ["get", "post", "put", "patch", "delete"] },
                "path": { "type": "string", "description": "e.g. /reports/{id}; default: /<name>" },
                "force": { "type": "boolean" }
            }}
        },
        {
            "name": "generate_middleware",
            "description": "Generate a middleware module (async fn(Request, Next) -> Response).",
            "inputSchema": { "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string" },
                "force": { "type": "boolean" }
            }}
        },
        {
            "name": "generate_auth",
            "description": "Add accounts to a project with a database: users migration, sign-up/log-in/log-out and password reset by email (JSON API, and htmx pages if templates exist), DB-backed sessions, a CurrentUser extractor, tests.",
            "inputSchema": { "type": "object", "properties": {
                "force": { "type": "boolean" }
            }}
        },
        {
            "name": "generate_job",
            "description": "Add a durable background job (stored in the database, retried with backoff): src/jobs/<name>.rs, registered in src/jobs/mod.rs and app(), plus a test. Enqueue with `jobs.enqueue(MyJob { .. }).await?`.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string", "description": "e.g. send_welcome" },
                "force": { "type": "boolean" }
            }}
        },
        {
            "name": "db_new_migration",
            "description": "Create migrations/<timestamp>_<name>.sql, optionally with its SQL.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string", "description": "e.g. add_email_to_users" },
                "sql": { "type": "string" }
            }}
        },
        {
            "name": "db_migrate",
            "description": "Apply pending migrations using the app's DATABASE_URL.",
            "inputSchema": { "type": "object", "properties": { "bin": bin }}
        },
        {
            "name": "db_seed",
            "description": "Insert the app's seed data (the function given to App::seed) into DATABASE_URL, after migrating.",
            "inputSchema": { "type": "object", "properties": { "bin": bin }}
        },
        {
            "name": "check",
            "description": "Type-check the project (cargo check or clippy, all targets). One line per error/warning with location and suggested fix.",
            "inputSchema": { "type": "object", "properties": {
                "clippy": { "type": "boolean" }
            }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "test",
            "description": "Run the tests. Returns totals and only the failing tests with their panic message; condensed compile errors if tests don't build.",
            "inputSchema": { "type": "object", "properties": {
                "filter": { "type": "string", "description": "Only tests whose name contains this" }
            }}
        },
        {
            "name": "routes",
            "description": "The app's route table: one `METHOD path` line per route.",
            "inputSchema": { "type": "object", "properties": { "bin": bin }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "typescript_client",
            "description": "Write a typed, dependency-free TypeScript client for the app's JSON API (one method per operation, interfaces per schema, typed 422 field errors). A .js/.mjs output gets an ES module with JSDoc types.",
            "inputSchema": { "type": "object", "properties": {
                "output": { "type": "string", "description": "File to write; default: the clients in zapline.toml, else frontend/src/api.ts with a frontend/ directory, else client.ts" },
                "bin": bin
            }}
        },
        {
            "name": "openapi",
            "description": "The app's OpenAPI 3.1 document (JSON). Large; prefer `routes` for an overview.",
            "inputSchema": { "type": "object", "properties": { "bin": bin }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "explain_error",
            "description": "What a zapline diagnostic code (ZL0002) means and the code that fixes it.",
            "inputSchema": { "type": "object", "required": ["code"], "properties": {
                "code": { "type": "string", "description": "e.g. ZL0002" }
            }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "recent_errors",
            "description": "The running dev app's latest errors, newest first: ZL code, status, request, message, panic location, fix. Needs `.dev_console()` and the app running (zapline dev).",
            "inputSchema": { "type": "object", "properties": {
                "limit": { "type": "integer", "description": "Default 10" },
                "url": url
            }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "recent_requests",
            "description": "The running dev app's latest requests, newest first: one `status METHOD path time [ZL code]` line each.",
            "inputSchema": { "type": "object", "properties": {
                "limit": { "type": "integer", "description": "Default 20" },
                "url": url
            }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "call_route",
            "description": "Send one HTTP request to the running dev app. Returns the status, the useful headers and the body (JSON compacted, cut at 2000 bytes); 500s come back as problem+json with the ZL code and fix.",
            "inputSchema": { "type": "object", "required": ["path"], "properties": {
                "method": { "type": "string", "description": "Default GET" },
                "path": { "type": "string", "description": "With the query, e.g. /todos?page=2" },
                "body": { "type": "string", "description": "Sent as JSON when it parses as JSON" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "url": url
            }}
        },
        {
            "name": "last_results",
            "description": "The latest `test`, `check` and `zapline fuzz` results with their age, without running anything.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        }
    ])
}

/// `tools/call`: tool failures are results with `isError`, so the model sees them.
fn call(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);
    let (mut text, is_error) = match run_tool(name, args) {
        Ok(text) => (text, false),
        Err(e) => (format!("{e:#}"), true),
    };
    if name != "explain_error" {
        append_fixes(&mut text);
    }
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn run_tool(name: &str, args: &Value) -> Result<String> {
    let s = |key: &str| args.get(key).and_then(Value::as_str);
    let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);
    let here = Path::new(".");
    match name {
        "docs" => Ok(docs(s("topic"))),
        "new_project" => {
            let template = match s("template") {
                Some(t) => Template::from_str(t, true).map_err(|e| anyhow!(e))?,
                None => Template::Api,
            };
            let name = s("name").context("`name` is required")?;
            let created = new::create(&new::NewOptions {
                name: name.to_string(),
                template,
                parent: PathBuf::from(s("path").unwrap_or(".")),
                zapline_path: None,
                git: true,
            })?;
            Ok(format!(
                "created {} ({} files). Work inside it: pass that directory to tools by starting `zapline mcp` there, or `cd` before running cargo.\n{}",
                created.root.display(),
                created.files.len(),
                created.files.join("\n")
            ))
        }
        "generate_resource" => {
            let fields: Vec<String> = args
                .get("fields")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let name = s("name").context("`name` is required")?;
            let report = generate::resource_with_fields(
                here,
                name,
                s("plural"),
                &fields,
                flag("memory"),
                flag("force"),
            )?;
            Ok(report_text(&report))
        }
        "generate_handler" => {
            let method = match s("method") {
                Some(m) => HandlerMethod::from_str(m, true).map_err(|e| anyhow!(e))?,
                None => HandlerMethod::Get,
            };
            let name = s("name").context("`name` is required")?;
            let report = generate::handler(here, name, method, s("path"), flag("force"))?;
            Ok(report_text(&report))
        }
        "generate_middleware" => {
            let name = s("name").context("`name` is required")?;
            Ok(report_text(&generate::middleware(
                here,
                name,
                flag("force"),
            )?))
        }
        "generate_auth" => {
            let project = Project::discover(here)?;
            Ok(report_text(&generate::auth::generate(
                &project,
                flag("force"),
            )?))
        }
        "generate_job" => {
            let name = s("name").context("`name` is required")?;
            let project = Project::discover(here)?;
            Ok(report_text(&generate::job::generate(
                &project,
                name,
                flag("force"),
            )?))
        }
        "db_new_migration" => {
            let project = Project::discover(here)?;
            let name = s("name").context("`name` is required")?;
            let sql = match s("sql") {
                Some(sql) => format!("{}\n", sql.trim_end()),
                None => format!("-- {name}\n"),
            };
            let path = db::create_migration(&project.root, name, &sql)?;
            Ok(format!(
                "created {}",
                path.strip_prefix(&project.root).unwrap_or(&path).display()
            ))
        }
        "db_migrate" => {
            let project = Project::discover(here)?;
            with_build_errors(&project, || db::migrate(&project, s("bin")))?;
            Ok("migrated: the database is up to date".to_string())
        }
        "db_seed" => {
            let project = Project::discover(here)?;
            with_build_errors(&project, || db::seed(&project, s("bin")))?;
            Ok("seeded: the app's seed data is in the database".to_string())
        }
        "check" => {
            let project = Project::discover(here)?;
            Ok(brief::check(&project, flag("clippy"))?.1)
        }
        "test" => {
            let project = Project::discover(here)?;
            Ok(brief::test(&project, s("filter"))?.1)
        }
        "routes" => {
            let project = Project::discover(here)?;
            with_build_errors(&project, || {
                routes::capture(&project, s("bin")).map(|(_, raw)| raw)
            })
        }
        "openapi" => {
            let project = Project::discover(here)?;
            let target = project.root.join("target").join("zapline-mcp-openapi.json");
            with_build_errors(&project, || openapi::export(&project, s("bin"), &target))?;
            let text = std::fs::read_to_string(&target)
                .with_context(|| format!("cannot read {}", target.display()))?;
            // Compact: pretty-printing roughly doubles the tokens.
            Ok(serde_json::from_str::<Value>(&text).map_or(text, |v| v.to_string()))
        }
        "typescript_client" => {
            let project = Project::discover(here)?;
            let output = s("output").map(PathBuf::from);
            with_build_errors(&project, || {
                crate::client::run(&project, s("bin"), output.as_deref())
            })?;
            Ok(match output {
                Some(o) => format!("wrote {}", o.display()),
                None => "wrote the configured clients".to_string(),
            })
        }
        "explain_error" => live::explain_error(s("code").context("`code` is required")?),
        "recent_errors" => live::recent_errors(&app_url(s("url")), limit(args, 10)),
        "recent_requests" => live::recent_requests(&app_url(s("url")), limit(args, 20)),
        "call_route" => {
            let path = s("path").context("`path` is required")?;
            let headers: Vec<(String, String)> = args
                .get("headers")
                .and_then(Value::as_object)
                .map(|h| {
                    h.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            live::call_route(
                &app_url(s("url")),
                s("method").unwrap_or("GET"),
                path,
                &headers,
                s("body"),
            )
        }
        "last_results" => {
            let project = Project::discover(here)?;
            Ok(live::last_results(&project.root))
        }
        _ => bail!("unknown tool `{name}`"),
    }
}

/// The running app's URL: the tool's `url`, or what the project recorded.
fn app_url(given: Option<&str>) -> String {
    let root = Project::discover(Path::new(".")).ok().map(|p| p.root);
    live::app_url(given, root.as_deref())
}

fn limit(args: &Value, default: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map_or(default, |n| n.clamp(1, 50) as usize)
}

/// Ends `text` with the fix of every diagnostic code it names but does not
/// already explain (startup errors, panics, test failures).
fn append_fixes(text: &mut String) {
    // problem+json from the dev error page has its own `fix`.
    if text.contains("\"fix\":") {
        return;
    }
    let mut extra = String::new();
    for code in codes::find_in(text) {
        // The fix up to `{T}` is the same whatever the error named.
        let known = code.fix.split("{T}").next().unwrap_or(code.fix);
        if !text.contains(known) {
            let fix = code.fix.replace("{T}", "T");
            extra.push_str(&format!("{} {}: fix: {fix}\n", code.code, code.title));
        }
    }
    if !extra.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&extra);
    }
}

/// Runs `f`; when it fails because the app does not build, replaces cargo's
/// one-line "build failed" with the condensed compile errors.
fn with_build_errors<T>(project: &Project, f: impl FnOnce() -> Result<T>) -> Result<T> {
    f().map_err(|e| {
        if format!("{e:#}").contains("build failed") {
            match brief::check(project, false) {
                Ok((_, report)) => anyhow!("the app does not build:\n{report}"),
                Err(_) => e,
            }
        } else {
            e
        }
    })
}

fn report_text(report: &Report) -> String {
    let mut out = String::new();
    for p in &report.created {
        out.push_str(&format!("created {}\n", p.display()));
    }
    for p in &report.modified {
        out.push_str(&format!("modified {}\n", p.display()));
    }
    for note in &report.notes {
        out.push_str(&format!("note: {note}\n"));
    }
    if out.is_empty() {
        out.push_str("nothing to do\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Docs search

/// A `## ` section of a guide page (or the page's intro).
#[derive(Debug)]
struct Section<'a> {
    title: &'a str,
    heading: &'a str,
    body: &'a str,
}

/// A page of the guide: `<!-- file.md -->` up to the next marker.
#[derive(Debug)]
struct Page<'a> {
    file: &'a str,
    title: &'a str,
    body: &'a str,
}

fn pages(guide: &str) -> Vec<Page<'_>> {
    let mut out = Vec::new();
    let mut rest = guide;
    while let Some(start) = rest.find("<!-- ") {
        let after = &rest[start + 5..];
        let Some(end) = after.find(".md -->") else {
            break;
        };
        let file = &after[..end];
        let body_start = end + ".md -->".len();
        let next = after[body_start..]
            .find("\n<!-- ")
            .map_or(after.len(), |n| body_start + n);
        let body = after[body_start..next]
            .trim()
            .trim_end_matches("---")
            .trim();
        let title = body
            .lines()
            .find_map(|l| l.strip_prefix("# "))
            .unwrap_or(file)
            .trim();
        out.push(Page { file, title, body });
        rest = &after[next..];
    }
    out
}

fn sections<'a>(page: &Page<'a>) -> Vec<Section<'a>> {
    let mut out = Vec::new();
    let mut heading = page.title;
    // The intro section starts after the page's `# Title` line.
    let mut start = page
        .body
        .strip_prefix("# ")
        .and_then(|_| page.body.find('\n'))
        .unwrap_or(0);
    let mut in_code = false;
    let mut offset = 0;
    for line in page.body.split_inclusive('\n') {
        if line.starts_with("```") {
            in_code = !in_code;
        } else if !in_code && line.starts_with("## ") {
            out.push(Section {
                title: page.title,
                heading,
                body: page.body[start..offset].trim(),
            });
            heading = line[3..].trim();
            start = offset;
        }
        offset += line.len();
    }
    out.push(Section {
        title: page.title,
        heading,
        body: page.body[start..].trim(),
    });
    out
}

/// Most text returned for one query.
const DOCS_BUDGET: usize = 9_000;
/// Most text for the "Also relevant" pointers after it.
const ALSO_BUDGET: usize = 200;

fn page_list(pages: &[Page<'_>]) -> String {
    let names: Vec<String> = pages
        .iter()
        .map(|p| format!("{} ({})", p.file, p.title))
        .collect();
    format!("Pages: {}", names.join(", "))
}

/// The `docs` tool.
pub fn docs(topic: Option<&str>) -> String {
    let pages = pages(GUIDE);
    let topic = topic.map(str::trim).filter(|t| !t.is_empty());
    let Some(topic) = topic else {
        let cheatsheet = pages
            .iter()
            .find(|p| p.file == "cheatsheet")
            .map_or("", |p| p.body);
        return format!(
            "{cheatsheet}\n\n{}\nAsk `docs` with a page name or keywords for more.",
            page_list(&pages)
        );
    };
    let q = topic.to_lowercase();

    // A page by file name or title: the whole page, if it fits.
    if let Some(page) = pages
        .iter()
        .find(|p| p.file.eq_ignore_ascii_case(&q) || p.title.eq_ignore_ascii_case(&q))
    {
        if page.body.len() <= DOCS_BUDGET {
            return page.body.to_string();
        }
        let headings: Vec<&str> = sections(page).iter().map(|s| s.heading).collect();
        let first = &sections(page)[0];
        return format!(
            "{}\n\n(The page is long; ask for one of its sections: {}.)",
            first.body,
            headings[1..].join("; ")
        );
    }

    // Otherwise rank sections: words in headings count most.
    let words: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() > 1)
        .map(|w| w.trim_end_matches('s').to_string())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return page_list(&pages);
    }
    let mut scored: Vec<(usize, Section<'_>)> = pages
        .iter()
        .flat_map(sections)
        .filter_map(|s| {
            let heading = s.heading.to_lowercase();
            let title = s.title.to_lowercase();
            let body = s.body.to_lowercase();
            let mut score = 0;
            for w in &words {
                let hits = body.matches(w.as_str()).count();
                let in_heading = heading.contains(w.as_str());
                let in_title = title.contains(w.as_str());
                if hits == 0 && !in_heading && !in_title {
                    return None;
                }
                score += 20 * usize::from(in_heading) + 8 * usize::from(in_title) + hits.min(10);
            }
            Some((score, s))
        })
        .collect();
    if scored.is_empty() {
        return format!("No section mentions \"{topic}\".\n{}", page_list(&pages));
    }
    scored.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut out = String::new();
    let mut more = Vec::new();
    for (_, s) in &scored {
        let block = format!("## {} › {}\n\n{}\n\n", s.title, s.heading, s.body);
        if out.is_empty() || out.len() + block.len() <= DOCS_BUDGET {
            if block.len() > DOCS_BUDGET {
                let cut = (0..=DOCS_BUDGET)
                    .rev()
                    .find(|&i| block.is_char_boundary(i))
                    .unwrap_or(0);
                out.push_str(&block[..cut]);
                out.push_str("\n[cut]\n\n");
            } else {
                out.push_str(&block);
            }
        } else {
            // A short pointer list: at most 6 entries and about 200 bytes.
            let entry = format!("{} › {}", s.title, s.heading);
            let used: usize = more.iter().map(|m: &String| m.len() + 2).sum();
            if more.len() < 6 && used + entry.len() <= 160 {
                more.push(entry);
            }
        }
    }
    // Pointers to the sections that didn't fit, as many as fit in their own
    // small budget, so the answer never grows past both.
    let mut also = String::from("Also relevant: ");
    let mut listed = 0;
    for name in &more {
        let sep = if listed == 0 { "" } else { "; " };
        if also.len() + sep.len() + name.len() + 1 > ALSO_BUDGET {
            break;
        }
        also.push_str(sep);
        also.push_str(name);
        listed += 1;
    }
    if listed > 0 {
        also.push('\n');
        out.push_str(&also);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: Value) -> Value {
        handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).unwrap()
    }

    #[test]
    fn initializes_with_a_supported_protocol_version() {
        let r = request(
            "initialize",
            json!({ "protocolVersion": "2025-03-26", "capabilities": {} }),
        );
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "zapline");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        let r = request("initialize", json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn notifications_get_no_reply_and_unknown_methods_an_error() {
        assert!(
            handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none()
        );
        let r = request("resources/list", json!({}));
        assert_eq!(r["error"]["code"], -32601);
        assert_eq!(request("ping", Value::Null)["result"], json!({}));
    }

    #[test]
    fn every_listed_tool_has_a_schema_and_is_dispatched() {
        let r = request("tools/list", json!({}));
        let tools = r["result"]["tools"].as_array().unwrap();
        assert!(tools.len() >= 10);
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
            let name = t["name"].as_str().unwrap();
            // Only tools that fail on the missing argument before doing
            // anything; the others would run cargo on this very crate.
            if t["inputSchema"]["required"] == json!(["name"]) {
                let err = run_tool(name, &json!({})).unwrap_err().to_string();
                assert!(err.contains("`name` is required"), "{name}: {err}");
            }
        }
        let r = request("tools/call", json!({ "name": "nope", "arguments": {} }));
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn docs_without_topic_is_the_cheatsheet() {
        let text = docs(None);
        assert!(text.contains("#[get("), "{text}");
        assert!(text.contains("Pages: "));
        // About 3k tokens: the cheatsheet, its example file and the page list.
        assert!(text.len() < 13_500, "{}", text.len());
    }

    #[test]
    fn docs_finds_pages_and_sections() {
        assert!(docs(Some("errors")).contains("not_found"));
        let db = docs(Some("database migrations"));
        assert!(db.to_lowercase().contains("migrat"), "{db}");
        assert!(db.len() <= DOCS_BUDGET + ALSO_BUDGET);
        let htmx = docs(Some("htmx partial"));
        assert!(htmx.contains("HxRequest"), "{htmx}");
        assert!(docs(Some("qwertyuiop")).starts_with("No section"));
    }

    #[test]
    fn the_guide_splits_into_pages_with_titles() {
        let pages = pages(GUIDE);
        assert!(pages.len() > 20);
        assert!(pages.iter().any(|p| p.file == "cheatsheet"));
        assert!(pages
            .iter()
            .all(|p| !p.title.is_empty() && !p.body.contains("\n<!-- ")));
    }
}
