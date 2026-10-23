//! `zapline generate`: add resources, handlers and middleware to an existing project.
//!
//! Every generator is split into a pure planning step (what files to write and
//! how to edit `mod.rs`) and a small writing step, so conflicts and malformed
//! markers are detected before anything is changed on disk.

pub mod auth;
pub mod db_resource;
pub mod deploy;
pub mod evolve;
pub mod job;
pub mod mailer;
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

/// The SQL dialect of a project: Postgres when its manifest enables only
/// zapline's `postgres` feature, otherwise SQLite.
pub fn dialect_of(manifest: &str) -> db_resource::Dialect {
    if manifest.contains("\"postgres\"") && !manifest.contains("\"sqlite\"") {
        db_resource::Dialect::Postgres
    } else {
        db_resource::Dialect::Sqlite
    }
}

/// `manifest` with `features` added to the zapline dependency (see
/// [`crate::features::add_features`]), or `None` when nothing changed (they
/// are all there already, or there is no zapline dependency to edit).
pub fn add_zapline_features(manifest: &str, features: &[&str]) -> Option<String> {
    crate::features::add_features(manifest, features)
        .ok()
        .filter(|patched| patched != manifest)
}

/// Turns on the zapline features generated code needs in the project's
/// manifest, or leaves a note saying how.
fn enable_zapline_features(
    project: &Project,
    features: &[&str],
    report: &mut Report,
) -> Result<()> {
    let path = project.root.join("Cargo.toml");
    let manifest =
        fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    if let Some(patched) = add_zapline_features(&manifest, features) {
        fs::write(&path, patched).with_context(|| format!("cannot write {}", path.display()))?;
        report.modified.push(PathBuf::from("Cargo.toml"));
    } else if features
        .iter()
        .any(|f| !manifest.contains(&format!("\"{f}\"")))
    {
        report.notes.push(format!(
            "the generated code needs zapline's {} feature(s); enable them in Cargo.toml",
            features
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(" and ")
        ));
    }
    Ok(())
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
    let mut fields = db_resource::parse_fields(fields)?;
    let html = project.root.join("templates/base.html").is_file();
    let mut report = Report::default();
    for field in &mut fields {
        let Some(parent) = field.parent.as_mut() else {
            continue;
        };
        if parent.names.plural == names.plural {
            bail!(
                "`{}` would make {} refer to itself, which the generator does not support; \
                 add the column with `zapline g migration` and write the query yourself",
                field.name,
                names.plural
            );
        }
        resolve_parent(project, parent, &mut report);
    }
    let settings = ResourceSettings::of(project);
    let plan = settings.plan(
        &names,
        &fields,
        // Regenerating (--force) rewrites the existing migration instead of adding a second one.
        crate::db::MigrationFiles::plan(&project.root, &format!("create_{}", names.plural))?,
    );
    let testable = plan.test_crate.is_some();
    let files = db_resource::files(&plan);

    let edit = plan_route_registration(project, &names.plural)?;
    if apply(project, files, edit, force, &mut report)? {
        report.notes.push(new_routes_module_note(project));
    }
    enable_zapline_features(project, &needed_features(&fields, true), &mut report)?;
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

/// What a SQL resource's code depends on besides its names and fields: the
/// same for `generate resource` and for `generate migration` regenerating it.
pub(crate) struct ResourceSettings {
    dialect: db_resource::Dialect,
    html: bool,
    flash: bool,
    test_crate: Option<String>,
}

impl ResourceSettings {
    /// Reads them from the project.
    pub(crate) fn of(project: &Project) -> Self {
        let manifest = fs::read_to_string(project.root.join("Cargo.toml")).unwrap_or_default();
        let lib = fs::read_to_string(project.root.join("src/lib.rs")).unwrap_or_default();
        let testable = project.lib_name.is_some() && lib.contains("pub fn app(db: Database)");
        ResourceSettings {
            dialect: dialect_of(&manifest),
            html: project.root.join("templates/base.html").is_file(),
            // Flash messages need sessions: the fullstack template and `g auth` add them.
            flash: auth::has_sessions(&lib) || project.root.join("src/routes/auth.rs").is_file(),
            test_crate: testable.then(|| crate_name_of(project)),
        }
    }

    /// The generation plan for `names` and `fields`.
    pub(crate) fn plan<'a>(
        &'a self,
        names: &'a ResourceNames,
        fields: &'a [db_resource::Field],
        migration: crate::db::MigrationFiles,
    ) -> db_resource::Plan<'a> {
        db_resource::Plan {
            names,
            fields,
            dialect: self.dialect,
            html: self.html,
            flash: self.flash,
            test_crate: self.test_crate.as_deref(),
            migration,
        }
    }
}

