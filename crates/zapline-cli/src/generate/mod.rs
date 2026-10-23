//! `zapline generate`: add resources, handlers and middleware to an existing project.
//!
//! Every generator is split into a pure planning step (what files to write and
//! how to edit `mod.rs`) and a small writing step, so conflicts and malformed
//! markers are detected before anything is changed on disk.

pub mod auth;
pub mod db_resource;
pub mod deploy;
pub mod job;
pub mod register;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;

use crate::naming::{crate_ident, kebab_case, snake_case, validate_identifier, ResourceNames};
use crate::project::Project;
use crate::render::{ensure_relative, render, write_files, OutputFile};
use crate::templates;
use crate::ui;
use register::{declare_in_module_file, register_route_module};

/// What a generator did, and what is left for the user to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// New files, relative to the project root.
    pub created: Vec<PathBuf>,
    /// Existing files that were edited, relative to the project root.
    pub modified: Vec<PathBuf>,
    /// Follow-up instructions for things the generator cannot safely do itself.
    pub notes: Vec<String>,
}

impl Report {
    /// Prints the report as cargo-style status lines.
    pub fn print(&self) {
        for p in &self.created {
            ui::status("Created", display(p));
        }
        for p in &self.modified {
            ui::status("Modified", display(p));
        }
        for note in &self.notes {
            eprintln!();
            for (i, line) in note.lines().enumerate() {
                if i == 0 {
                    ui::note(line);
                } else {
                    eprintln!("  {line}");
                }
            }
        }
    }
}

fn display(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// HTTP verbs `generate handler` can scaffold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HandlerMethod {
    /// Read a resource (responds 200).
    Get,
    /// Create something (responds 201).
    Post,
    /// Replace a resource (responds 200).
    Put,
    /// Partially update a resource (responds 200).
    Patch,
    /// Remove a resource (responds 204).
    Delete,
}

impl HandlerMethod {
    fn lower(self) -> &'static str {
        match self {
            HandlerMethod::Get => "get",
            HandlerMethod::Post => "post",
            HandlerMethod::Put => "put",
            HandlerMethod::Patch => "patch",
            HandlerMethod::Delete => "delete",
        }
    }
}

// ---- shared planning ----------------------------------------------------------

/// A pending edit of an existing (or new) module file.
enum ModEdit {
    /// Write `text` to `path`; `is_new` tells whether the file did not exist.
    Write {
        path: PathBuf,
        text: String,
        is_new: bool,
    },
    /// Nothing to change.
    Unchanged,
    /// Could not be done safely; show the note instead.
    Manual(String),
}

/// Decides how to register `module` in `src/routes/mod.rs`.
fn plan_route_registration(project: &Project, module: &str) -> Result<ModEdit> {
    let rel = PathBuf::from("src/routes/mod.rs");
    let mod_rs = project.root.join(&rel);
    let mount = format!("app = app.plugin({module}::mount);");

    let manual = |why: &str| {
        ModEdit::Manual(format!(
            "{why}\nregister the module yourself:\n\
             \x20 pub mod {module};\n\
             \x20 // ...and where you build the app:\n\
             \x20 let app = app.plugin({module}::mount);"
        ))
    };

    if mod_rs.is_file() {
        let text = fs::read_to_string(&mod_rs)
            .with_context(|| format!("cannot read {}", mod_rs.display()))?;
        return Ok(match register_route_module(&text, module, &mount)? {
            Some(r) if r.text == text => ModEdit::Unchanged,
            Some(r) => ModEdit::Write {
                path: rel,
                text: r.text,
                is_new: false,
            },
            None => manual(
                "src/routes/mod.rs has no `// zapline:routes:start` / `// zapline:routes:end` markers, so the module was not registered automatically",
            ),
        });
    }
    if project.root.join("src/routes.rs").is_file() {
        return Ok(manual(
            "this project declares `routes` in src/routes.rs, so the module was not registered automatically",
        ));
    }

    // No routes module yet: create one with the markers.
    let fresh = register_route_module(templates::ROUTES_MOD, module, &mount)?
        .context("internal error: the routes template has no markers")?;
    Ok(ModEdit::Write {
        path: rel,
        text: fresh.text,
        is_new: true,
    })
}

