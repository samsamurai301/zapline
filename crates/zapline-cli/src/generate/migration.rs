//! `zapline generate migration <name> [field:type[=default] ...]`, also
//! spelled `zapline db new`: schema changes for an existing table.
//!
//! The migration's name says what it does:
//!
//! - `add_<columns>_to_<table> views:int=0 ...` writes one `ALTER TABLE ..
//!   ADD COLUMN` per field, and
//! - `remove_<column>_from_<table> [column ...]` one `ALTER TABLE .. DROP
//!   COLUMN` per column;
//! - any other name gives an empty migration to fill in.
//!
//! When `src/routes/<table>.rs` was made by `zapline generate resource`, the
//! module, its htmx page and its test are updated too: the model structs,
//! every SELECT / RETURNING column list, the INSERT and UPDATE statements and
//! their binds, the list endpoint's sort/search/filter columns, the form and
//! the test's sample body. The struct, form and sample anchors are the
//! [`FIELDS_MARKER`] / [`TEMPLATE_MARKER`] comments; the SQL is found by its
//! shape. A file that no longer has that shape is left alone and the exact
//! lines to change are printed instead, so a half-patched module (whose
//! queries no longer match its struct) is never written.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::db_resource::{input, Dialect, Field, FieldType, FIELDS_MARKER, TEMPLATE_MARKER};
use super::{dialect_of, format_files, Report};
use crate::db::{create_migration, MIGRATIONS_DIR};
use crate::naming::snake_case;
use crate::project::Project;

/// A column to add: its field and the `DEFAULT` existing rows get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Name, type and nullability.
    pub field: Field,
    /// The SQL literal after `DEFAULT`, if any.
    pub default: Option<String>,
    /// Whether `default` is the type's zero value, picked because a
    /// required column needs one (a warning says so).
    pub implied_default: bool,
}

impl Column {
    /// Parses `views:int`, `views:int=0`, `title=Untitled`, `bio:text?`.
    ///
    /// A required column gets a default, since existing rows need a value
    /// (and SQLite refuses `NOT NULL` without one): the given `=value`, or
    /// the type's zero value. Booleans default to `FALSE`, as in `CREATE
    /// TABLE`; optional columns to NULL.
    pub fn parse(spec: &str) -> Result<Column> {
        let (spec, raw) = match spec.split_once('=') {
            Some((s, raw)) => (s, Some(raw)),
            None => (spec, None),
        };
        let field = Field::parse(spec)?;
        if let Some(raw) = raw {
            let literal = default_literal(field.ty, raw)
                .with_context(|| format!("`{spec}={raw}`: bad default"))?;
            return Ok(Column {
                field,
                default: Some(literal),
                implied_default: false,
            });
        }
        let (default, implied_default) = match (field.optional, field.ty) {
            (true, _) => (None, false),
            (false, FieldType::Bool) => (Some("FALSE".to_string()), false),
            (false, ty) => (Some(ty.zero_default().to_string()), true),
        };
        Ok(Column {
            field,
            default,
            implied_default,
        })
    }

    /// `views BIGINT NOT NULL DEFAULT 0`.
    fn definition(&self) -> String {
        let f = &self.field;
        let null = if f.optional { "" } else { " NOT NULL" };
        let default = self
            .default
            .as_ref()
            .map_or(String::new(), |d| format!(" DEFAULT {d}"));
        format!("{} {}{null}{default}", f.name, f.ty.sql())
    }
}

/// The SQL literal for `raw` as a value of `ty`.
fn default_literal(ty: FieldType, raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    Ok(match ty {
        FieldType::Int => trimmed
            .parse::<i64>()
            .with_context(|| format!("`{raw}` is not an integer"))?
            .to_string(),
        FieldType::Float => {
            let v = trimmed
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .with_context(|| format!("`{raw}` is not a number"))?;
            format!("{v:?}")
        }
        FieldType::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => "TRUE".to_string(),
            "false" | "0" | "no" | "off" => "FALSE".to_string(),
            _ => bail!("`{raw}` is not true or false"),
        },
        FieldType::String | FieldType::Text => format!("'{}'", raw.replace('\'', "''")),
    })
}

/// What a migration name and its field arguments ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// `add_<x>_to_<table> field:type...`.
    Add {
        /// The table to alter.
        table: String,
        /// The columns to add, in order.
        columns: Vec<Column>,
    },
    /// `remove_<x>_from_<table> [column...]`.
    Remove {
        /// The table to alter.
        table: String,
        /// The columns to drop.
        columns: Vec<String>,
    },
    /// Any other name: an empty migration.
    Empty,
}

impl Change {
    /// Reads `name` (any case style) and the field arguments.
    ///
    /// `add_views_to_posts views:int` adds to `posts`; the part before
    /// `_to_` is only a description, the fields say what to add (without
    /// fields it is an empty migration). `remove_views_from_posts` drops
    /// `views`, or the columns given as arguments.
    pub fn parse(name: &str, fields: &[String]) -> Result<Change> {
        let slug = snake_case(name);
        if let Some((_, table)) = split_intent(&slug, "add_", "_to_") {
            if fields.is_empty() {
                return Ok(Change::Empty);
            }
            let mut columns: Vec<Column> = Vec::new();
            for spec in fields {
                let column = Column::parse(spec)?;
                if columns.iter().any(|c| c.field.name == column.field.name) {
                    bail!("field `{}` is listed twice", column.field.name);
                }
                columns.push(column);
            }
            return Ok(Change::Add { table, columns });
        }
        if let Some((what, table)) = split_intent(&slug, "remove_", "_from_") {
            let specs: Vec<&str> = if fields.is_empty() {
                vec![what.as_str()]
            } else {
                fields.iter().map(String::as_str).collect()
            };
            let mut columns: Vec<String> = Vec::new();
            for spec in specs {
                // Only the name matters; `views:int` is accepted too.
                let spec = spec.split_once('=').map_or(spec, |(s, _)| s);
                let name = Field::parse(spec)
                    .with_context(|| format!("`{spec}` is not a column name"))?
                    .name;
                if !columns.contains(&name) {
                    columns.push(name);
                }
            }
            return Ok(Change::Remove { table, columns });
        }
        if let Some(first) = fields.first() {
            bail!(
                "fields like `{first}` need a name that says where they go: \
                 `add_<columns>_to_<table>` or `remove_<columns>_from_<table>`"
            );
        }
        Ok(Change::Empty)
    }