/// The zapline features code for `fields` needs: `chrono` for dates and
/// times (and for the `created_at`/`updated_at` of generated models), `uuid`.
fn needed_features(fields: &[db_resource::Field], timestamps: bool) -> Vec<&'static str> {
    use db_resource::FieldType;
    let mut out = Vec::new();
    if timestamps
        || fields
            .iter()
            .any(|f| matches!(f.ty, FieldType::Date | FieldType::DateTime))
    {
        out.push("chrono");
    }
    if fields.iter().any(|f| f.ty == FieldType::Uuid) {
        out.push("uuid");
    }
    out
}

/// The `(file name, SQL)` of every migration, in order.
fn migrations(project: &Project) -> Vec<(String, String)> {
    let dir = project.root.join(crate::db::MIGRATIONS_DIR);
    let mut out: Vec<(String, String)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
        .filter_map(|e| {
            let sql = fs::read_to_string(e.path()).ok()?;
            Some((e.file_name().to_string_lossy().into_owned(), sql))
        })
        .collect();
    out.sort();
    out
}

/// The migration that creates `table`, if any.
fn creating_migration(project: &Project, table: &str) -> Option<(String, String)> {
    let needle = format!("CREATE TABLE {}", table.to_ascii_uppercase());
    migrations(project).into_iter().find(|(_, sql)| {
        let upper = sql.to_ascii_uppercase();
        [" (", "(", "\n"]
            .iter()
            .any(|end| upper.contains(&format!("{needle}{end}")))
            || upper.contains(&format!(
                "CREATE TABLE IF NOT EXISTS {} ",
                table.to_ascii_uppercase()
            ))
    })
}

/// Fills in what the project knows about a `references` field's table: the
/// column to show in `<select>`s and whether it has pages to link to.
fn resolve_parent(project: &Project, parent: &mut db_resource::Parent, report: &mut Report) {
    let table = &parent.names.plural;
    match creating_migration(project, table) {
        Some((_, sql)) => parent.label = db_resource::label_column(&sql, table),
        None => report.notes.push(format!(
            "no migration creates the `{table}` table this resource refers to; generate it first, e.g.\n\
             \x20 zapline g resource {} name\n\
             then regenerate this resource with --force so its migration runs after the new one \
             (Postgres refuses a foreign key to a table that doesn't exist yet)",
            parent.names.singular
        )),
    }
    let module = project
        .root
        .join(format!("src/routes/{}.rs", parent.names.plural));
    parent.page = fs::read_to_string(module)
        .map(|src| {
            src.contains(&format!(
                "#[get(\"{}/{{id}}\", tag = \"pages\")]",
                parent.names.path
            ))
        })
        .unwrap_or(false);
}

/// What `zapline generate migration` writes for a name.
#[derive(Debug, PartialEq, Eq)]
enum MigrationKind {
    /// `add_<columns>_to_<table>`.
    AddColumns { table: String, columns: String },
    /// `remove_<columns>_from_<table>`.
    RemoveColumns { table: String, columns: String },
    /// `create_<table>`, or any other name with fields.
    CreateTable { table: String },
    /// Anything else: an empty migration to fill in.
    Blank,
}

fn migration_kind(slug: &str, has_fields: bool) -> MigrationKind {
    let split = |rest: &str, sep: &str| -> Option<(String, String)> {
        let (cols, table) = rest.rsplit_once(sep)?;
        (!cols.is_empty() && !table.is_empty()).then(|| (cols.to_string(), table.to_string()))
    };
    if let Some((columns, table)) = slug.strip_prefix("add_").and_then(|r| split(r, "_to_")) {
        return MigrationKind::AddColumns { table, columns };
    }
    if let Some((columns, table)) = slug
        .strip_prefix("remove_")
        .and_then(|r| split(r, "_from_"))
    {
        return MigrationKind::RemoveColumns { table, columns };
    }
    if let Some(table) = slug.strip_prefix("create_").filter(|t| !t.is_empty()) {
        return MigrationKind::CreateTable {
            table: table.to_string(),
        };
    }
    if has_fields {
        MigrationKind::CreateTable {
            table: slug.to_string(),
        }
    } else {
        MigrationKind::Blank
    }
}