/// Notes for the dependencies the generated resource code relies on.
fn missing_dependency_note(project: &Project) -> Option<String> {
    // Models use `#[model]`, which needs no serde/schemars/validator
    // dependency; only the generated tests need these.
    let mut cmds: Vec<&str> = Vec::new();
    if !project.dev_depends_on("serde_json") {
        cmds.push("cargo add --dev serde_json");
    }
    if !project.dev_depends_on("tokio") {
        cmds.push("cargo add --dev tokio --features full");
    }
    if cmds.is_empty() {
        return None;
    }
    let mut note =
        String::from("the generated code needs dependencies this project does not have yet; run:");
    for c in cmds {
        note.push_str("\n\x20 ");
        note.push_str(c);
    }
    Some(note)
}

/// Extra notes after a new `src/routes/mod.rs` was created.
fn new_routes_module_note(project: &Project) -> String {
    let lib = if project.lib_name.is_some() {
        "src/lib.rs"
    } else {
        "src/main.rs"
    };
    format!(
        "created src/routes/mod.rs; wire it into the app:\n\
         \x20 add `pub mod routes;` to {lib}\n\
         \x20 and install it with `.plugin(routes::install)` on your `App`"
    )
}

fn warn_if_not_zapline(project: &Project, report: &mut Report) {
    if !project.depends_on("zapline") {
        report.notes.push(
            "this project does not depend on `zapline` yet; run:\n\x20 cargo add zapline"
                .to_string(),
        );
    }
}

fn no_lib_note(what: &str) -> String {
    format!(
        "no src/lib.rs, so the {what} test was not generated\n\
         integration tests can only import library crates; move your app setup into src/lib.rs to test it"
    )
}