    /// The migration's SQL in `dialect`; `command` is quoted in its header.
    pub fn sql(&self, dialect: Dialect, command: &str) -> String {
        let mut s = format!("-- Created by `zapline generate migration {command}`.\n");
        match self {
            Change::Add { table, columns } => {
                for c in columns {
                    if c.implied_default {
                        s.push_str(&format!(
                            "-- Existing rows get {} for {}; new rows must set it.\n",
                            c.default.as_deref().unwrap_or("NULL"),
                            c.field.name
                        ));
                    }
                    s.push_str(&format!(
                        "ALTER TABLE {table} ADD COLUMN {};\n",
                        c.definition()
                    ));
                }
            }
            Change::Remove { table, columns } => {
                if dialect == Dialect::Sqlite {
                    s.push_str(
                        "-- DROP COLUMN needs SQLite 3.35 or newer (sqlx bundles one), and fails\n\
                         -- for a column that is indexed, UNIQUE, a PRIMARY or FOREIGN KEY, or used\n\
                         -- by a view, trigger or CHECK: drop those first.\n",
                    );
                }
                for c in columns {
                    s.push_str(&format!("ALTER TABLE {table} DROP COLUMN {c};\n"));
                }
            }
            Change::Empty => {
                s.push_str(
                    "-- Applied once, in file-name order, by `zapline db migrate` or at app startup.\n\n",
                );
            }
        }
        s
    }
}