/// `ALTER TABLE <table> ADD COLUMN` for each field. A required column gets
/// a default so rows already there stay valid: the one in `defaults` (from
/// `name:type=value`), else the type's zero value. One with no sensible
/// default is an error, or, when `lenient` (re-adding a removed column on
/// the way down), comes back optional.
fn add_columns_sql(
    table: &str,
    fields: &[db_resource::Field],
    defaults: &[(String, String)],
    dialect: db_resource::Dialect,
    lenient: bool,
) -> Result<String> {
    use db_resource::FieldType;
    let mut sql = String::new();
    for f in fields {
        let mut column = f.column_sql(dialect);
        let given = defaults.iter().find(|(name, _)| *name == f.name);
        let default = match (given, f.optional, f.ty) {
            (Some((_, literal)), _, FieldType::Bool) => {
                column = column.replace(" DEFAULT FALSE", "");
                format!(" DEFAULT {literal}")
            }
            (Some((_, literal)), _, _) => format!(" DEFAULT {literal}"),
            (None, true, _) | (None, false, FieldType::Bool) => String::new(),
            // Existing rows need a value for a required column.
            (None, false, FieldType::String | FieldType::Text) => " DEFAULT ''".into(),
            (None, false, FieldType::Int | FieldType::Float) => " DEFAULT 0".into(),
            (None, false, _) if lenient => {
                column = column.replace(" NOT NULL", "");
                String::new()
            }
            (None, false, _) => bail!(
                "`{}` can't be added as a required column: rows that already exist have no value for it\n\
                 hint: give them one (`{}:{}=<value>`), or add it as optional (`{}:{}?`), fill it in, \
                 then make it required in a later migration",
                f.name,
                f.parent.as_ref().map_or(f.name.as_str(), |p| p.names.singular.as_str()),
                f.ty.name(),
                f.parent.as_ref().map_or(f.name.as_str(), |p| p.names.singular.as_str()),
                f.ty.name()
            ),
        };
        sql.push_str(&format!(
            "ALTER TABLE {table} ADD COLUMN {column}{default};\n"
        ));
        if f.parent.is_some() {
            sql.push_str(&db_resource::index_sql(table, &f.name));
        }
    }
    Ok(sql)
}

/// Splits `views:int=0` into the field spec and the SQL literal of its
/// default (checked against the type); `None` without `=`.
fn split_default(spec: &str, dialect: db_resource::Dialect) -> Result<(String, Option<String>)> {
    use db_resource::FieldType;
    let Some((field_spec, raw)) = spec.split_once('=') else {
        return Ok((spec.to_string(), None));
    };
    let field = db_resource::Field::parse(field_spec)?;
    let value = raw.trim();
    let bad = |what: &str| anyhow::anyhow!("`{spec}`: `{raw}` is not {what}");
    let literal = match field.ty {
        FieldType::Int | FieldType::References => value
            .parse::<i64>()
            .map_err(|_| bad("an integer"))?
            .to_string(),
        FieldType::Float => {
            let v = value
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| bad("a number"))?;
            format!("{v:?}")
        }
        FieldType::Bool => match value.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => "TRUE".into(),
            "false" | "0" | "no" | "off" => "FALSE".into(),
            _ => return Err(bad("true or false")),
        },
        FieldType::Uuid if dialect == db_resource::Dialect::Sqlite => bail!(
            "`{spec}`: SQLite stores uuids as bytes, so a uuid column can't get a text default there\n\
             hint: add it as optional (`{}:uuid?`) and fill it in",
            field.name
        ),
        _ => format!("'{}'", raw.replace('\'', "''")),
    };
    Ok((field_spec.to_string(), Some(literal)))
}

/// `ALTER TABLE <table> DROP COLUMN` for each field, last first (and the
/// index of a `references` column before the column).
fn drop_columns_sql(table: &str, fields: &[db_resource::Field]) -> String {
    let mut sql = String::new();
    for f in fields.iter().rev() {
        if f.parent.is_some() {
            sql.push_str(&format!("DROP INDEX {table}_{}_idx;\n", f.name));
        }
        sql.push_str(&format!("ALTER TABLE {table} DROP COLUMN {};\n", f.name));
    }
    sql
}