/// Formats freshly generated files with the project's rustfmt settings. Best
/// effort: without rustfmt the files are already close to formatted.
fn format_files(project: &Project, files: &[PathBuf]) {
    if files.is_empty() {
        return;
    }
    let _ = Command::new("rustfmt")
        .arg("--edition")
        .arg(&project.edition)
        .args(files.iter().map(|f| project.root.join(f)))
        .current_dir(&project.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Writes new files, then applies the module edit; reports what happened.
/// Returns whether a new `mod.rs` was created.
fn apply(
    project: &Project,
    files: Vec<OutputFile>,
    edit: ModEdit,
    force: bool,
    report: &mut Report,
) -> Result<bool> {
    // Fails before writing anything if a file exists (unless forced).
    write_files(&project.root, &files, force)?;
    let rust_files: Vec<PathBuf> = files
        .iter()
        .map(|f| f.path.clone())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    format_files(project, &rust_files);
    report.created.extend(files.into_iter().map(|f| f.path));

    let mut new_module_file = false;
    match edit {
        ModEdit::Write { path, text, is_new } => {
            ensure_relative(&path)?;
            let target = project.root.join(&path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, text)
                .with_context(|| format!("cannot write {}", target.display()))?;
            if is_new {
                report.created.push(path);
                new_module_file = true;
            } else {
                report.modified.push(path);
            }
        }
        ModEdit::Unchanged => {}
        ModEdit::Manual(note) => report.notes.push(note),
    }
    Ok(new_module_file)
}

// ---- resource ---------------------------------------------------------------------

/// Renders the resource module (and its test, when `with_test`).
pub fn resource_files(
    crate_name: &str,
    names: &ResourceNames,
    with_test: bool,
) -> Result<Vec<OutputFile>> {
    let vars = [
        ("singular", names.singular.as_str()),
        ("plural", names.plural.as_str()),
        ("pascal", names.pascal.as_str()),
        ("pascal_plural", names.pascal_plural.as_str()),
        ("path", names.path.as_str()),
        ("human", names.human.as_str()),
        ("a_human", names.a_human.as_str()),
        ("plural_human", names.plural_human.as_str()),
        ("crate_name", crate_name),
    ];
    let module = &names.plural;
    let mut files = vec![OutputFile {
        path: PathBuf::from(format!("src/routes/{module}.rs")),
        contents: render(templates::RESOURCE, &vars)?,
    }];
    if with_test {
        files.push(OutputFile {
            path: PathBuf::from(format!("tests/{module}.rs")),
            contents: render(templates::RESOURCE_TEST, &vars)?,
        });
    }
    Ok(files)
}

/// `zapline generate resource <name> [field:type ...]`.
///
/// In a project with a `migrations/` directory this generates a SQL-backed
/// resource (see [`db_resource`]); `memory` forces the in-memory store.
pub fn resource_with_fields(
    dir: &Path,
    name: &str,
    plural: Option<&str>,
    fields: &[String],
    memory: bool,
    force: bool,
) -> Result<Report> {
    let project = Project::discover(dir)?;
    if !memory && project.root.join(crate::db::MIGRATIONS_DIR).is_dir() {
        return sql_resource(&project, name, plural, fields, force);
    }
    if !fields.is_empty() {
        bail!(
            "fields like `{}` need a database: this project has no migrations/ directory\n\
             hint: create one with `zapline db new init` (and enable zapline's `sqlite` or `postgres` feature), \
             or start from `zapline new <name> --template fullstack`",
            fields[0]
        );
    }
    resource(dir, name, plural, force)
}

fn sql_resource(
    project: &Project,
    name: &str,
    plural: Option<&str>,
    fields: &[String],
    force: bool,
) -> Result<Report> {
    let names = ResourceNames::parse(name, plural)?;
    let fields = db_resource::parse_fields(fields)?;
    let manifest = fs::read_to_string(project.root.join("Cargo.toml")).unwrap_or_default();
    let dialect = if manifest.contains("\"postgres\"") && !manifest.contains("\"sqlite\"") {
        db_resource::Dialect::Postgres
    } else {
        db_resource::Dialect::Sqlite
    };
    let html = project.root.join("templates/base.html").is_file();
    let lib = fs::read_to_string(project.root.join("src/lib.rs")).unwrap_or_default();
    let testable = project.lib_name.is_some() && lib.contains("pub fn app(db: Database)");
    let krate = crate_name_of(project);
    let plan = db_resource::Plan {
        names: &names,
        fields: &fields,
        dialect,
        html,
        // Flash messages need sessions: the fullstack template and `g auth` add them.
        flash: auth::has_sessions(&lib) || project.root.join("src/routes/auth.rs").is_file(),
        test_crate: testable.then_some(krate.as_str()),
        // Regenerating (--force) rewrites the existing migration instead of adding a second one.
        migration_name: match existing_migration(project, &format!("_create_{}.sql", names.plural))
        {
            Some(name) => name,
            None => {
                crate::db::new_migration_name(&project.root, &format!("create_{}", names.plural))?
            }
        },
    };
    let files = db_resource::files(&plan);

    let mut report = Report::default();
    let edit = plan_route_registration(project, &names.plural)?;
    if apply(project, files, edit, force, &mut report)? {
        report.notes.push(new_routes_module_note(project));
    }
    if !testable {
        report.notes.push(
            "no test was generated: tests build the app with `app(Database::sqlite_memory())`, \
             so src/lib.rs needs `pub fn app(db: Database) -> App` (the fullstack template has it)"
                .to_string(),
        );
    }
    if !project.depends_on("sqlx") {
        report
            .notes
            .push("the generated code derives sqlx::FromRow; run:\n\x20 cargo add sqlx@0.8 --no-default-features --features derive".to_string());
    }
    report.notes.push(format!(
        "the table is created by the new migration when the app starts (or run `zapline db migrate`); \
         open {} to try it",
        if html { names.path.clone() } else { "/docs".to_string() }
    ));
    warn_if_not_zapline(project, &mut report);
    if let Some(note) = missing_dependency_note(project) {
        report.notes.push(note);
    }
    Ok(report)
}

/// `zapline generate resource <name>`: model, CRUD handlers, in-memory store and tests.
pub fn resource(dir: &Path, name: &str, plural: Option<&str>, force: bool) -> Result<Report> {
    let project = Project::discover(dir)?;
    let names = ResourceNames::parse(name, plural)?;
    let with_test = project.lib_name.is_some();
    let files = resource_files(&crate_name_of(&project), &names, with_test)?;

    let mut report = Report::default();
    if !with_test {
        report.notes.push(no_lib_note("resource"));
    }
    let edit = plan_route_registration(&project, &names.plural)?;
    if apply(&project, files, edit, force, &mut report)? {
        report.notes.push(new_routes_module_note(&project));
    }

    warn_if_not_zapline(&project, &mut report);
    if let Some(note) = missing_dependency_note(&project) {
        report.notes.push(note);
    }
    Ok(report)
}

/// The crate name tests import: the library name, or the package name as a fallback.
/// The file name of an existing migration ending in `suffix`.
fn existing_migration(project: &Project, suffix: &str) -> Option<String> {
    let dir = project.root.join(crate::db::MIGRATIONS_DIR);
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.ends_with(suffix))
}

fn crate_name_of(project: &Project) -> String {
    project
        .lib_name
        .clone()
        .unwrap_or_else(|| crate_ident(&project.package_name))
}

// ---- handler ----------------------------------------------------------------------

/// Checks a route path and returns its parameter names (`{id}` or `{*rest}`).
pub fn route_params(path: &str) -> Result<Vec<String>> {
    if !path.starts_with('/') {
        bail!("route path `{path}` must start with `/`");
    }
    if path.len() > 200 {
        bail!("route path is too long (max 200 characters)");
    }
    if let Some(bad) = path
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\\' | '?' | '#'))
    {
        bail!("route path `{path}` contains unsupported character {bad:?}");
    }
    let segments: Vec<&str> = path[1..].split('/').collect();
    let mut params: Vec<String> = Vec::new();
    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() && i + 1 != segments.len() {
            bail!("route path `{path}` has an empty segment");
        }
        if !seg.contains(['{', '}']) {
            continue;
        }
        let inner = seg
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .with_context(|| {
                format!("`{seg}` in `{path}`: a parameter must be a whole segment like `{{id}}`")
            })?;
        let (wildcard, name) = match inner.strip_prefix('*') {
            Some(n) => (true, n),
            None => (false, inner),
        };
        if wildcard && i + 1 != segments.len() {
            bail!("wildcard `{{*{name}}}` must be the last segment of `{path}`");
        }
        validate_identifier(name, name)
            .with_context(|| format!("invalid parameter in route path `{path}`"))?;
        if params.iter().any(|p| p == name) {
            bail!("parameter `{name}` appears twice in `{path}`");
        }
        params.push(name.to_string());
    }
    Ok(params)
}