/// `add_views_to_posts` with ("add_", "_to_") is `("views", "posts")`.
fn split_intent(slug: &str, prefix: &str, sep: &str) -> Option<(String, String)> {
    let (what, table) = slug.strip_prefix(prefix)?.rsplit_once(sep)?;
    let valid = |s: &str| {
        s.starts_with(|c: char| c.is_ascii_lowercase())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    (valid(what) && valid(table)).then(|| (what.to_string(), table.to_string()))
}

/// `zapline generate migration <name> [field:type ...]`.
pub fn generate(project: &Project, name: &str, fields: &[String]) -> Result<Report> {
    let change = Change::parse(name, fields)?;
    let dialect = dialect_of(project);
    let command = std::iter::once(name)
        .chain(fields.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let mut report = Report::default();

    let table = match &change {
        Change::Add { table, .. } | Change::Remove { table, .. } => Some(table.clone()),
        Change::Empty => None,
    };
    if let Some(table) = &table {
        if !table_is_created(&project.root, table) {
            report.warnings.push(format!(
                "no migration in {MIGRATIONS_DIR}/ creates a `{table}` table; check the name \
                 (the part after `_to_` / `_from_` is the table)"
            ));
        }
    }

    let path = create_migration(&project.root, name, &change.sql(dialect, &command))?;
    let rel = path
        .strip_prefix(&project.root)
        .unwrap_or(&path)
        .to_path_buf();
    let shown = rel.to_string_lossy().replace('\\', "/");
    report.created.push(rel);

    match &change {
        Change::Empty => {
            if split_intent(&snake_case(name), "add_", "_to_").is_some() {
                report.notes.push(format!(
                    "the migration is empty: list the columns to have it written for you, e.g.\n\
                     \x20 zapline g migration {name} views:int=0 summary:text?"
                ));
            }
        }
        Change::Add { columns, .. } => {
            for c in columns.iter().filter(|c| c.implied_default) {
                let f = &c.field;
                let mut msg = format!(
                    "`{}` is required, so existing rows get DEFAULT {} (edit it in {shown} \
                     before it runs, or next time pass `{}:{}=<value>`)",
                    f.name,
                    c.default.as_deref().unwrap_or("NULL"),
                    f.name,
                    type_word(f.ty)
                );
                if f.ty == FieldType::String {
                    msg.push_str(
                        " (the API rejects an empty string, so update those rows before editing them)",
                    );
                }
                report.warnings.push(msg);
            }
        }
        Change::Remove { .. } => {}
    }

    if let Some(table) = &table {
        update_resource(project, table, &change, &mut report)?;
        report.notes.push(
            "the migration runs when the app starts (or with `zapline db migrate`); \
             `zapline db status` lists what is applied"
                .to_string(),
        );
    }
    Ok(report)
}

/// The command-line word for `ty`.
fn type_word(ty: FieldType) -> &'static str {
    match ty {
        FieldType::String => "string",
        FieldType::Text => "text",
        FieldType::Int => "int",
        FieldType::Float => "float",
        FieldType::Bool => "bool",
    }
}

/// Whether some migration has `CREATE TABLE [IF NOT EXISTS] <table>`.
fn table_is_created(root: &Path, table: &str) -> bool {
    let Ok(entries) = fs::read_dir(root.join(MIGRATIONS_DIR)) else {
        return false;
    };
    entries.flatten().any(|e| {
        let sql = fs::read_to_string(e.path())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let words: Vec<&str> = sql
            .split(|c: char| c.is_whitespace() || c == '(' || c == ';' || c == '"')
            .filter(|w| !w.is_empty())
            .collect();
        words.windows(2).enumerate().any(|(i, w)| {
            if w != ["create", "table"] {
                return false;
            }
            let rest = &words[i + 2..];
            let name = match rest {
                ["if", "not", "exists", name, ..] => name,
                [name, ..] => name,
                [] => return false,
            };
            *name == table
        })
    })
}

// ---- updating the generated resource ---------------------------------------------

/// Patches `src/routes/<table>.rs`, `templates/<table>/index.html` and
/// `tests/<table>.rs` when they exist; each file is all or nothing.
fn update_resource(
    project: &Project,
    table: &str,
    change: &Change,
    report: &mut Report,
) -> Result<()> {
    let module = PathBuf::from(format!("src/routes/{table}.rs"));
    if !project.root.join(&module).is_file() {
        report.notes.push(format!(
            "no {} (from `zapline g resource`), so no Rust code was changed; \
             update the queries and structs that use `{table}` yourself",
            module.display()
        ));
        return Ok(());
    }
    let mut formatted: Vec<PathBuf> = Vec::new();
    let files: [(PathBuf, Patcher, Snippets); 3] = [
        (module, patch_module, module_snippets),
        (
            PathBuf::from(format!("templates/{table}/index.html")),
            patch_template,
            template_snippets,
        ),
        (
            PathBuf::from(format!("tests/{table}.rs")),
            patch_test,
            test_snippets,
        ),
    ];
    for (rel, patch, snippets) in files {
        let path = project.root.join(&rel);
        if !path.is_file() {
            continue;
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        match patch(&text, table, change) {
            Ok(patched) => {
                if patched.notes.is_empty() && patched.text == text {
                    continue;
                }
                report.notes.extend(
                    patched
                        .notes
                        .into_iter()
                        .map(|n| format!("{}: {n}", rel.display())),
                );
                if patched.text != text {
                    fs::write(&path, &patched.text)
                        .with_context(|| format!("cannot write {}", path.display()))?;
                    if rel.extension().is_some_and(|e| e == "rs") {
                        formatted.push(rel.clone());
                    }
                    report.modified.push(rel);
                }
            }
            Err(why) => report.notes.push(format!(
                "{} was not changed ({why}); make these changes yourself:\n{}",
                rel.display(),
                snippets(table, change)
            )),
        }
    }
    format_files(project, &formatted);
    Ok(())
}

/// A patched file and what the user should still look at.
#[derive(Debug)]
struct Patched {
    text: String,
    notes: Vec<String>,
}

impl From<String> for Patched {
    fn from(text: String) -> Self {
        Patched {
            text,
            notes: Vec::new(),
        }
    }
}

/// Why a file could not be patched.
type Why = String;
type Patcher = fn(&str, &str, &Change) -> std::result::Result<Patched, Why>;
type Snippets = fn(&str, &Change) -> String;

// ---- the module ------------------------------------------------------------------

/// Applies `change` to a module made by `zapline generate resource`.
fn patch_module(text: &str, table: &str, change: &Change) -> std::result::Result<Patched, Why> {
    let model = model_name(text).ok_or("no `#[derive(sqlx::FromRow)]` model struct")?;
    let payload = format!("New{model}");
    if !struct_body(text, &model).is_some_and(|(a, b)| text[a..b].contains(FIELDS_MARKER)) {
        return Err(format!(
            "`{model}` has no `{FIELDS_MARKER}` marker, which `zapline g resource` writes"
        ));
    }
    let mut s = text.to_string();
    let (old_select, _) = listing_columns(&s, table).ok_or_else(|| {
        format!("no `Listing::new(\"{table}\", \"<columns>\")` to read the column list from")
    })?;
    let mut cols: Vec<String> = split_list(&old_select);

    match change {
        Change::Add { columns, .. } => {
            for c in columns {
                if cols.contains(&c.field.name) {
                    return Err(format!("it already selects `{}`", c.field.name));
                }
            }
            let model_lines: String = columns.iter().map(|c| c.field.model_line()).collect();
            let payload_lines: String = columns.iter().map(|c| c.field.payload_lines()).collect();
            s = insert_in_struct(&s, &model, &model_lines)?;
            s = insert_in_struct(&s, &payload, &payload_lines)?;
            cols.extend(columns.iter().map(|c| c.field.name.clone()));
        }
        Change::Remove { columns, .. } => {
            for c in columns {
                if !cols.contains(c) {
                    return Err(format!("it does not select `{c}`"));
                }
            }
            cols.retain(|c| !columns.contains(c));
            if cols.iter().all(|c| c == "id") {
                return Err("that would remove every column besides `id`".to_string());
            }
            for c in columns {
                s = remove_from_struct(&s, &model, c)?;
                s = remove_from_struct(&s, &payload, c)?;
            }
        }
        Change::Empty => return Ok(s.into()),
    }

    // Every place that lists the columns read back into the model.
    let new_select = cols.join(", ");
    let (_, at) = listing_columns(&s, table).ok_or("the Listing column list moved")?;
    s.replace_range(at..at + old_select.len(), &new_select);
    s = s.replace(
        &format!("RETURNING {old_select}\""),
        &format!("RETURNING {new_select}\""),
    );
    s = s.replace(
        &format!("\"SELECT {old_select} FROM {table} "),
        &format!("\"SELECT {new_select} FROM {table} "),
    );

    // INSERT and UPDATE, and the binds that go with them.
    let (insert_cols, _) = parse_insert(&s, table)?;
    let (sets, _) = parse_update(&s, table)?;
    if insert_cols != sets {
        return Err("its INSERT and UPDATE list different columns".to_string());
    }
    let mut written = insert_cols.clone();
    match change {
        Change::Add { columns, .. } => {
            let last = written
                .last()
                .cloned()
                .ok_or("its INSERT lists no columns")?;
            let binds: String = columns
                .iter()
                .map(|c| format!("\n    {}", c.field.bind()))
                .collect();
            s = insert_after_binds(&s, &last, &binds)?;
            written.extend(columns.iter().map(|c| c.field.name.clone()));
        }
        Change::Remove { columns, .. } => {
            for c in columns {
                s = remove_binds(&s, c)?;
            }
            written.retain(|c| !columns.contains(c));
        }
        Change::Empty => {}
    }
    s = rewrite_insert(&s, table, &written)?;
    s = rewrite_update(&s, table, &written)?;

    // The list endpoint's sort, search and filter columns, and its doc.
    s = patch_listing(&s, change)?;
    Ok(s.into())
}

/// The name of the struct after `#[derive(sqlx::FromRow)]`.
fn model_name(text: &str) -> Option<String> {
    let at = text.find("#[derive(sqlx::FromRow)]")?;
    let rest = &text[at..];
    let start = rest.find("pub struct ")? + "pub struct ".len();
    let name: String = rest[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The byte range of `pub struct <name> { .. }`'s body (after `{`, up to
/// the closing `}`).
fn struct_body(text: &str, name: &str) -> Option<(usize, usize)> {
    let head = format!("pub struct {name} {{");
    let start = text.find(&head)? + head.len();
    let end = start + text[start..].find("\n}")?;
    Some((start, end + 1))
}

/// Inserts `lines` above the [`FIELDS_MARKER`] in struct `name`.
fn insert_in_struct(text: &str, name: &str, lines: &str) -> std::result::Result<String, Why> {
    let (start, end) = struct_body(text, name).ok_or(format!("no `pub struct {name}`"))?;
    let body = &text[start..end];
    let marker = body
        .find(FIELDS_MARKER)
        .ok_or(format!("`{name}` has no `{FIELDS_MARKER}` marker"))?;
    let line_start = start + body[..marker].rfind('\n').map_or(0, |i| i + 1);
    let mut out = text.to_string();
    out.insert_str(line_start, lines);
    Ok(out)
}

/// Removes the `pub <field>:` line, and the attributes above it, from
/// struct `name`.
fn remove_from_struct(text: &str, name: &str, field: &str) -> std::result::Result<String, Why> {
    let (start, end) = struct_body(text, name).ok_or(format!("no `pub struct {name}`"))?;
    let body = &text[start..end];
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let at = lines
        .iter()
        .position(|l| l.trim_start().starts_with(&format!("pub {field}:")))
        .ok_or(format!("`{name}` has no field `{field}`"))?;
    let mut from = at;
    while from > 0 && lines[from - 1].trim_start().starts_with("#[") {
        from -= 1;
    }
    let mut kept: String = lines[..from].concat();
    kept.push_str(&lines[at + 1..].concat());
    Ok(format!("{}{kept}{}", &text[..start], &text[end..]))
}

/// The second argument of `Listing::new("<table>", "<columns>")` (on one
/// line or, after rustfmt, several), and where its text starts.
fn listing_columns(text: &str, table: &str) -> Option<(String, usize)> {
    let at = text.find("Listing::new(")? + "Listing::new(".len();
    let rest = text[at..].trim_start();
    let rest = rest.strip_prefix(&format!("\"{table}\""))?.trim_start();
    let rest = rest.strip_prefix(',')?.trim_start().strip_prefix('"')?;
    let start = text.len() - rest.len();
    Some((rest[..rest.find('"')?].to_string(), start))
}

/// `"a, b"` is `["a", "b"]`; also strips the quotes of `"a", "b"`.
fn split_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The text between `start` and the next `end` after it, with its range.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<(&'a str, usize, usize)> {
    let from = text.find(start)? + start.len();
    let to = from + text[from..].find(end)?;
    Some((&text[from..to], from, to))
}

/// The columns of `INSERT INTO <table> (..) VALUES ($1, ..)`, checking that
/// the placeholders are `$1..$n`.
fn parse_insert(text: &str, table: &str) -> std::result::Result<(Vec<String>, usize), Why> {
    let head = format!("\"INSERT INTO {table} (");
    let (cols, _, end) =
        between(text, &head, ")").ok_or(format!("no `INSERT INTO {table} (..)` statement"))?;
    let cols = split_list(cols);
    let values = text[end..]
        .strip_prefix(") VALUES (")
        .and_then(|v| v.find(')').map(|i| &v[..i]))
        .ok_or("its INSERT has no `VALUES (..)`")?;
    let expected: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
    if split_list(values) != expected {
        return Err("its INSERT values are not just `$1, $2, ..` placeholders".to_string());
    }
    Ok((cols, end))
}

/// The columns of `UPDATE <table> SET a = $1, .. WHERE id = $n`, checking the
/// numbering.
fn parse_update(text: &str, table: &str) -> std::result::Result<(Vec<String>, usize), Why> {
    let head = format!("\"UPDATE {table} SET ");
    let (sets, _, end) = between(text, &head, " WHERE id = $").ok_or(format!(
        "no `UPDATE {table} SET .. WHERE id = $n` statement"
    ))?;
    let mut cols = Vec::new();
    for (i, set) in sets.split(',').enumerate() {
        let (col, ph) = set
            .split_once('=')
            .ok_or("its UPDATE has an unexpected SET")?;
        if ph.trim() != format!("${}", i + 1) {
            return Err("its UPDATE placeholders are not numbered `$1, $2, ..`".to_string());
        }
        cols.push(col.trim().to_string());
    }
    Ok((cols, end))
}

/// Rewrites the INSERT for `cols`.
fn rewrite_insert(text: &str, table: &str, cols: &[String]) -> std::result::Result<String, Why> {
    let head = format!("\"INSERT INTO {table} (");
    let (_, from, _) = between(text, &head, ")").ok_or("no INSERT")?;
    let values_end = from
        + text[from..]
            .find(") RETURNING")
            .ok_or("no INSERT .. RETURNING")?;
    let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
    Ok(format!(
        "{}{}) VALUES ({}{}",
        &text[..from],
        cols.join(", "),
        placeholders.join(", "),
        &text[values_end..]
    ))
}

/// Rewrites the UPDATE's SET list and id placeholder for `cols`.
fn rewrite_update(text: &str, table: &str, cols: &[String]) -> std::result::Result<String, Why> {
    let head = format!("\"UPDATE {table} SET ");
    let (_, from, to) = between(text, &head, " WHERE id = $").ok_or("no UPDATE")?;
    let after = to + " WHERE id = $".len();
    let digits = text[after..]
        .find(|c: char| !c.is_ascii_digit())
        .ok_or("no UPDATE id placeholder")?;
    let sets: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c} = ${}", i + 1))
        .collect();
    Ok(format!(
        "{}{} WHERE id = ${}{}",
        &text[..from],
        sets.join(", "),
        cols.len() + 1,
        &text[after + digits..]
    ))
}

/// Inserts `binds` after every `.bind(&new.<last>)` / `.bind(new.<last>)`.
fn insert_after_binds(text: &str, last: &str, binds: &str) -> std::result::Result<String, Why> {
    let mut out = text.to_string();
    let mut found = 0;
    for pattern in [format!(".bind(&new.{last})"), format!(".bind(new.{last})")] {
        found += out.matches(&pattern).count();
        out = out.replace(&pattern, &format!("{pattern}{binds}"));
    }
    if found == 0 {
        return Err(format!("no `.bind(new.{last})` after its INSERT/UPDATE"));
    }
    Ok(out)
}

/// Removes every `.bind(&new.<field>)` / `.bind(new.<field>)`, with the line
/// it was on when nothing else is left there.
fn remove_binds(text: &str, field: &str) -> std::result::Result<String, Why> {
    let patterns = [
        format!(".bind(&new.{field})"),
        format!(".bind(new.{field})"),
    ];
    if !patterns.iter().any(|p| text.contains(p.as_str())) {
        return Err(format!("no `.bind(new.{field})` to remove"));
    }
    Ok(remove_everywhere(text, &patterns))
}

/// Removes each occurrence of `patterns`; a line left empty goes too.
fn remove_everywhere(text: &str, patterns: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let mut l = line.to_string();
        for p in patterns {
            l = l.replace(p.as_str(), "");
        }
        if l.trim().is_empty() && !line.trim().is_empty() {
            continue;
        }
        out.push_str(&l);
    }
    out
}

/// Updates `fn listing()`: its `.sortable([..])`, `.search([..])` and
/// `.filter_*(..)` calls, then the doc comment above it.
fn patch_listing(text: &str, change: &Change) -> std::result::Result<String, Why> {
    let head = "fn listing() -> Listing {";
    let start = text.find(head).ok_or("no `fn listing()`")? + head.len();
    let end = start
        + text[start..]
            .find("\n}")
            .ok_or("no end of `fn listing()`")?;
    let mut body = text[start..end].to_string();

    match change {
        Change::Add { columns, .. } => {
            for c in columns {
                let f = &c.field;
                let quoted = format!("\"{}\"", f.name);
                if f.sortable() {
                    body = edit_list_call(&body, ".sortable([", |l| l.push(quoted.clone()))
                        .ok_or("no `.sortable([..])` in `fn listing()`")?;
                }
                if f.searchable() {
                    body = match edit_list_call(&body, ".search([", |l| l.push(quoted.clone())) {
                        Some(b) => b,
                        None => {
                            let at = list_call_end(&body, ".sortable([")
                                .ok_or("no `.sortable([..])` in `fn listing()`")?;
                            format!(
                                "{}\n        .search([{quoted}]){}",
                                &body[..at],
                                &body[at..]
                            )
                        }
                    };
                }
                if let Some(m) = f.filter_method() {
                    body.push_str(&format!("\n        .{m}(\"{}\")", f.name));
                }
            }
        }
        Change::Remove { columns, .. } => {
            for c in columns {
                let quoted = format!("\"{c}\"");
                if let Some(b) =
                    edit_list_call(&body, ".sortable([", |l| l.retain(|x| *x != quoted))
                {
                    body = b;
                }
                if let Some(b) = edit_list_call(&body, ".search([", |l| l.retain(|x| *x != quoted))
                {
                    body = b;
                }
                body = remove_everywhere(&body, &[".search([])".to_string()]);
                let filters: Vec<String> = ["filter_int", "filter_bool", "filter_text"]
                    .iter()
                    .map(|m| format!(".{m}(\"{c}\")"))
                    .collect();
                body = remove_everywhere(&body, &filters);
            }
        }
        Change::Empty => {}
    }
    let mut out = format!("{}{body}{}", &text[..start], &text[end..]);
    out = rewrite_listing_doc(&out, &body);
    Ok(out)
}

/// Where `call` (e.g. `.sortable([`) ends, just after its `])`.
fn list_call_end(body: &str, call: &str) -> Option<usize> {
    let from = body.find(call)? + call.len();
    Some(from + body[from..].find("])")? + 2)
}

/// Rewrites the list inside `call([..])` with `edit`; `None` without one.
fn edit_list_call(body: &str, call: &str, edit: impl FnOnce(&mut Vec<String>)) -> Option<String> {
    let from = body.find(call)? + call.len();
    let to = from + body[from..].find("])")?;
    let mut items: Vec<String> = body[from..to]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    edit(&mut items);
    Some(format!(
        "{}{}{}",
        &body[..from],
        items.join(", "),
        &body[to..]
    ))
}

/// Regenerates "/// `GET <path>` accepts .." above `fn listing()` from the
/// calls in its (updated) `body`. Left alone when it is not there.
fn rewrite_listing_doc(text: &str, body: &str) -> String {
    let Some(fn_at) = text.find("fn listing() -> Listing {") else {
        return text.to_string();
    };
    let line_start = text[..fn_at.saturating_sub(1)]
        .rfind('\n')
        .map_or(0, |i| i + 1);
    let line = &text[line_start..fn_at];
    let Some(path_end) = line.find(" accepts ") else {
        return text.to_string();
    };
    if !line.starts_with("/// `GET ") {
        return text.to_string();
    }
    let mut accepts = vec!["`?limit=&offset=`".to_string(), "`?sort=-id`".to_string()];
    let searched = body
        .find(".search([")
        .and_then(|from| {
            let from = from + ".search([".len();
            body[from..]
                .find("])")
                .map(|to| split_list(&body[from..from + to]))
        })
        .unwrap_or_default();
    if !searched.is_empty() {
        accepts.push(format!("`?q=` (searches {})", searched.join(", ")));
    }
    let mut rest = body;
    while let Some(at) = rest.find(".filter_") {
        let call = &rest[at..];
        if let Some((name, _, _)) = between(call, "(\"", "\")") {
            accepts.push(format!("`?{name}=`"));
        }
        rest = &rest[at + 1..];
    }
    format!(
        "{}{} accepts {}.\n{}",
        &text[..line_start],
        &line[..path_end],
        accepts.join(", "),
        &text[fn_at..]
    )
}

fn module_snippets(table: &str, change: &Change) -> String {
    match change {
        Change::Add { columns, .. } => {
            let mut s = String::from("  in the model struct and in its New.. payload struct, add:\n");
            for c in columns {
                for line in c.field.payload_lines().lines() {
                    s.push_str(&format!("  {line}\n"));
                }
            }
            let names: Vec<&str> = columns.iter().map(|c| c.field.name.as_str()).collect();
            s.push_str(&format!(
                "  add `{}` to every SELECT / RETURNING column list on `{table}` and to the INSERT\n\
                 \x20 and UPDATE statements (with `$n` placeholders), and bind the values after the\n\
                 \x20 existing ones:\n",
                names.join(", ")
            ));
            for c in columns {
                s.push_str(&format!("      {}\n", c.field.bind()));
            }
            s.trim_end().to_string()
        }
        Change::Remove { columns, .. } => format!(
            "  remove `{}` from the model and payload structs, from every SELECT / RETURNING\n\
             \x20 column list on `{table}`, from the INSERT and UPDATE statements (renumbering the\n\
             \x20 `$n` placeholders) and their `.bind(..)` calls",
            columns.join("`, `")
        ),
        Change::Empty => String::new(),
    }
}

// ---- the htmx page ----------------------------------------------------------------

/// Where the generated page's table row starts its delete button.
const ROW_END: &str = "<td><button class=\"link\" hx-delete=";
/// The generated page's last header cell.
const HEAD_END: &str = "<th></th></tr></thead>";

fn patch_template(text: &str, _table: &str, change: &Change) -> std::result::Result<Patched, Why> {
    let mut s = text.to_string();
    match change {
        Change::Add { columns, .. } => {
            let marker = s
                .find(TEMPLATE_MARKER)
                .ok_or(format!("no `{TEMPLATE_MARKER}` marker in the form"))?;
            let line_start = s[..marker].rfind('\n').map_or(0, |i| i + 1);
            let indent = s[line_start..marker].to_string();
            let inputs: String = columns
                .iter()
                .map(|c| format!("{indent}{}\n", input(&c.field)))
                .collect();
            s.insert_str(line_start, &inputs);
            let heads: String = columns
                .iter()
                .map(|c| format!("<th>{}</th>", c.field.label()))
                .collect();
            s = replace_first(&s, HEAD_END, &format!("{heads}{HEAD_END}"))
                .ok_or("no table header row")?;
            let cells: String = columns.iter().map(|c| c.field.cell()).collect();
            s = replace_first(&s, ROW_END, &format!("{cells}{ROW_END}"))
                .ok_or("no table row with a delete button")?;
            s = bump_colspan(&s, columns.len() as i64).ok_or("no `colspan` on the empty row")?;
        }
        Change::Remove { columns, .. } => {
            for c in columns {
                let label = Field::parse(c).map(|f| f.label()).unwrap_or_default();
                let before = s.clone();
                s = s
                    .split_inclusive('\n')
                    .filter(|l| !l.contains(&format!("name=\"{c}\"")))
                    .collect();
                s = replace_first(&s, &format!("<th>{label}</th>"), "").unwrap_or(s);
                for cell in [
                    format!("<td>{{{{ '✓' if item.{c} }}}}</td>"),
                    format!("<td>{{{{ item.{c} if item.{c} is not none }}}}</td>"),
                    format!("<td>{{{{ item.{c} }}}}</td>"),
                ] {
                    s = s.replace(&cell, "");
                }
                if s == before {
                    return Err(format!("it does not show `{c}`"));
                }
                s = bump_colspan(&s, -1).unwrap_or(s);
            }
        }
        Change::Empty => {}
    }
    Ok(s.into())
}

/// Replaces the first `from`.
fn replace_first(text: &str, from: &str, to: &str) -> Option<String> {
    let at = text.find(from)?;
    Some(format!("{}{to}{}", &text[..at], &text[at + from.len()..]))
}

/// Adds `by` to the first `colspan="n"`.
fn bump_colspan(text: &str, by: i64) -> Option<String> {
    let (n, from, to) = between(text, "colspan=\"", "\"")?;
    let n: i64 = n.parse().ok()?;
    Some(format!(
        "{}{}{}",
        &text[..from],
        (n + by).max(1),
        &text[to..]
    ))
}

fn template_snippets(_table: &str, change: &Change) -> String {
    match change {
        Change::Add { columns, .. } => {
            let mut s = String::from("  in the form:\n");
            for c in columns {
                s.push_str(&format!("    {}\n", input(&c.field)));
            }
            s.push_str("  in the table: a header and a cell per column:\n");
            for c in columns {
                s.push_str(&format!(
                    "    <th>{}</th>  {}\n",
                    c.field.label(),
                    c.field.cell()
                ));
            }
            s.trim_end().to_string()
        }
        Change::Remove { columns, .. } => format!(
            "  remove the form inputs, table headers and cells for `{}`",
            columns.join("`, `")
        ),
        Change::Empty => String::new(),
    }
}

// ---- the test ---------------------------------------------------------------------

fn patch_test(text: &str, _table: &str, change: &Change) -> std::result::Result<Patched, Why> {
    let marker = text
        .find(FIELDS_MARKER)
        .ok_or(format!("no `{FIELDS_MARKER}` marker in its sample body"))?;
    let line_start = text[..marker].rfind('\n').map_or(0, |i| i + 1);
    let indent = text[line_start..marker].to_string();
    let mut notes = Vec::new();
    let s = match change {
        Change::Add { columns, .. } => {
            let entries: String = columns
                .iter()
                .map(|c| format!("{indent}{}\n", c.field.sample_entry()))
                .collect();
            let mut s = text.to_string();
            s.insert_str(line_start, &entries);
            s
        }
        Change::Remove { columns, .. } => {
            let sample_start = text[..marker].rfind("fn sample()").unwrap_or(0);
            let (head, sample, tail) = (
                &text[..sample_start],
                &text[sample_start..marker],
                &text[marker..],
            );
            let sample: String = sample
                .split_inclusive('\n')
                .filter(|l| {
                    !columns
                        .iter()
                        .any(|c| l.trim_start().starts_with(&format!("\"{c}\":")))
                })
                .collect();
            for c in columns {
                if tail.contains(&format!("body[\"{c}\"]")) {
                    notes.push(format!(
                        "`invalid_input_is_422` still sets `{c}`; point it at another required \
                         text column or delete it"
                    ));
                }
            }
            format!("{head}{sample}{tail}")
        }
        Change::Empty => text.to_string(),
    };
    Ok(Patched { text: s, notes })
}

fn test_snippets(_table: &str, change: &Change) -> String {
    match change {
        Change::Add { columns, .. } => {
            let mut s = String::from("  add to the request bodies:\n");
            for c in columns {
                s.push_str(&format!("    {}\n", c.field.sample_entry()));
            }
            s.trim_end().to_string()
        }
        Change::Remove { columns, .. } => format!(
            "  remove `{}` from the request bodies",
            columns.join("`, `")
        ),
        Change::Empty => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::db_resource::{files, parse_fields, Plan};
    use crate::naming::ResourceNames;

    fn change(name: &str, fields: &[&str]) -> Change {
        let fields: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        Change::parse(name, &fields).unwrap()
    }

    #[test]
    fn names_say_what_the_migration_does() {
        let Change::Add { table, columns } = change("add_views_to_posts", &["views:int"]) else {
            panic!()
        };
        assert_eq!(table, "posts");
        assert_eq!(columns[0].field.name, "views");
        // Any case style; the table is after the last `_to_`.
        let Change::Add { table, .. } = change("AddDueToDateToTasks", &["due"]) else {
            panic!()
        };
        assert_eq!(table, "tasks");
        assert_eq!(
            change("remove_views_from_posts", &[]),
            Change::Remove {
                table: "posts".into(),
                columns: vec!["views".into()]
            }
        );
        assert_eq!(
            change("remove_stats_from_posts", &["views:int", "likes"]),
            Change::Remove {
                table: "posts".into(),
                columns: vec!["views".into(), "likes".into()]
            }
        );
        // Without fields, an add is an empty migration; so is any other name.
        assert_eq!(change("add_views_to_posts", &[]), Change::Empty);
        assert_eq!(change("backfill_slugs", &[]), Change::Empty);
        // No `<x>` before `_to_`: not an add.
        assert_eq!(change("add_to_posts", &[]), Change::Empty);
    }

    #[test]
    fn bad_specs_are_explained() {
        let err = |name: &str, fields: &[&str]| {
            let fields: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
            Change::parse(name, &fields).unwrap_err().to_string()
        };
        assert!(err("backfill", &["x:int"]).contains("add_<columns>_to_<table>"));
        assert!(err("add_x_to_t", &["x:int=abc"]).contains("bad default"));
        assert!(err("add_x_to_t", &["x:bool=maybe"]).contains("bad default"));
        assert!(err("add_x_to_t", &["x", "x:int"]).contains("twice"));
        assert!(err("add_x_to_t", &["id:int"]).contains("added automatically"));
        assert!(err("remove_x_from_t", &["order"]).contains("not a column name"));
    }

    #[test]
    fn required_columns_get_a_default() {
        let c = Column::parse("views:int").unwrap();
        assert_eq!((c.default.as_deref(), c.implied_default), (Some("0"), true));
        let c = Column::parse("views:int=10").unwrap();
        assert_eq!(
            (c.default.as_deref(), c.implied_default),
            (Some("10"), false)
        );
        let c = Column::parse("title=It's new").unwrap();
        assert_eq!(c.default.as_deref(), Some("'It''s new'"));
        assert_eq!(
            Column::parse("score:float").unwrap().default.as_deref(),
            Some("0.0")
        );
        assert_eq!(
            Column::parse("score:float=2").unwrap().default.as_deref(),
            Some("2.0")
        );
        assert_eq!(
            Column::parse("ok:bool=yes").unwrap().default.as_deref(),
            Some("TRUE")
        );
        let c = Column::parse("done:bool").unwrap();
        assert_eq!(
            (c.default.as_deref(), c.implied_default),
            (Some("FALSE"), false)
        );
        let c = Column::parse("bio:text?").unwrap();
        assert_eq!((c.default, c.implied_default), (None, false));
    }

    #[test]
    fn add_columns_sql_in_both_dialects() {
        let c = change(
            "add_stats_to_posts",
            &[
                "views:int",
                "rating:float?",
                "slug:string=draft",
                "pinned:bool",
            ],
        );
        let expected = "\
-- Created by `zapline generate migration add_stats_to_posts ...`.
-- Existing rows get 0 for views; new rows must set it.
ALTER TABLE posts ADD COLUMN views BIGINT NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN rating DOUBLE PRECISION;
ALTER TABLE posts ADD COLUMN slug TEXT NOT NULL DEFAULT 'draft';
ALTER TABLE posts ADD COLUMN pinned BOOLEAN NOT NULL DEFAULT FALSE;
";
        // ADD COLUMN is portable: the same statements on both.
        assert_eq!(c.sql(Dialect::Sqlite, "add_stats_to_posts ..."), expected);
        assert_eq!(c.sql(Dialect::Postgres, "add_stats_to_posts ..."), expected);
    }

    #[test]
    fn drop_column_sql_in_both_dialects() {
        let c = change("remove_views_from_posts", &[]);
        let sqlite = c.sql(Dialect::Sqlite, "remove_views_from_posts");
        assert!(sqlite.contains("SQLite 3.35"), "{sqlite}");
        assert!(
            sqlite.ends_with("ALTER TABLE posts DROP COLUMN views;\n"),
            "{sqlite}"
        );
        let pg = c.sql(Dialect::Postgres, "remove_views_from_posts");
        assert_eq!(
            pg,
            "-- Created by `zapline generate migration remove_views_from_posts`.\n\
             ALTER TABLE posts DROP COLUMN views;\n"
        );
        let empty = Change::Empty.sql(Dialect::Sqlite, "backfill");
        assert!(empty.contains("Applied once"), "{empty}");
    }

    // ---- patching: a patched module equals a freshly generated one --------------

    fn generated(fields: &[&str]) -> Vec<(String, String)> {
        let names = ResourceNames::parse("post", None).unwrap();
        let specs: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        let fields = parse_fields(&specs).unwrap();
        files(&Plan {
            names: &names,
            fields: &fields,
            dialect: Dialect::Sqlite,
            html: true,
            test_crate: Some("demo"),
            migration_name: "1_create_posts.sql".into(),
        })
        .into_iter()
        .map(|f| (f.path.to_string_lossy().replace('\\', "/"), f.contents))
        .collect()
    }

    fn file<'a>(files: &'a [(String, String)], path: &str) -> &'a str {
        &files.iter().find(|(p, _)| p == path).unwrap().1
    }

    /// Whitespace-insensitive: rustfmt reflows the real files anyway.
    fn squash(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn assert_patches(before: &[&str], name: &str, args: &[&str], after: &[&str]) {
        let old = generated(before);
        let new = generated(after);
        let c = change(name, args);
        for (path, patch) in [
            ("src/routes/posts.rs", patch_module as Patcher),
            ("templates/posts/index.html", patch_template),
            ("tests/posts.rs", patch_test),
        ] {
            let mut patched = patch(file(&old, path), "posts", &c)
                .unwrap_or_else(|e| panic!("{path}: {e}"))
                .text;
            let mut expected = file(&new, path).to_string();
            // `invalid_input_is_422` exists only with a required string
            // column; adding the first one does not write the test, and
            // removing the last one leaves it to the user (with a note).
            if patched.contains(INVALID) != expected.contains(INVALID) {
                patched = without_invalid_test(&patched);
                expected = without_invalid_test(&expected);
            }
            assert_eq!(
                squash(&patched),
                squash(&expected),
                "{path}:\n{patched}\n---- expected ----\n{expected}"
            );
        }
    }

    const INVALID: &str = "async fn invalid_input_is_422";

    fn without_invalid_test(s: &str) -> String {
        let Some(at) = s.find(INVALID) else {
            return s.to_string();
        };
        let start = s[..at].rfind("#[tokio::test]").unwrap();
        let end = s[at..].find("#[tokio::test]").map_or(s.len(), |i| at + i);
        format!("{}{}", &s[..start], &s[end..])
    }

    #[test]
    fn adding_columns_matches_generating_them() {
        assert_patches(
            &["title"],
            "add_views_to_posts",
            &["views:int=0"],
            &["title", "views:int"],
        );
        assert_patches(
            &["title", "body:text"],
            "add_extras_to_posts",
            &["score:float?", "done:bool", "slug", "notes:text?"],
            &[
                "title",
                "body:text",
                "score:float?",
                "done:bool",
                "slug",
                "notes:text?",
            ],
        );
        // The first searchable column adds `.search([..])`.
        assert_patches(
            &["views:int"],
            "add_title_to_posts",
            &["title"],
            &["views:int", "title"],
        );
    }

    #[test]
    fn removing_columns_matches_generating_without_them() {
        assert_patches(
            &["title", "views:int"],
            "remove_views_from_posts",
            &[],
            &["title"],
        );
        assert_patches(
            &["title", "body:text", "done:bool", "score:float?"],
            "remove_extras_from_posts",
            &["body", "done"],
            &["title", "score:float?"],
        );
        assert_patches(
            &["views:int", "title"],
            "remove_title_from_posts",
            &[],
            &["views:int"],
        );
    }

    #[test]
    fn rustfmt_layout_is_patched_too() {
        // The real file after rustfmt: the INSERT on its own line, binds
        // indented, the Listing arguments split over lines.
        let text = "\
#[derive(sqlx::FromRow)]
pub struct Post {
    pub id: i64,
    pub title: String,
    // zapline:fields
}

pub struct NewPost {
    #[validate(length(min = 1, max = 200))]
    pub title: String,
    // zapline:fields
}

/// `GET /api/posts` accepts `?limit=&offset=`, `?sort=-id`, `?q=` (searches title), `?title=`.
fn listing() -> Listing {
    Listing::new(
        \"posts\",
        \"id, title\",
    )
    .sortable([\"title\"])
    .search([\"title\"])
    .filter_text(\"title\")
}

async fn insert(db: &Db, new: &NewPost) -> Result<Post> {
    let row =
        sqlx::query_as::<_, Post>(\"INSERT INTO posts (title) VALUES ($1) RETURNING id, title\")
            .bind(&new.title)
            .fetch_one(&**db)
            .await?;
    Ok(row)
}

async fn replace(db: Db, id: i64, new: NewPost) -> Result<Post> {
    sqlx::query_as::<_, Post>(\"UPDATE posts SET title = $1 WHERE id = $2 RETURNING id, title\")
        .bind(&new.title)
        .bind(id)
        .fetch_one(&*db)
        .await
}
";
        let out = patch_module(text, "posts", &change("add_views_to_posts", &["views:int"]))
            .unwrap()
            .text;
        assert!(
            out.contains("pub views: i64,\n    // zapline:fields"),
            "{out}"
        );
        assert!(out.contains("\"id, title, views\""), "{out}");
        assert!(out.contains(
            "INSERT INTO posts (title, views) VALUES ($1, $2) RETURNING id, title, views"
        ));
        assert!(out.contains(
            "UPDATE posts SET title = $1, views = $2 WHERE id = $3 RETURNING id, title, views"
        ));
        assert_eq!(out.matches(".bind(new.views)").count(), 2, "{out}");
        assert!(out.contains(".filter_int(\"views\")"), "{out}");
        assert!(out.contains("`?title=`, `?views=`."), "{out}");
    }

    #[test]
    fn edited_or_unmarked_files_are_left_alone() {
        let old = generated(&["title"]);
        let module = file(&old, "src/routes/posts.rs");
        let c = change("add_views_to_posts", &["views:int"]);

        let unmarked = module.replace("    // zapline:fields\n", "");
        let why = patch_module(&unmarked, "posts", &c).unwrap_err();
        assert!(why.contains("marker"), "{why}");

        let custom = module.replace("VALUES ($1)", "VALUES (lower($1))");
        let why = patch_module(&custom, "posts", &c).unwrap_err();
        assert!(why.contains("placeholders"), "{why}");

        let twice = patch_module(module, "posts", &c).unwrap().text;
        let why = patch_module(&twice, "posts", &c).unwrap_err();
        assert!(why.contains("already selects `views`"), "{why}");

        let why =
            patch_module(module, "posts", &change("remove_body_from_posts", &[])).unwrap_err();
        assert!(why.contains("does not select `body`"), "{why}");
        let why =
            patch_module(module, "posts", &change("remove_title_from_posts", &[])).unwrap_err();
        assert!(why.contains("every column"), "{why}");

        // What the user is told to do instead.
        let s = module_snippets("posts", &c);
        assert!(s.contains("pub views: i64,"), "{s}");
        assert!(s.contains(".bind(new.views)"), "{s}");
        assert!(template_snippets("posts", &c).contains("type=\"number\""));
        assert!(test_snippets("posts", &c).contains("\"views\": 42,"));
    }

    #[test]
    fn removing_the_column_the_invalid_test_uses_is_flagged() {
        let old = generated(&["title", "name"]);
        let patched = patch_test(
            file(&old, "tests/posts.rs"),
            "posts",
            &change("remove_title_from_posts", &[]),
        )
        .unwrap();
        assert!(!patched.text.contains("\"title\": \"Example\""));
        assert_eq!(patched.notes.len(), 1, "{:?}", patched.notes);
    }

    #[test]
    fn finds_tables_created_by_migrations() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(MIGRATIONS_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("1_init.sql"),
            "CREATE TABLE IF NOT EXISTS users(id INTEGER);\ncreate table posts (id int);",
        )
        .unwrap();
        assert!(table_is_created(tmp.path(), "users"));
        assert!(table_is_created(tmp.path(), "posts"));
        assert!(!table_is_created(tmp.path(), "post"));
    }
}