/// The up and down SQL of a migration; `fields` are already parsed.
fn migration_sql(
    kind: &MigrationKind,
    fields: &[db_resource::Field],
    defaults: &[(String, String)],
    dialect: db_resource::Dialect,
    name: &str,
) -> Result<(String, String)> {
    Ok(match kind {
        MigrationKind::AddColumns { table, .. } => (
            add_columns_sql(table, fields, defaults, dialect, false)?,
            drop_columns_sql(table, fields),
        ),
        MigrationKind::RemoveColumns { table, .. } => {
            // Drop the index along with a removed `references` column.
            let mut up = String::new();
            for f in fields {
                if f.parent.is_some() {
                    up.push_str(&format!("DROP INDEX IF EXISTS {table}_{}_idx;\n", f.name));
                }
                up.push_str(&format!("ALTER TABLE {table} DROP COLUMN {};\n", f.name));
            }
            (
                up,
                format!(
                    "-- The columns come back empty (with their defaults).\n{}",
                    add_columns_sql(table, fields, &[], dialect, true)?
                ),
            )
        }
        MigrationKind::CreateTable { table } => (
            db_resource::create_table_sql(table, fields, dialect),
            format!("DROP TABLE {table};\n"),
        ),
        MigrationKind::Blank => {
            let (_, down) = crate::db::blank_sql(name, crate::db::Style::Reversible);
            (
                String::from("-- Write your SQL here.\n"),
                down.unwrap_or_default(),
            )
        }
    })
}