/// The rendered pieces of a handler's signature and body.
struct HandlerShape {
    args: String,
    ret: String,
    body: String,
    status: &'static str,
}

fn handler_shape(method: HandlerMethod, params: &[String]) -> HandlerShape {
    if params.is_empty() {
        let (ret, body, status) = match method {
            HandlerMethod::Get => ("&'static str", "\"ok\"", "200"),
            HandlerMethod::Post => (
                "(StatusCode, &'static str)",
                "(StatusCode::CREATED, \"created\")",
                "201",
            ),
            HandlerMethod::Put | HandlerMethod::Patch => ("&'static str", "\"updated\"", "200"),
            HandlerMethod::Delete => ("StatusCode", "StatusCode::NO_CONTENT", "204"),
        };
        return HandlerShape {
            args: String::new(),
            ret: ret.into(),
            body: body.into(),
            status,
        };
    }
    let args = match params {
        [one] => format!("Path({one}): Path<String>"),
        many => format!(
            "Path(({})): Path<({})>",
            many.join(", "),
            vec!["String"; many.len()].join(", ")
        ),
    };
    // A lone parameter is already a String; clippy flags `format!("{id}")`.
    let message = match params {
        [one] => one.clone(),
        many => format!(
            "format!(\"{}\")",
            many.iter()
                .map(|p| format!("{{{p}}}"))
                .collect::<Vec<_>>()
                .join("/")
        ),
    };
    let (ret, body, status) = match method {
        HandlerMethod::Post => (
            "(StatusCode, String)".to_string(),
            format!("(StatusCode::CREATED, {message})"),
            "201",
        ),
        _ => ("String".to_string(), message, "200"),
    };
    HandlerShape {
        args,
        ret,
        body,
        status,
    }
}

/// Replaces `{param}` segments with a sample value for the generated test.
fn sample_path(path: &str) -> String {
    path.split('/')
        .map(|seg| if seg.starts_with('{') { "1" } else { seg })
        .collect::<Vec<_>>()
        .join("/")
}

/// Renders the handler module (and its test, when `with_test`).
pub fn handler_files(
    crate_name: &str,
    module: &str,
    method: HandlerMethod,
    path: &str,
    with_test: bool,
) -> Result<Vec<OutputFile>> {
    let params = route_params(path)?;
    let shape = handler_shape(method, &params);
    let method_lower = method.lower();
    let method_upper = method_lower.to_uppercase();
    let sample = sample_path(path);
    let vars = [
        ("name", module),
        ("method", method_lower),
        ("METHOD", method_upper.as_str()),
        ("path", path),
        ("args", shape.args.as_str()),
        ("ret", shape.ret.as_str()),
        ("body", shape.body.as_str()),
        ("status", shape.status),
        ("sample_path", sample.as_str()),
        ("crate_name", crate_name),
    ];
    let mut files = vec![OutputFile {
        path: PathBuf::from(format!("src/routes/{module}.rs")),
        contents: render(templates::HANDLER, &vars)?,
    }];
    if with_test {
        files.push(OutputFile {
            path: PathBuf::from(format!("tests/{module}.rs")),
            contents: render(templates::HANDLER_TEST, &vars)?,
        });
    }
    Ok(files)
}

/// `zapline generate handler <name>`: a single route in `src/routes/<name>.rs`.
pub fn handler(
    dir: &Path,
    name: &str,
    method: HandlerMethod,
    path: Option<&str>,
    force: bool,
) -> Result<Report> {
    let project = Project::discover(dir)?;
    let module = snake_case(name);
    validate_identifier(&module, name)?;
    let path = match path {
        Some(p) => p.to_string(),
        None => format!("/{}", kebab_case(&module)),
    };
    let with_test = project.lib_name.is_some();
    let files = handler_files(&crate_name_of(&project), &module, method, &path, with_test)?;

    let mut report = Report::default();
    if !with_test {
        report.notes.push(no_lib_note("handler"));
    }
    let edit = plan_route_registration(&project, &module)?;
    if apply(&project, files, edit, force, &mut report)? {
        report.notes.push(new_routes_module_note(&project));
    }
    warn_if_not_zapline(&project, &mut report);
    Ok(report)
}

// ---- middleware -------------------------------------------------------------------

/// Renders the middleware module (and its test, when `with_test`).
pub fn middleware_files(
    crate_name: &str,
    module: &str,
    with_test: bool,
) -> Result<Vec<OutputFile>> {
    let kebab = kebab_case(module);
    let vars = [
        ("name", module),
        ("kebab", kebab.as_str()),
        ("crate_name", crate_name),
    ];
    let mut files = vec![OutputFile {
        path: PathBuf::from(format!("src/middleware/{module}.rs")),
        contents: render(templates::MIDDLEWARE, &vars)?,
    }];
    if with_test {
        files.push(OutputFile {
            path: PathBuf::from(format!("tests/{module}_middleware.rs")),
            contents: render(templates::MIDDLEWARE_TEST, &vars)?,
        });
    }
    Ok(files)
}

/// `zapline generate middleware <name>`: `src/middleware/<name>.rs` plus a test.
pub fn middleware(dir: &Path, name: &str, force: bool) -> Result<Report> {
    let project = Project::discover(dir)?;
    let module = snake_case(name);
    validate_identifier(&module, name)?;
    let with_test = project.lib_name.is_some();
    let files = middleware_files(&crate_name_of(&project), &module, with_test)?;

    let mut report = Report::default();
    if !with_test {
        report.notes.push(no_lib_note("middleware"));
    }

    // src/middleware/mod.rs: create or extend.
    let rel = PathBuf::from("src/middleware/mod.rs");
    let mod_rs = project.root.join(&rel);
    let edit = if mod_rs.is_file() {
        let text = fs::read_to_string(&mod_rs)
            .with_context(|| format!("cannot read {}", mod_rs.display()))?;
        let (updated, changed) = declare_in_module_file(&text, &module);
        if changed {
            ModEdit::Write {
                path: rel,
                text: updated,
                is_new: false,
            }
        } else {
            ModEdit::Unchanged
        }
    } else if project.root.join("src/middleware.rs").is_file() {
        ModEdit::Manual(format!(
            "this project declares `middleware` in src/middleware.rs, so the module was not registered automatically\n\
             add `pub mod {module};` there"
        ))
    } else {
        let (text, _) = declare_in_module_file(templates::MIDDLEWARE_MOD, &module);
        ModEdit::Write {
            path: rel,
            text,
            is_new: true,
        }
    };
    apply(&project, files, edit, force, &mut report)?;

    if !declares_module(&project, "middleware") {
        let lib = if project.lib_name.is_some() {
            "src/lib.rs"
        } else {
            "src/main.rs"
        };
        report.notes.push(format!(
            "declare the module: add `pub mod middleware;` to {lib}"
        ));
    }
    report.notes.push(format!(
        "register it on your app (runs before routing, for every request):\n\x20 app.wrap(middleware::{module}::{module})"
    ));
    warn_if_not_zapline(&project, &mut report);
    Ok(report)
}

/// Whether `src/lib.rs` or `src/main.rs` already declares `mod <name>;`.
fn declares_module(project: &Project, name: &str) -> bool {
    ["src/lib.rs", "src/main.rs"].iter().any(|f| {
        fs::read_to_string(project.root.join(f))
            .map(|text| {
                text.lines().any(|l| {
                    let l = l.trim();
                    l == format!("mod {name};")
                        || l == format!("pub mod {name};")
                        || l == format!("pub(crate) mod {name};")
                })
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests;