/// `zapline generate migration <name> [field:type[=default] ...]`:
/// `add_<col>_to_<table>` adds columns, `remove_<col>_from_<table>` drops
/// them, `create_<table>` (or any other name with fields) creates a table
/// like `generate resource` does; anything else is an empty migration.
///
/// Adding or removing columns of a table made by `generate resource` also
/// updates its module, pages and test (see [`evolve`]).
pub fn migration(dir: &Path, name: &str, fields: &[String]) -> Result<Report> {
    let project = Project::discover(dir)?;
    if !project.root.join(crate::db::MIGRATIONS_DIR).is_dir() {
        bail!(
            "this project has no migrations/ directory\n\
             hint: create one with `zapline db new init` (and enable zapline's `sqlite` or `postgres` feature), \
             or start from `zapline new <name> --template fullstack`"
        );
    }
    let manifest = fs::read_to_string(project.root.join("Cargo.toml")).unwrap_or_default();
    let dialect = dialect_of(&manifest);
    let slug = snake_case(name);
    let mut specs = Vec::new();
    let mut defaults = Vec::new();
    for spec in fields {
        let (spec, default) = split_default(spec, dialect)?;
        if let Some(literal) = default {
            defaults.push((db_resource::Field::parse(&spec)?.name, literal));
        }
        specs.push(spec);
    }
    let mut parsed = db_resource::parse_field_list(&specs)?;
    let kind = migration_kind(&slug, !parsed.is_empty());
    if !defaults.is_empty() && !matches!(kind, MigrationKind::AddColumns { .. }) {
        bail!("`name:type=value` defaults are for `add_<columns>_to_<table>` migrations");
    }
    // `add_email_to_users` alone adds a text column `email`.
    if let MigrationKind::AddColumns { columns, .. }
    | MigrationKind::RemoveColumns { columns, .. } = &kind
    {
        if parsed.is_empty() {
            parsed = vec![db_resource::Field::parse(columns)?];
        }
    }
    // A removed column of a generated resource keeps its real type (for
    // the down file): `remove_author_from_posts` drops `author_id BIGINT`.
    if let MigrationKind::RemoveColumns { table, .. } = &kind {
        if let Some(known) = evolve::recorded_fields(&project, table) {
            for f in &mut parsed {
                let id = format!("{}_id", f.name);
                if let Some(k) = known.iter().find(|k| k.name == f.name || k.name == id) {
                    *f = k.clone();
                }
            }
        }
    }
    let mut report = Report::default();
    for field in &mut parsed {
        if let Some(parent) = field.parent.as_mut() {
            resolve_parent(&project, parent, &mut report);
        }
    }
    let (up, down) = migration_sql(&kind, &parsed, &defaults, dialect, &slug)?;
    let up = match (&kind, dialect) {
        (MigrationKind::RemoveColumns { .. }, db_resource::Dialect::Sqlite) => format!(
            "-- DROP COLUMN needs SQLite 3.35 or newer (sqlx bundles one), and fails for a\n\
             -- column that is UNIQUE, a PRIMARY KEY or used by a view, trigger or CHECK.\n{up}"
        ),
        _ => up,
    };

    let header = format!("-- Created by `zapline generate migration {slug}`.\n");
    let down = match kind {
        MigrationKind::Blank => down,
        _ => format!("-- Reverts {slug}.\n{down}"),
    };
    let written =
        crate::db::create_migration(&project.root, &slug, &format!("{header}{up}"), Some(&down))?;
    let up_file = written
        .first()
        .map(|p| display(p.strip_prefix(&project.root).unwrap_or(p)))
        .unwrap_or_default();
    for path in written {
        report.created.push(
            path.strip_prefix(&project.root)
                .unwrap_or(&path)
                .to_path_buf(),
        );
    }

    match &kind {
        MigrationKind::AddColumns { table, .. } | MigrationKind::RemoveColumns { table, .. } => {
            if creating_migration(&project, table).is_none() {
                report.notes.push(format!(
                    "no migration creates the `{table}` table; check the name (tables are plural, like `posts`)"
                ));
            }
            if let MigrationKind::AddColumns { .. } = kind {
                for f in &parsed {
                    let implied = match f.ty {
                        _ if f.optional || defaults.iter().any(|(n, _)| *n == f.name) => None,
                        db_resource::FieldType::String | db_resource::FieldType::Text => Some("''"),
                        db_resource::FieldType::Int | db_resource::FieldType::Float => Some("0"),
                        _ => None,
                    };
                    if let Some(value) = implied {
                        report.notes.push(format!(
                            "`{}` is required, so existing rows get DEFAULT {value}; \
                             edit {up_file} before it runs, or pass `{}:{}=<value>`",
                            f.name,
                            f.name,
                            f.ty.name()
                        ));
                    }
                }
            }
            let change = match kind {
                MigrationKind::AddColumns { .. } => evolve::Change::Add(parsed.clone()),
                _ => evolve::Change::Remove(parsed.iter().map(|f| f.name.clone()).collect()),
            };
            let updated = evolve::update_resource(&project, table, &change, &mut report)?;
            let module = PathBuf::from(format!("src/routes/{table}.rs"));
            if !updated && project.root.join(&module).is_file() {
                let what = match kind {
                    MigrationKind::AddColumns { .. } => {
                        let lines: Vec<String> = parsed
                            .iter()
                            .map(|f| format!("\x20 pub {}: {},", f.name, f.rust_type()))
                            .collect();
                        format!(
                            "{} was not made by `zapline g resource` (it has no `{}` line), so it was not changed; \
                             add the new column(s) to its model (and its New/Update structs):\n{}\n\
                             and to the column lists of its queries and its `listing()`",
                            display(&module),
                            db_resource::RECIPE_MARKER,
                            lines.join("\n")
                        )
                    }
                    _ => format!(
                        "{} was not made by `zapline g resource` (it has no `{}` line), so it was not changed; \
                         take the removed column(s) out of its structs and queries",
                        display(&module),
                        db_resource::RECIPE_MARKER
                    ),
                };
                report.notes.push(what);
            }
        }
        MigrationKind::CreateTable { .. } | MigrationKind::Blank => {}
    }
    if !matches!(kind, MigrationKind::RemoveColumns { .. }) {
        // What Rust code reading these columns will need.
        let features = needed_features(&parsed, false);
        if !features.is_empty() {
            enable_zapline_features(&project, &features, &mut report)?;
        }
    }
    report.notes.push(
        "the migration runs when the app starts (or run `zapline db migrate`); \
         `zapline db status` lists what is applied, `zapline db rollback` runs its .down.sql file"
            .to_string(),
    );
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
    // Document statuses other than 200 so the OpenAPI spec (and the fuzzer)
    // expect what the handler answers.
    let route_status = match shape.status {
        "200" => String::new(),
        code => format!(", status = {code}"),
    };
    let vars = [
        ("name", module),
        ("method", method_lower),
        ("METHOD", method_upper.as_str()),
        ("path", path),
        ("args", shape.args.as_str()),
        ("ret", shape.ret.as_str()),
        ("body", shape.body.as_str()),
        ("status", shape.status),
        ("route_status", route_status.as_str()),
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
