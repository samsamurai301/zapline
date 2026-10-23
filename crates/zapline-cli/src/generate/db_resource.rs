//! `zapline generate resource <name> field:type ...` in a project with a
//! database: a SQL migration, a sqlx-backed CRUD module (JSON API, plus htmx
//! pages when the project has templates) and tests. Also the SQL behind
//! `zapline generate migration`.
//!
//! The code is built here rather than from a `.tmpl` file because its shape
//! depends on the fields; it is formatted with rustfmt after writing.
//!
//! The module records the arguments it was generated from on a
//! [`RECIPE_MARKER`] line, so `zapline generate migration
//! add_<column>_to_<table>` can carry a schema change into the module, its
//! pages and its test (see [`super::evolve`]).

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::db::MigrationFiles;
use crate::naming::{is_keyword, snake_case, ResourceNames};
use crate::render::OutputFile;

/// Starts the comment line in a generated module that records what it was
/// generated from: `// zapline:resource post title:string body:text?`.
pub const RECIPE_MARKER: &str = "// zapline:resource";

/// A column type on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// Short text: `String`, `TEXT`, validated to 1..=200 characters.
    String,
    /// Long text: `String`, `TEXT`, shown as a textarea.
    Text,
    /// `i64`, `BIGINT`.
    Int,
    /// `f64`, `DOUBLE PRECISION`.
    Float,
    /// `bool`, `BOOLEAN`, a checkbox.
    Bool,
    /// A calendar day: `NaiveDate`, `DATE` (`TEXT` on SQLite), `type=date`.
    Date,
    /// A point in time: `DateTime<Utc>`, `TIMESTAMPTZ` (`TEXT` on SQLite),
    /// `type=datetime-local`.
    DateTime,
    /// `Uuid`, `UUID` (`BLOB` on SQLite, which is how sqlx stores it there).
    Uuid,
    /// Any JSON value: `JsonValue` (`serde_json::Value`), `JSONB` (`TEXT` on
    /// SQLite), a textarea holding JSON in forms.
    Json,
    /// `author:references`: an `author_id BIGINT` foreign key to `authors(id)`.
    References,
}

/// Every spelling `FieldType::parse` accepts, for error messages.
const TYPE_NAMES: &str = "string, text, int, float, bool, date, datetime, uuid, json, references";

impl FieldType {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "string" | "str" => FieldType::String,
            "text" => FieldType::Text,
            "int" | "integer" | "i64" => FieldType::Int,
            "float" | "f64" | "number" => FieldType::Float,
            "bool" | "boolean" => FieldType::Bool,
            "date" => FieldType::Date,
            "datetime" | "timestamp" | "timestamptz" => FieldType::DateTime,
            "uuid" => FieldType::Uuid,
            "json" | "jsonb" => FieldType::Json,
            "references" | "reference" | "ref" | "belongs_to" => FieldType::References,
            other => bail!(
                "unknown field type `{other}`; use one of: {TYPE_NAMES} \
                 (add `?` for optional, e.g. `bio:text?`, and `:unique` or `:index` after the type)"
            ),
        })
    }

    /// The command-line spelling: `string`, `int`, `references`...
    pub fn name(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Text => "text",
            FieldType::Int => "int",
            FieldType::Float => "float",
            FieldType::Bool => "bool",
            FieldType::Date => "date",
            FieldType::DateTime => "datetime",
            FieldType::Uuid => "uuid",
            FieldType::Json => "json",
            FieldType::References => "references",
        }
    }

    fn rust(self) -> &'static str {
        match self {
            FieldType::String | FieldType::Text => "String",
            FieldType::Int | FieldType::References => "i64",
            FieldType::Float => "f64",
            FieldType::Bool => "bool",
            FieldType::Date => "NaiveDate",
            FieldType::DateTime => "DateTime<Utc>",
            FieldType::Uuid => "Uuid",
            FieldType::Json => "JsonValue",
        }
    }

    /// The column type in `dialect`.
    pub fn sql(self, dialect: Dialect) -> &'static str {
        let pg = dialect == Dialect::Postgres;
        match self {
            FieldType::String | FieldType::Text => "TEXT",
            FieldType::Int | FieldType::References => "BIGINT",
            FieldType::Float => "DOUBLE PRECISION",
            FieldType::Bool => "BOOLEAN",
            FieldType::Date if pg => "DATE",
            FieldType::DateTime if pg => "TIMESTAMPTZ",
            FieldType::Uuid if pg => "UUID",
            FieldType::Json if pg => "JSONB",
            FieldType::Date | FieldType::DateTime | FieldType::Json => "TEXT",
            FieldType::Uuid => "BLOB",
        }
    }

    /// Whether `.bind()` takes the value by reference (it is not `Copy`).
    fn borrowed(self) -> bool {
        matches!(self, FieldType::String | FieldType::Text | FieldType::Json)
    }
}

/// The table a `references` field points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parent {
    /// The parent table's names (`authors`, `/authors`, `Author`...): `users`
    /// for `author:ref:users`.
    pub names: ResourceNames,
    /// What the field calls the parent: `author` for `author:references` and
    /// for `author:ref:users`. Names the column (`author_id`), the error code
    /// (`author_not_found`) and the nested list handler (`list_for_author`).
    pub role: String,
    /// The column `<select>`s show for a parent row: `name`, `title`... or `id`.
    pub label: String,
    /// The parent has an HTML page at `{path}/{id}` to link to.
    pub page: bool,
}

/// One `name:type` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Column and Rust field name (snake_case); `author_id` for `author:references`.
    pub name: String,
    /// Its type.
    pub ty: FieldType,
    /// `name:type?`: nullable column, `Option<T>` field.
    pub optional: bool,
    /// `:unique`: a unique index on the column; a duplicate is a 409
    /// `<name>_taken`.
    pub unique: bool,
    /// `:index`: an index on the column (references always get one).
    pub index: bool,
    /// For `references`, the table it points to.
    pub parent: Option<Parent>,
}

/// `bio?` -> (`bio`, true).
fn split_optional(part: &str) -> (&str, bool) {
    match part.strip_suffix('?') {
        Some(p) => (p, true),
        None => (part, false),
    }
}

impl Field {
    /// Parses `title`, `title:string`, `bio:text?`, `email:string:unique`,
    /// `rating:int:index`, `author:references`, `author:ref:users?`.
    pub fn parse(spec: &str) -> Result<Field> {
        let mut parts = spec.split(':');
        let raw_name = parts.next().unwrap_or_default();
        let (ty, mut optional) = parts.next().map_or(("string", false), split_optional);
        let ty = FieldType::parse(&ty.to_ascii_lowercase())?;
        let (mut unique, mut index, mut table) = (false, false, None::<String>);
        for (i, part) in parts.enumerate() {
            let (part, opt) = split_optional(part);
            optional |= opt;
            match part.to_ascii_lowercase().as_str() {
                "unique" => unique = true,
                "index" => index = true,
                t if ty == FieldType::References && i == 0 && !t.is_empty() => {
                    table = Some(part.to_string());
                }
                other => bail!(
                    "`{spec}`: unknown modifier `{other}`; use `unique` or `index`{}",
                    if ty == FieldType::References {
                        " (or a table right after the type, e.g. `author:ref:users`)"
                    } else {
                        ""
                    }
                ),
            }
        }
        let mut name = snake_case(raw_name);
        if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
            bail!("`{spec}`: field names must start with a letter");
        }
        let mut parent = None;
        if ty == FieldType::References {
            // `author_id:references` means the same as `author:references`.
            let stem = match name.strip_suffix("_id") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => name.clone(),
            };
            let role =
                ResourceNames::parse(&stem, None).map_err(|e| anyhow::anyhow!("`{spec}`: {e}"))?;
            let names = match &table {
                Some(t) => {
                    let t = snake_case(t);
                    ResourceNames::parse(&t, Some(&t))
                        .map_err(|e| anyhow::anyhow!("`{spec}`: {e}"))?
                }
                None => role.clone(),
            };
            if SQL_RESERVED.contains(&names.plural.as_str()) {
                bail!(
                    "`{spec}`: `{}` is a reserved word in SQL and can't be a table name",
                    names.plural
                );
            }
            name = format!("{}_id", role.singular);
            parent = Some(Parent {
                names,
                role: role.singular,
                label: "id".into(),
                page: false,
            });
        }
        if is_keyword(&name) {
            bail!("`{name}` is a Rust keyword; pick another field name");
        }
        if matches!(name.as_str(), "id" | "created_at" | "updated_at") {
            bail!("`{name}` is added automatically");
        }
        if SQL_RESERVED.contains(&name.as_str()) {
            bail!(
                "`{name}` is a reserved word in SQL; pick another field name (e.g. `is_{name}` or `{name}_value`)"
            );
        }
        if unique && ty == FieldType::Bool {
            bail!("`{spec}`: a bool can't be unique");
        }
        // A missing checkbox already means `false`.
        let optional = optional && ty != FieldType::Bool;
        Ok(Field {
            name,
            ty,
            optional,
            unique,
            index,
            parent,
        })
    }

    /// The argument that parses back into this field: `bio:text?`,
    /// `email:string:unique`, `author:references`, `author:references:users?`.
    pub fn spec(&self) -> String {
        let mut spec = match &self.parent {
            Some(p) => {
                let mut s = format!("{}:{}", p.role, self.ty.name());
                if p.names.plural != crate::naming::pluralize(&p.role) {
                    let _ = write!(s, ":{}", p.names.plural);
                }
                s
            }
            None => format!("{}:{}", self.name, self.ty.name()),
        };
        if self.unique {
            spec.push_str(":unique");
        }
        if self.index {
            spec.push_str(":index");
        }
        if self.optional {
            spec.push('?');
        }
        spec
    }

    /// What the field is called in hints and error codes: `author` for
    /// `author_id`, else its name.
    pub fn role(&self) -> &str {
        self.parent.as_ref().map_or(&self.name, |p| &p.role)
    }

    /// The field's Rust type, `Option<..>` when optional.
    pub fn rust_type(&self) -> String {
        if self.optional {
            format!("Option<{}>", self.ty.rust())
        } else {
            self.ty.rust().to_string()
        }
    }

    fn label(&self) -> String {
        let mut words = self.role().replace('_', " ");
        if let Some(first) = words.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        words
    }

    /// The column definition in a `CREATE TABLE` / `ADD COLUMN`.
    pub fn column_sql(&self, dialect: Dialect) -> String {
        let null = if self.optional { "" } else { " NOT NULL" };
        let default = if self.ty == FieldType::Bool {
            " DEFAULT FALSE"
        } else {
            ""
        };
        let fk = match &self.parent {
            // A required parent takes its children with it; an optional one
            // leaves them parentless.
            Some(p) => format!(
                " REFERENCES {}(id) ON DELETE {}",
                p.names.plural,
                if self.optional { "SET NULL" } else { "CASCADE" }
            ),
            None => String::new(),
        };
        format!("{} {}{null}{default}{fk}", self.name, self.ty.sql(dialect))
    }

    /// The index the column gets, if any: unique for `:unique` (named
    /// `<table>_<column>_key`, like the constraint Postgres makes for an inline
    /// `UNIQUE`), plain for `:index` and for references. An index rather than
    /// an inline constraint, so SQLite can still drop the column later.
    pub fn index_sql(&self, table: &str) -> String {
        if self.unique {
            format!(
                "CREATE UNIQUE INDEX {table}_{c}_key ON {table} ({c});\n",
                c = self.name
            )
        } else if self.index || self.parent.is_some() {
            index_sql(table, &self.name)
        } else {
            String::new()
        }
    }

    /// `DROP INDEX` for [`index_sql`](Self::index_sql): needed before SQLite
    /// drops the column.
    pub fn drop_index_sql(&self, table: &str, if_exists: bool) -> String {
        let suffix = if self.unique {
            "key"
        } else if self.index || self.parent.is_some() {
            "idx"
        } else {
            return String::new();
        };
        let if_exists = if if_exists { "IF EXISTS " } else { "" };
        format!("DROP INDEX {if_exists}{table}_{}_{suffix};\n", self.name)
    }

    /// A value for tests, as a `json!` expression (a variable for references).
    fn sample_json(&self) -> String {
        match self.ty {
            FieldType::String => "\"Example\"".into(),
            FieldType::Text => "\"Some longer text.\"".into(),
            FieldType::Int => "42".into(),
            FieldType::Float => "1.5".into(),
            FieldType::Bool => "true".into(),
            FieldType::Date => "\"2026-01-15\"".into(),
            FieldType::DateTime => "\"2026-01-15T09:30:00Z\"".into(),
            FieldType::Uuid => "\"67e55044-10b1-426f-9247-bb680e5fe0c8\"".into(),
            FieldType::Json => "{\"tags\": [\"a\", \"b\"], \"n\": 1}".into(),
            FieldType::References => self.name.clone(),
        }
    }

    /// A different value for tests of updates.
    fn changed_json(&self) -> String {
        match self.ty {
            FieldType::String => "\"Changed\"".into(),
            FieldType::Text => "\"Changed text.\"".into(),
            FieldType::Int => "7".into(),
            FieldType::Float => "2.5".into(),
            FieldType::Bool => "false".into(),
            FieldType::Date => "\"2026-02-28\"".into(),
            FieldType::DateTime => "\"2026-02-28T18:45:00Z\"".into(),
            FieldType::Uuid => "\"a1a2a3a4-b1b2-4c1c-8d1d-e1e2e3e4e5e6\"".into(),
            FieldType::Json => "{\"n\": 2}".into(),
            FieldType::References => self.name.clone(),
        }
    }

    /// What a browser form sends for the changed value, as a `String` expression.
    fn changed_form(&self) -> String {
        match self.ty {
            FieldType::String => "\"Changed\".to_string()".into(),
            FieldType::Text => "\"Changed text.\".to_string()".into(),
            FieldType::Int => "\"7\".to_string()".into(),
            FieldType::Float => "\"2.5\".to_string()".into(),
            FieldType::Bool => "\"on\".to_string()".into(),
            FieldType::Date => "\"2026-02-28\".to_string()".into(),
            FieldType::DateTime => "\"2026-02-28T18:45\".to_string()".into(),
            FieldType::Uuid => "\"a1a2a3a4-b1b2-4c1c-8d1d-e1e2e3e4e5e6\".to_string()".into(),
            FieldType::Json => r#""{\"n\": 2}".to_string()"#.into(),
            FieldType::References => format!("{}.to_string()", self.name),
        }
    }

    /// The serde attribute for the create/replace payload (JSON or a form).
    fn input_attr(&self) -> Option<&'static str> {
        Some(match (self.ty, self.optional) {
            (FieldType::Bool, _) => {
                "#[serde(default, deserialize_with = \"zapline::extract::checkbox\")]"
            }
            (FieldType::DateTime, false) => {
                "#[serde(deserialize_with = \"zapline::extract::datetime\")]"
            }
            (FieldType::DateTime, true) => {
                "#[serde(default, deserialize_with = \"zapline::extract::optional_datetime\")]"
            }
            // A form's textarea sends the JSON as text.
            (FieldType::Json, false) => {
                "#[serde(deserialize_with = \"zapline::extract::json_text\")]"
            }
            (FieldType::Json, true) => {
                "#[serde(default, deserialize_with = \"zapline::extract::json_text\")]"
            }
            (_, true) => {
                "#[serde(default, deserialize_with = \"zapline::extract::empty_as_none\")]"
            }
            (FieldType::String, false) => "#[validate(length(min = 1, max = 200))]",
            (FieldType::Text, false) => "#[validate(length(max = 100000))]",
            _ => return None,
        })
    }
}

/// Words SQLite or Postgres reserve: unusable as unquoted column names.
const SQL_RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "between",
    "both",
    "by",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "cross",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "delete",
    "desc",
    "distinct",
    "do",
    "drop",
    "else",
    "end",
    "except",
    "exists",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "in",
    "index",
    "initially",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "lateral",
    "leading",
    "left",
    "like",
    "limit",
    "natural",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "placing",
    "primary",
    "references",
    "returning",
    "right",
    "select",
    "session_user",
    "set",
    "some",
    "symmetric",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "update",
    "user",
    "using",
    "values",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// The [`RECIPE_MARKER`] line for a resource: the `generate resource`
/// arguments, `--plural` only when it is not the regular plural.
pub fn recipe(names: &ResourceNames, fields: &[Field]) -> String {
    let mut line = format!("{RECIPE_MARKER} {}", names.singular);
    let regular = ResourceNames::parse(&names.singular, None).ok();
    if regular.is_none_or(|r| r.plural != names.plural) {
        let _ = write!(line, " --plural {}", names.plural);
    }
    for f in fields {
        let _ = write!(line, " {}", f.spec());
    }
    line
}

/// Reads the [`RECIPE_MARKER`] line of a generated module back into the
/// resource's names and fields (parents not yet resolved). `None` when the
/// module has no such line.
pub fn parse_recipe(module: &str) -> Option<Result<(ResourceNames, Vec<Field>)>> {
    let line = module
        .lines()
        .find_map(|l| l.trim().strip_prefix(RECIPE_MARKER))?;
    let mut words = line.split_whitespace();
    let parsed = (|| {
        let singular = words
            .next()
            .ok_or_else(|| anyhow::anyhow!("`{RECIPE_MARKER}` names no resource"))?;
        let mut plural = None;
        let mut specs = Vec::new();
        while let Some(word) = words.next() {
            if word == "--plural" {
                plural = words.next();
            } else {
                specs.push(word.to_string());
            }
        }
        Ok((
            ResourceNames::parse(singular, plural)?,
            parse_field_list(&specs)?,
        ))
    })();
    Some(parsed)
}

/// Parses all field arguments; defaults to `name:string` when there are none.
pub fn parse_fields(specs: &[String]) -> Result<Vec<Field>> {
    if specs.is_empty() {
        return Ok(vec![Field::parse("name:string")?]);
    }
    parse_field_list(specs)
}

/// Parses field arguments, rejecting duplicates; an empty list stays empty.
pub fn parse_field_list(specs: &[String]) -> Result<Vec<Field>> {
    let mut fields: Vec<Field> = Vec::new();
    for spec in specs {
        let field = Field::parse(spec)?;
        if fields.iter().any(|f| f.name == field.name) {
            bail!("field `{}` is listed twice", field.name);
        }
        fields.push(field);
    }
    Ok(fields)
}

/// Which SQL dialect the migration targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `INTEGER PRIMARY KEY AUTOINCREMENT`.
    Sqlite,
    /// `BIGSERIAL PRIMARY KEY`.
    Postgres,
}

/// What to generate.
pub struct Plan<'a> {
    /// Resource names.
    pub names: &'a ResourceNames,
    /// Columns besides `id` and the timestamps.
    pub fields: &'a [Field],
    /// SQL dialect of the migration.
    pub dialect: Dialect,
    /// Generate htmx pages (the project has `templates/base.html`).
    pub html: bool,
    /// The app has sessions, so the pages can set flash messages.
    pub flash: bool,
    /// Crate name for the test's imports, when a test can be generated.
    pub test_crate: Option<&'a str>,
    /// Migration file names (`<timestamp>_create_<plural>.up.sql` + `.down.sql`).
    pub migration: MigrationFiles,
}

impl Plan<'_> {
    fn references(&self) -> impl Iterator<Item = (&Field, &Parent)> {
        self.fields
            .iter()
            .filter_map(|f| f.parent.as_ref().map(|p| (f, p)))
    }

    fn uses(&self, ty: FieldType) -> bool {
        self.fields.iter().any(|f| f.ty == ty)
    }

    /// The references that get a nested list route: all but those whose
    /// table another field refers to as well (`/users/{id}/posts` would be
    /// ambiguous for `author:ref:users editor:ref:users`).
    fn nested(&self) -> impl Iterator<Item = (&Field, &Parent)> {
        self.references().filter(|(_, p)| {
            self.references()
                .filter(|(_, o)| o.names.plural == p.names.plural)
                .count()
                == 1
        })
    }

    /// Each table the references point to, once: one `<select>` list each.
    fn choice_tables(&self) -> Vec<&Parent> {
        let mut out: Vec<&Parent> = Vec::new();
        for (_, p) in self.references() {
            if !out.iter().any(|o| o.names.plural == p.names.plural) {
                out.push(p);
            }
        }
        out
    }

    /// Whether `field` refers to this resource's own table.
    fn is_self(&self, field: &Field) -> bool {
        field
            .parent
            .as_ref()
            .is_some_and(|p| p.names.plural == self.names.plural)
    }

    /// Whether writes can fail in a way worth a named error: a missing
    /// parent or a duplicate in a unique column.
    fn checked_writes(&self) -> bool {
        self.fields.iter().any(|f| f.unique || f.parent.is_some())
    }
}

/// The references in `fields` that share a table with another one, as
/// `(table, [column, ..])`: they get no nested list route.
pub fn ambiguous_references(fields: &[Field]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for f in fields {
        let Some(p) = &f.parent else { continue };
        match out.iter_mut().find(|(t, _)| *t == p.names.plural) {
            Some((_, cols)) => cols.push(f.name.clone()),
            None => out.push((p.names.plural.clone(), vec![f.name.clone()])),
        }
    }
    out.retain(|(_, cols)| cols.len() > 1);
    out
}

/// The column `<select>`s show for a row of a resource with `fields`:
/// `name`, `title`, `label`, `email` or `username`, else the first required
/// text column, else `id`. For a resource that refers to itself, whose
/// table has no migration yet.
pub fn label_field(fields: &[Field]) -> String {
    for preferred in ["name", "title", "label", "email", "username"] {
        if fields.iter().any(|f| f.name == preferred) {
            return preferred.to_string();
        }
    }
    fields
        .iter()
        .find(|f| matches!(f.ty, FieldType::String | FieldType::Text) && !f.optional)
        .map(|f| f.name.clone())
        .unwrap_or_else(|| "id".to_string())
}

/// The migration (and its down file), the module, the templates and the test.
pub fn files(plan: &Plan<'_>) -> Vec<OutputFile> {
    let n = plan.names;
    let mut out = vec![OutputFile {
        path: PathBuf::from(format!("migrations/{}", plan.migration.up)),
        contents: migration(plan),
    }];
    if let Some(down) = &plan.migration.down {
        out.push(OutputFile {
            path: PathBuf::from(format!("migrations/{down}")),
            contents: format!(
                "-- Reverts {}: run by `zapline db rollback`.\nDROP TABLE {};\n",
                plan.migration.up, n.plural
            ),
        });
    }
    out.push(OutputFile {
        path: PathBuf::from(format!("src/routes/{}.rs", n.plural)),
        contents: module(plan),
    });
    if plan.html {
        for (name, contents) in [
            ("index", index_template(plan)),
            ("_row", row_template(plan)),
            ("show", show_template(plan)),
            ("edit", edit_template(plan)),
        ] {
            out.push(OutputFile {
                path: PathBuf::from(format!("templates/{}/{name}.html", n.plural)),
                contents,
            });
        }
    }
    if let Some(krate) = plan.test_crate {
        out.push(OutputFile {
            path: PathBuf::from(format!("tests/{}.rs", n.plural)),
            contents: test(plan, krate),
        });
    }
    out
}

/// `CREATE TABLE` with an `id`, the fields, `created_at`/`updated_at` and an
/// index per foreign key, `:index` and `:unique` column.
pub fn create_table_sql(table: &str, fields: &[Field], dialect: Dialect) -> String {
    let (id, ts) = match dialect {
        Dialect::Sqlite => ("id INTEGER PRIMARY KEY AUTOINCREMENT", "TEXT"),
        Dialect::Postgres => ("id BIGSERIAL PRIMARY KEY", "TIMESTAMPTZ"),
    };
    let mut cols = vec![format!("    {id}")];
    for f in fields {
        cols.push(format!("    {}", f.column_sql(dialect)));
    }
    for c in ["created_at", "updated_at"] {
        cols.push(format!("    {c} {ts} NOT NULL DEFAULT CURRENT_TIMESTAMP"));
    }
    let mut sql = format!("CREATE TABLE {table} (\n{}\n);\n", cols.join(",\n"));
    for f in fields {
        sql.push_str(&f.index_sql(table));
    }
    sql
}

/// `CREATE INDEX <table>_<column>_idx ...`: foreign keys are not indexed
/// automatically, and every nested list filters on one.
pub fn index_sql(table: &str, column: &str) -> String {
    format!("CREATE INDEX {table}_{column}_idx ON {table} ({column});\n")
}

fn migration(plan: &Plan<'_>) -> String {
    format!(
        "-- Created by `zapline generate resource {}`.\n{}",
        plan.names.singular,
        create_table_sql(&plan.names.plural, plan.fields, plan.dialect)
    )
}

/// `/api/posts` when there is an HTML page at `/posts`, otherwise `/posts`.
fn api_path(plan: &Plan<'_>) -> String {
    format!("{}{}", api_prefix(plan), plan.names.path)
}

fn api_prefix(plan: &Plan<'_>) -> &'static str {
    if plan.html {
        "/api"
    } else {
        ""
    }
}

/// `GET /api/authors/{id}/posts`.
fn nested_path(plan: &Plan<'_>, parent: &Parent) -> String {
    format!(
        "{}{}/{{id}}{}",
        api_prefix(plan),
        parent.names.path,
        plan.names.path
    )
}

fn binds(fields: &[Field], var: &str) -> String {
    fields
        .iter()
        .map(|f| {
            format!(
                "\n    .bind({}{var}.{})",
                if f.ty.borrowed() { "&" } else { "" },
                f.name
            )
        })
        .collect()
}

/// `app.mount((a, b, ...))`, split so no tuple has more than 16 elements.
fn mount_chain(handlers: &[String]) -> String {
    let mut s = String::from("    app");
    for chunk in handlers.chunks(16) {
        let _ = write!(s, "\n        .mount(({},))", chunk.join(", "));
    }
    s
}

fn module(plan: &Plan<'_>) -> String {
    let n = plan.names;
    let p = &n.pascal;
    let t = &n.plural;
    let api = api_path(plan);
    let cols: Vec<&str> = plan.fields.iter().map(|f| f.name.as_str()).collect();
    let select_cols = format!("id, {}, created_at, updated_at", cols.join(", "));
    let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
    let sets: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c} = ${}", i + 1))
        .collect();
    let coalesce: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c} = COALESCE(${}, {c})", i + 1))
        .collect();
    let id_param = cols.len() + 1;
    let new_binds = binds(plan.fields, "new");
    let change_binds = binds(plan.fields, "changes");
    let refs: Vec<(&Field, &Parent)> = plan.references().collect();
    let nested: Vec<(&Field, &Parent)> = plan.nested().collect();
    // How a write query ends: `?`, or through `write_error` for named errors.
    let finish = |var: &str, all_optional: bool| -> String {
        if !plan.checked_writes() {
            return "\n    .fetch_one(&**db)\n    .await?;\n    Ok(row)".to_string();
        }
        if refs.is_empty() {
            return "\n    .fetch_one(&**db)\n    .await\n    .map_err(write_error)?;\n    Ok(row)"
                .to_string();
        }
        let ids: Vec<String> = refs
            .iter()
            .map(|(f, _)| {
                if f.optional || all_optional {
                    format!("{var}.{}", f.name)
                } else {
                    format!("Some({var}.{})", f.name)
                }
            })
            .collect();
        format!(
            "\n    .fetch_one(&**db)\n    .await;\n    \
             match row {{\n        Ok(row) => Ok(row),\n        \
             Err(err) => Err(write_error(db, err, [{}]).await),\n    }}",
            ids.join(", ")
        )
    };

    let mut s = String::new();
    let _ = writeln!(
        s,
        "//! `{}` resource: SQL-backed CRUD. JSON API under `{api}`{}.",
        n.pascal,
        if plan.html {
            format!(", HTML pages at `{}`", n.path)
        } else {
            String::new()
        }
    );
    let _ = writeln!(s, "//!");
    let _ = writeln!(
        s,
        "//! Table `{t}` is created by the migration generated alongside this file."
    );
    let _ = writeln!(
        s,
        "\n{}\n// `zapline g migration add_<columns>_to_{t}` (or `remove_<columns>_from_{t}`) \
         reads the line above to update this file, its pages and its test.\n",
        recipe(n, plan.fields)
    );
    if plan.uses(FieldType::Date) {
        s.push_str("use zapline::chrono::{DateTime, NaiveDate, Utc};\n");
    } else {
        s.push_str("use zapline::chrono::{DateTime, Utc};\n");
    }
    s.push_str("use zapline::db::{sqlx, Db, ListParams, Listing, Paged};\n");
    s.push_str("use zapline::prelude::*;\n");
    if plan.uses(FieldType::Json) {
        s.push_str("use zapline::db::sqlx::types::JsonValue;\n");
    }
    if plan.uses(FieldType::Uuid) {
        s.push_str("use zapline::uuid::Uuid;\n");
    }
    if plan.html && plan.flash {
        s.push_str("use zapline::session::Flash;\n");
    }
    if plan.html {
        s.push_str("use zapline::view::{context, HxRequest, ValidForm, Views};\n");
    }
    s.push('\n');

    // Model.
    let _ = writeln!(s, "/// A stored {}.", n.human);
    s.push_str("#[model]\n#[derive(sqlx::FromRow)]\n");
    let _ = writeln!(s, "pub struct {p} {{");
    s.push_str("    pub id: i64,\n");
    for f in plan.fields {
        let _ = writeln!(s, "    pub {}: {},", f.name, f.rust_type());
    }
    s.push_str("    pub created_at: DateTime<Utc>,\n    pub updated_at: DateTime<Utc>,\n");
    s.push_str("}\n\n");

    // Input.
    let _ = writeln!(
        s,
        "/// Payload for creating or replacing {} (JSON or an HTML form).",
        n.a_human
    );
    s.push_str("#[model]\n");
    let _ = writeln!(s, "pub struct New{p} {{");
    for f in plan.fields {
        if let Some(attr) = f.input_attr() {
            let _ = writeln!(s, "    {attr}");
        }
        let _ = writeln!(s, "    pub {}: {},", f.name, f.rust_type());
    }
    s.push_str("}\n\n");

    // Partial update.
    let _ = writeln!(
        s,
        "/// Payload for a partial update of {}: fields left out keep their value.",
        n.a_human
    );
    s.push_str("#[model]\n");
    let _ = writeln!(s, "pub struct Update{p} {{");
    for f in plan.fields {
        match f.ty {
            FieldType::String => s.push_str("    #[validate(length(min = 1, max = 200))]\n"),
            FieldType::Text => s.push_str("    #[validate(length(max = 100000))]\n"),
            FieldType::DateTime => s.push_str(
                "    #[serde(default, deserialize_with = \"zapline::extract::optional_datetime\")]\n",
            ),
            FieldType::Json => s.push_str(
                "    #[serde(default, deserialize_with = \"zapline::extract::json_text\")]\n",
            ),
            _ => {}
        }
        let _ = writeln!(s, "    pub {}: Option<{}>,", f.name, f.ty.rust());
    }
    s.push_str("}\n\n");

    if plan.html && !refs.is_empty() {
        s.push_str("/// One option of a `<select>`.\n#[model]\n#[derive(sqlx::FromRow)]\n");
        s.push_str("pub struct Choice {\n    pub id: i64,\n    pub label: String,\n}\n\n");
    }

    // What the list endpoint accepts.
    let mut sortable: Vec<String> = plan
        .fields
        .iter()
        .filter(|f| !matches!(f.ty, FieldType::Text | FieldType::Json))
        .map(|f| format!("\"{}\"", f.name))
        .collect();
    sortable.extend(["\"created_at\"".to_owned(), "\"updated_at\"".to_owned()]);
    let searchable: Vec<String> = plan
        .fields
        .iter()
        .filter(|f| matches!(f.ty, FieldType::String | FieldType::Text))
        .map(|f| format!("\"{}\"", f.name))
        .collect();
    let mut accepts = vec!["`?limit=&offset=`".to_owned(), "`?sort=-id`".to_owned()];
    let mut chain = format!(
        "    Listing::new(\"{t}\", \"{select_cols}\")\n        .sortable([{}])\n",
        sortable.join(", "),
    );
    if !searchable.is_empty() {
        accepts.push(format!(
            "`?q=` (searches {})",
            searchable.join(", ").replace('"', "")
        ));
        let _ = writeln!(chain, "        .search([{}])", searchable.join(", "));
    }
    for f in plan.fields {
        let filter = match f.ty {
            FieldType::Int | FieldType::References => "filter_int",
            FieldType::Bool => "filter_bool",
            FieldType::String => "filter_text",
            // Text is searched; the rest would need typed comparisons.
            FieldType::Text
            | FieldType::Float
            | FieldType::Date
            | FieldType::DateTime
            | FieldType::Uuid
            | FieldType::Json => continue,
        };
        accepts.push(format!("`?{}=`", f.name));
        let _ = writeln!(chain, "        .{filter}(\"{}\")", f.name);
    }
    let _ = writeln!(
        s,
        "/// `GET {api}` accepts {}.\nfn listing() -> Listing {{\n{}}}\n",
        accepts.join(", "),
        chain
    );

    // Mount.
    let mut handlers: Vec<String> = ["list", "get_one", "create", "replace", "update", "remove"]
        .iter()
        .map(|h| h.to_string())
        .collect();
    for (_, parent) in &nested {
        handlers.push(format!("list_for_{}", parent.role));
    }
    if plan.html {
        handlers.extend(
            [
                "index_page",
                "create_form",
                "show_page",
                "edit_page",
                "update_form",
                "remove_form",
            ]
            .iter()
            .map(|h| h.to_string()),
        );
    }
    let _ = writeln!(s, "/// Adds the {} routes.", n.human);
    let _ = writeln!(
        s,
        "pub fn mount(app: App) -> App {{\n{}\n}}\n",
        mount_chain(&handlers)
    );

    // Queries.
    s.push_str(
        "// ---- queries ---------------------------------------------------------------\n\n",
    );
    let _ = writeln!(
        s,
        "async fn find(db: &Db, id: i64) -> Result<{p}> {{\n    \
         let row = sqlx::query_as::<_, {p}>(\"SELECT {select_cols} FROM {t} WHERE id = $1\")\n        \
         .bind(id)\n        .fetch_one(&**db)\n        .await?;\n    Ok(row)\n}}\n"
    );
    let _ = writeln!(
        s,
        "async fn insert(db: &Db, new: &New{p}) -> Result<{p}> {{\n    \
         let row = sqlx::query_as::<_, {p}>(\n        \"INSERT INTO {t} ({cols}) VALUES ({vals}) RETURNING {select_cols}\",\n    ){new_binds}{end}\n}}\n",
        cols = cols.join(", "),
        vals = placeholders.join(", "),
        end = finish("new", false),
    );
    let _ = writeln!(
        s,
        "async fn update_row(db: &Db, id: i64, new: &New{p}) -> Result<{p}> {{\n    \
         let row = sqlx::query_as::<_, {p}>(\n        \"UPDATE {t} SET {sets}, updated_at = CURRENT_TIMESTAMP WHERE id = ${id_param} RETURNING {select_cols}\",\n    ){new_binds}\n    .bind(id){end}\n}}\n",
        sets = sets.join(", "),
        end = finish("new", false),
    );
    let _ = writeln!(
        s,
        "/// Fields that are `None` keep their value.\n\
         async fn patch_row(db: &Db, id: i64, changes: &Update{p}) -> Result<{p}> {{\n    \
         let row = sqlx::query_as::<_, {p}>(\n        \"UPDATE {t} SET {sets}, updated_at = CURRENT_TIMESTAMP WHERE id = ${id_param} RETURNING {select_cols}\",\n    ){change_binds}\n    .bind(id){end}\n}}\n",
        sets = coalesce.join(", "),
        end = finish("changes", true),
    );
    if plan.checked_writes() {
        write_error_fn(&mut s, plan);
    }
    let _ = writeln!(
        s,
        "async fn delete_row(db: &Db, id: i64) -> Result<()> {{\n    \
         let done = sqlx::query(\"DELETE FROM {t} WHERE id = $1\")\n        .bind(id)\n        .execute(&**db)\n        .await?;\n    \
         if done.rows_affected() == 0 {{\n        return Err(Error::not_found());\n    }}\n    Ok(())\n}}\n"
    );
    if plan.html {
        for parent in plan.choice_tables() {
            let _ = writeln!(
                s,
                "/// The {ph} to pick from in forms.\n\
                 async fn {sg}_choices(db: &Db) -> Result<Vec<Choice>> {{\n    \
                 let rows = sqlx::query_as::<_, Choice>(\n        \"SELECT id, CAST({l} AS TEXT) AS label FROM {pt} ORDER BY {l} LIMIT 1000\",\n    )\n    \
                 .fetch_all(&**db)\n    .await?;\n    Ok(rows)\n}}\n",
                ph = parent.names.plural_human,
                sg = parent.names.singular,
                l = parent.label,
                pt = parent.names.plural,
            );
        }
    }

    // JSON API.
    s.push_str(
        "// ---- JSON API --------------------------------------------------------------\n\n",
    );
    let _ = writeln!(
        s,
        "/// List {ph}.\n#[get(\"{api}\", tag = \"{t}\")]\n\
         async fn list(db: Db, params: ListParams) -> Result<Json<Paged<{p}>>> {{\n    \
         Ok(Json(listing().fetch(&db, &params).await?))\n}}\n",
        ph = n.plural_human,
    );
    let _ = writeln!(
        s,
        "/// Fetch one {h}.\n#[get(\"{api}/{{id}}\", tag = \"{t}\")]\n\
         async fn get_one(db: Db, Path(id): Path<i64>) -> Result<Json<{p}>> {{\n    \
         Ok(Json(find(&db, id).await?))\n}}\n",
        h = n.human,
    );
    let _ = writeln!(
        s,
        "/// Create {a}.\n#[post(\"{api}\", tag = \"{t}\", status = 201)]\n\
         async fn create(db: Db, Valid(Json(new)): Valid<Json<New{p}>>) -> Result<(StatusCode, Json<{p}>)> {{\n    \
         Ok((StatusCode::CREATED, Json(insert(&db, &new).await?)))\n}}\n",
        a = n.a_human,
    );
    let _ = writeln!(
        s,
        "/// Replace {a}.\n#[put(\"{api}/{{id}}\", tag = \"{t}\")]\n\
         async fn replace(\n    db: Db,\n    Path(id): Path<i64>,\n    Valid(Json(new)): Valid<Json<New{p}>>,\n) -> Result<Json<{p}>> {{\n    \
         Ok(Json(update_row(&db, id, &new).await?))\n}}\n",
        a = n.a_human,
    );
    let _ = writeln!(
        s,
        "/// Change some fields of {a}.\n#[patch(\"{api}/{{id}}\", tag = \"{t}\")]\n\
         async fn update(\n    db: Db,\n    Path(id): Path<i64>,\n    Valid(Json(changes)): Valid<Json<Update{p}>>,\n) -> Result<Json<{p}>> {{\n    \
         Ok(Json(patch_row(&db, id, &changes).await?))\n}}\n",
        a = n.a_human,
    );
    let _ = writeln!(
        s,
        "/// Delete {a}.\n#[delete(\"{api}/{{id}}\", tag = \"{t}\", status = 204)]\n\
         async fn remove(db: Db, Path(id): Path<i64>) -> Result<StatusCode> {{\n    \
         delete_row(&db, id).await?;\n    Ok(StatusCode::NO_CONTENT)\n}}\n",
        a = n.a_human,
    );
    for (field, parent) in &nested {
        let _ = writeln!(
            s,
            "/// List the {ph} of {pa} (404 if there is no such {phh}).\n#[get(\"{path}\", tag = \"{t}\")]\n\
             async fn list_for_{role}(\n    db: Db,\n    Path(id): Path<i64>,\n    params: ListParams,\n) -> Result<Json<Paged<{p}>>> {{\n    \
             sqlx::query(\"SELECT id FROM {pt} WHERE id = $1\")\n        .bind(id)\n        .fetch_one(&*db)\n        .await?;\n    \
             let listing = listing().scope(\"{col}\", id);\n    \
             Ok(Json(listing.fetch(&db, &params).await?))\n}}\n",
            ph = n.plural_human,
            pa = parent.names.a_human,
            phh = parent.names.human,
            path = nested_path(plan, parent),
            role = parent.role,
            pt = parent.names.plural,
            col = field.name,
        );
    }

    if plan.html {
        html_handlers(&mut s, plan);
    }
    s.trim_end().to_string() + "\n"
}

/// `write_error`, which turns a failed insert or update into an error that
/// names the field at fault (and `missing`, which it asks).
fn write_error_fn(s: &mut String, plan: &Plan<'_>) {
    let t = &plan.names.plural;
    let refs: Vec<(&Field, &Parent)> = plan.references().collect();
    let uniques: Vec<&Field> = plan.fields.iter().filter(|f| f.unique).collect();
    let mut doc = Vec::new();
    if let Some((f, _)) = refs.first() {
        doc.push(format!(
            "a 422 `{}_not_found` for a missing {}",
            f.role(),
            f.role().replace('_', " ")
        ));
    }
    if let Some(f) = uniques.first() {
        doc.push(format!(
            "a 409 `{}_taken` for a duplicate {}",
            f.name,
            f.name.replace('_', " ")
        ));
    }
    let _ = writeln!(
        s,
        "/// The error for a failed insert or update, naming the field at fault:\n\
         /// {}.\n/// Anything else is what `?` would give.",
        doc.join(",\n/// ")
    );
    let mut body = String::new();
    if !refs.is_empty() {
        let vars: Vec<&str> = refs.iter().map(|(f, _)| f.name.as_str()).collect();
        let _ = write!(
            body,
            "    if e.is_foreign_key_violation() {{\n        let [{}] = parents;",
            vars.join(", ")
        );
        for (f, p) in &refs {
            let _ = write!(
                body,
                "\n        if missing(db, \"{pt}\", {c}).await {{\n            \
                 return Error::unprocessable(\"{h} not found\").with_code(\"{r}_not_found\");\n        }}",
                pt = p.names.plural,
                c = f.name,
                h = title_case(&p.role.replace('_', " ")),
                r = p.role,
            );
        }
        body.push_str("\n    }\n");
    }
    for f in &uniques {
        // Postgres names the index; SQLite says `UNIQUE constraint failed: posts.slug`.
        let _ = writeln!(
            body,
            "    if e.is_unique_violation()\n        \
             && (e.constraint() == Some(\"{t}_{c}_key\") || e.message().ends_with(\": {t}.{c}\"))\n    {{\n        \
             return Error::conflict(\"{h} is already taken\").with_code(\"{c}_taken\");\n    }}",
            c = f.name,
            h = title_case(&f.name.replace('_', " ")),
        );
    }
    if refs.is_empty() {
        let _ = writeln!(
            s,
            "fn write_error(err: sqlx::Error) -> Error {{\n    \
             let sqlx::Error::Database(e) = &err else {{\n        return err.into();\n    }};\n\
             {body}    err.into()\n}}\n"
        );
        return;
    }
    let _ = writeln!(
        s,
        "async fn write_error(db: &Db, err: sqlx::Error, parents: [Option<i64>; {n}]) -> Error {{\n    \
         let sqlx::Error::Database(e) = &err else {{\n        return err.into();\n    }};\n\
         {body}    err.into()\n}}\n",
        n = refs.len()
    );
    s.push_str(
        "/// Whether `table` has no row `id`; asked only after a write failed.\n\
         async fn missing(db: &Db, table: &str, id: Option<i64>) -> bool {\n    \
         let Some(id) = id else {\n        return false;\n    };\n    \
         let sql = format!(\"SELECT COUNT(*) FROM {table} WHERE id = $1\");\n    \
         let found = sqlx::query_scalar::<_, i64>(&sql).bind(id).fetch_one(&**db).await;\n    \
         matches!(found, Ok(0))\n}\n\n",
    );
}

/// `let author_choices = author_choices(&db).await?;` lines and the names
/// to add to `context! { .. }`.
fn choices_setup(plan: &Plan<'_>, db: &str) -> (String, String) {
    let mut lets = String::new();
    let mut names = String::new();
    for parent in plan.choice_tables() {
        let sg = &parent.names.singular;
        let _ = write!(lets, "\n    let {sg}_choices = {sg}_choices({db}).await?;");
        let _ = write!(names, ", {sg}_choices");
    }
    (lets, names)
}

fn html_handlers(s: &mut String, plan: &Plan<'_>) {
    let n = plan.names;
    let p = &n.pascal;
    let t = &n.plural;
    let path = &n.path;
    let (lets_ref, ctx) = choices_setup(plan, "db");
    let (lets_owned, _) = choices_setup(plan, "&db");
    s.push_str(
        "// ---- HTML (htmx) -----------------------------------------------------------\n\n",
    );
    s.push_str(
        "// A rejected form comes back as a 422 with its `form` block re-rendered: a\n\
         // message next to each field and the typed values kept.\n\n",
    );
    let _ = writeln!(
        s,
        "async fn items(db: &Db) -> Result<Vec<{p}>> {{\n    \
         let page: Paged<{p}> = listing().page_size(200, 200).fetch(db, &ListParams::default()).await?;\n    \
         Ok(page.items)\n}}\n"
    );
    let _ = writeln!(
        s,
        "async fn render(views: &Views, db: &Db, hx: &HxRequest) -> Result<Html<String>> {{\n    \
         let items = items(db).await?;{lets_ref}\n    \
         views.page_or_block(hx, \"{t}/index.html\", \"list\", context! {{ items{ctx} }})\n}}\n"
    );
    let _ = writeln!(
        s,
        "/// The {ph} page (or just its list, for htmx).\n#[get(\"{path}\", tag = \"pages\")]\n\
         async fn index_page(views: Views, db: Db, hx: HxRequest) -> Result<Html<String>> {{\n    \
         render(&views, &db, &hx).await\n}}\n",
        ph = n.plural_human,
    );
    let _ = writeln!(
        s,
        "/// Add {a} from the page's form: the new list, or the form again with\n\
         /// what is wrong.\n#[post(\"{path}\", tag = \"pages\")]\n\
         async fn create_form(\n    views: Views,\n    db: Db,\n    hx: HxRequest,\n    ValidForm(form): ValidForm<New{p}>,\n) -> Result<Response> {{\n    \
         let new = match form {{\n        Ok(new) => new,\n        Err(invalid) => {{\n            \
         let items = items(&db).await?;{lets_inner}\n            \
         return views.render_invalid(\"{t}/index.html\", context! {{ items{ctx} }}, invalid);\n        }}\n    }};\n    \
         insert(&db, &new).await?;\n    Ok(render(&views, &db, &hx).await?.into_response())\n}}\n",
        a = n.a_human,
        lets_inner = lets_owned.replace("\n    ", "\n            "),
    );
    let _ = writeln!(
        s,
        "/// One {h}'s page.\n#[get(\"{path}/{{id}}\", tag = \"pages\")]\n\
         async fn show_page(views: Views, db: Db, Path(id): Path<i64>) -> Result<Html<String>> {{\n    \
         let item = find(&db, id).await?;{lets_owned}\n    \
         views.render(\"{t}/show.html\", context! {{ item{ctx} }})\n}}\n",
        h = n.human,
    );
    let _ = writeln!(
        s,
        "/// The edit form: a whole page, or just the editable table row for htmx.\n\
         #[get(\"{path}/{{id}}/edit\", tag = \"pages\")]\n\
         async fn edit_page(\n    views: Views,\n    db: Db,\n    hx: HxRequest,\n    Path(id): Path<i64>,\n) -> Result<Html<String>> {{\n    \
         let item = find(&db, id).await?;{lets_owned}\n    \
         views.page_or_block(&hx, \"{t}/edit.html\", \"form\", context! {{ item{ctx} }})\n}}\n"
    );
    let human = title_case(&n.human);
    let (flash_arg, saved, flash_doc) = if plan.flash {
        (
            "\n    flash: Flash,",
            format!("\n    flash.success(\"{human} saved.\");"),
            ", with a flash message",
        )
    } else {
        ("", String::new(), "")
    };
    let _ = writeln!(
        s,
        "/// Save the edit form; answers with the updated row, which htmx swaps in{flash_doc},\n\
         /// or the form row again with what is wrong.\n\
         #[put(\"{path}/{{id}}\", tag = \"pages\")]\n\
         async fn update_form(\n    views: Views,{flash_arg}\n    db: Db,\n    Path(id): Path<i64>,\n    ValidForm(form): ValidForm<New{p}>,\n) -> Result<Response> {{\n    \
         let new = match form {{\n        Ok(new) => new,\n        Err(invalid) => {{\n            \
         let item = find(&db, id).await?;{lets_inner}\n            \
         return views.render_invalid(\"{t}/edit.html\", context! {{ item{ctx} }}, invalid);\n        }}\n    }};\n    \
         let item = update_row(&db, id, &new).await?;{lets_owned}{saved}\n    \
         Ok(views.render(\"{t}/_row.html\", context! {{ item{ctx} }})?.into_response())\n}}\n",
        lets_inner = lets_owned.replace("\n    ", "\n            "),
    );
    let _ = writeln!(
        s,
        "/// Delete {a} from the page.\n#[delete(\"{path}/{{id}}\", tag = \"pages\")]\n\
         async fn remove_form(views: Views, db: Db, hx: HxRequest, Path(id): Path<i64>) -> Result<Html<String>> {{\n    \
         delete_row(&db, id).await?;\n    render(&views, &db, &hx).await\n}}\n",
        a = n.a_human,
    );
}

/// The form control for `f`, with its error message after it. It shows
/// what the user typed after a rejected post (`{{ old.title }}`); with
/// `item`, it otherwise starts with that row's value (`{{ item.title }}`).
fn input(f: &Field, item: Option<&str>) -> String {
    let required = if f.optional { "" } else { " required" };
    let name = &f.name;
    let label = f.label();
    let old = format!("old.{name}");
    let field = item.map(|i| format!("{i}.{name}")).unwrap_or_default();
    // The value to show, where `shown` is how `field` is written in the
    // input. `old` is empty (falsy) until a post is rejected.
    let value = |shown: &str| match item {
        None => old.clone(),
        Some(_) if f.optional => format!("{old} if old else ({shown} if {field} is not none)"),
        Some(_) => format!("{old} if old else {shown}"),
    };
    let value_attr = |shown: &str| format!(" value=\"{{{{ {} }}}}\"", value(shown));
    let invalid = format!("{{% if errors.{name} %}} aria-invalid=\"true\"{{% endif %}}");
    let control = match f.ty {
        FieldType::String => format!(
            "<input name=\"{name}\" placeholder=\"{label}\" maxlength=\"200\"{}{required}{invalid}>",
            value_attr(&field)
        ),
        FieldType::Text => format!(
            "<textarea name=\"{name}\" placeholder=\"{label}\"{required}{invalid}>{{{{ {} }}}}</textarea>",
            value(&field)
        ),
        FieldType::Int => format!(
            "<input name=\"{name}\" type=\"number\" step=\"1\" placeholder=\"{label}\"{}{required}{invalid}>",
            value_attr(&field)
        ),
        FieldType::Float => format!(
            "<input name=\"{name}\" type=\"number\" step=\"any\" placeholder=\"{label}\"{}{required}{invalid}>",
            value_attr(&field)
        ),
        FieldType::Bool => {
            let checked = match item {
                Some(_) => format!("{old} if old else {field}"),
                None => old.clone(),
            };
            format!(
                "<label><input name=\"{name}\" type=\"checkbox\"{{{{ ' checked' if ({checked}) }}}}> {label}</label>"
            )
        }
        FieldType::Date => format!(
            "<input name=\"{name}\" type=\"date\" aria-label=\"{label}\"{}{required}{invalid}>",
            value_attr(&field)
        ),
        // Browsers want `2026-01-31T09:30`; the JSON form is `2026-01-31T09:30:00Z`.
        FieldType::DateTime => format!(
            "<input name=\"{name}\" type=\"datetime-local\" aria-label=\"{label} (UTC)\"{}{required}{invalid}>",
            value_attr(&format!("{field}[:16]"))
        ),
        FieldType::Uuid => format!(
            "<input name=\"{name}\" placeholder=\"{label}\" pattern=\"[0-9a-fA-F-]{{32,36}}\"{}{required}{invalid}>",
            value_attr(&field)
        ),
        // The stored value is shown as JSON text; `json_text` parses it back.
        FieldType::Json => format!(
            "<textarea name=\"{name}\" placeholder=\"{label} (JSON)\"{required}{invalid}>{{{{ {} }}}}</textarea>",
            value(&format!("{field}|tojson"))
        ),
        FieldType::References => {
            let parent = f.parent.as_ref().map(|p| p.names.singular.as_str()).unwrap_or("");
            // Submitted values are strings; ids are numbers.
            let selected = match item {
                Some(_) => format!(
                    "{{{{ ' selected' if ((c.id|string) == {old} if old else c.id == {field}) }}}}"
                ),
                None => format!("{{{{ ' selected' if (c.id|string) == {old} }}}}"),
            };
            format!(
                "<select name=\"{name}\" aria-label=\"{label}\"{required}{invalid}>\
                 <option value=\"\">{label}…</option>\
                 {{% for c in {parent}_choices %}}<option value=\"{{{{ c.id }}}}\"{selected}>{{{{ c.label }}}}</option>{{% endfor %}}\
                 </select>"
            )
        }
    };
    format!(
        "{control}{{% if errors.{name} %}}<small class=\"error\">{label} {{{{ errors.{name} }}}}</small>{{% endif %}}"
    )
}

/// How `item.<field>` is shown in a table cell or on the show page.
fn display(f: &Field) -> String {
    let v = format!("item.{}", f.name);
    match (&f.parent, f.ty, f.optional) {
        (Some(parent), _, _) => {
            // The parent's label, linked to its page when it has one.
            let label = format!(
                "{{% for c in {sg}_choices if c.id == {v} %}}{{{{ c.label }}}}{{% else %}}{{{{ {v} if {v} is not none }}}}{{% endfor %}}",
                sg = parent.names.singular
            );
            if parent.page {
                format!(
                    "{{% if {v} is not none %}}<a href=\"{}/{{{{ {v} }}}}\">{label}</a>{{% endif %}}",
                    parent.names.path
                )
            } else {
                label
            }
        }
        (None, FieldType::Bool, _) => format!("{{{{ '✓' if {v} }}}}"),
        (None, FieldType::Json, true) => {
            format!("{{% if {v} is not none %}}<code>{{{{ {v}|tojson }}}}</code>{{% endif %}}")
        }
        (None, FieldType::Json, false) => format!("<code>{{{{ {v}|tojson }}}}</code>"),
        (None, _, true) => format!("{{{{ {v} if {v} is not none }}}}"),
        _ => format!("{{{{ {v} }}}}"),
    }
}

fn title_case(s: &str) -> String {
    let mut t = s.to_string();
    if let Some(first) = t.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    t
}

/// The table header: one column per field, then the actions.
fn thead(plan: &Plan<'_>) -> String {
    let mut s = String::from("  <thead><tr>");
    for f in plan.fields {
        let _ = write!(s, "<th>{}</th>", f.label());
    }
    s.push_str("<th></th></tr></thead>\n");
    s
}

/// `id` of the create form, so a rejected post is swapped into the form
/// rather than into the list it targets.
fn form_id(plan: &Plan<'_>) -> String {
    format!("{}-form", plan.names.singular.replace('_', "-"))
}

fn index_template(plan: &Plan<'_>) -> String {
    let n = plan.names;
    let title = title_case(&n.plural_human);
    let mut s = String::new();
    s.push_str("{% extends \"base.html\" %}\n");
    let _ = writeln!(s, "{{% block title %}}{title}{{% endblock %}}");
    s.push_str("{% block content %}\n");
    let _ = writeln!(s, "<h1>{title}</h1>\n");
    s.push_str("{# A rejected post re-renders the `form` block here (422), not the list. #}\n");
    let _ = writeln!(
        s,
        "<form id=\"{id}\" method=\"post\" action=\"{p}\" hx-post=\"{p}\" hx-target=\"#{t}\"\n      \
         hx-on::after-request=\"if (event.detail.xhr.status < 300) {{ this.reset(); this.querySelectorAll('.error').forEach((e) => e.remove()) }}\">\n  {{% block form %}}",
        id = form_id(plan),
        p = n.path,
        t = n.plural
    );
    for f in plan.fields {
        let _ = writeln!(s, "  {}", input(f, None));
    }
    s.push_str("  <button>Add</button>\n  {% endblock %}\n</form>\n\n");
    s.push_str("<table>\n");
    s.push_str(&thead(plan));
    let _ = writeln!(s, "  <tbody id=\"{}\">", n.plural);
    s.push_str("  {% block list %}\n  {% for item in items %}\n");
    let _ = writeln!(s, "    {{% include \"{}/_row.html\" %}}", n.plural);
    let _ = writeln!(
        s,
        "  {{% else %}}\n    <tr><td colspan=\"{}\">No {} yet.</td></tr>\n  {{% endfor %}}\n  {{% endblock %}}",
        plan.fields.len() + 1,
        n.plural_human
    );
    s.push_str("  </tbody>\n</table>\n{% endblock %}\n");
    s
}

/// One row of the index table. The page includes it per item; saving the
/// edit form renders it alone.
fn row_template(plan: &Plan<'_>) -> String {
    let n = plan.names;
    let mut s = String::new();
    let _ = write!(
        s,
        "<tr id=\"{}-{{{{ item.id }}}}\">",
        n.singular.replace('_', "-")
    );
    for (i, f) in plan.fields.iter().enumerate() {
        let cell = display(f);
        // The first column links to the item's page.
        if i == 0 && f.parent.is_none() {
            let _ = write!(
                s,
                "<td><a href=\"{}/{{{{ item.id }}}}\">{cell}</a></td>",
                n.path
            );
        } else {
            let _ = write!(s, "<td>{cell}</td>");
        }
    }
    let _ = writeln!(
        s,
        "<td>\
         <button class=\"link\" hx-get=\"{p}/{{{{ item.id }}}}/edit\" hx-target=\"closest tr\" hx-swap=\"outerHTML\" aria-label=\"Edit\">✎</button>\
         <button class=\"link\" hx-delete=\"{p}/{{{{ item.id }}}}\" hx-target=\"#{t}\" aria-label=\"Delete\">✕</button>\
         </td></tr>",
        p = n.path,
        t = n.plural
    );
    s
}

fn show_template(plan: &Plan<'_>) -> String {
    let n = plan.names;
    let title = title_case(&n.human);
    let mut s = String::new();
    s.push_str("{% extends \"base.html\" %}\n");
    let _ = writeln!(
        s,
        "{{% block title %}}{title} {{{{ item.id }}}}{{% endblock %}}"
    );
    s.push_str("{% block content %}\n");
    let _ = writeln!(s, "<h1>{title} {{{{ item.id }}}}</h1>\n\n<dl>");
    for f in plan.fields {
        let style = if f.ty == FieldType::Text {
            " style=\"white-space: pre-wrap\""
        } else {
            ""
        };
        let _ = writeln!(s, "  <dt>{}</dt><dd{style}>{}</dd>", f.label(), display(f));
    }
    s.push_str("  <dt>Created</dt><dd>{{ item.created_at }}</dd>\n");
    s.push_str("  <dt>Updated</dt><dd>{{ item.updated_at }}</dd>\n</dl>\n\n");
    let _ = writeln!(
        s,
        "<p><a href=\"{p}/{{{{ item.id }}}}/edit\">Edit</a> · <a href=\"{p}\">All {ph}</a></p>",
        p = n.path,
        ph = n.plural_human
    );
    s.push_str("{% endblock %}\n");
    s
}

/// The edit form as a table row (htmx swaps it in place of the row being
/// edited), on a page of its own for direct visits. A rejected save
/// re-renders the `form` block: the row with a message per field.
fn edit_template(plan: &Plan<'_>) -> String {
    let n = plan.names;
    let mut s = String::new();
    s.push_str("{% extends \"base.html\" %}\n");
    let _ = writeln!(
        s,
        "{{% block title %}}Edit {} {{{{ item.id }}}}{{% endblock %}}",
        n.human
    );
    s.push_str("{% block content %}\n");
    let _ = writeln!(s, "<h1>Edit {} {{{{ item.id }}}}</h1>\n", n.human);
    s.push_str("<table>\n");
    s.push_str(&thead(plan));
    s.push_str("  <tbody>\n  {% block form %}\n");
    let _ = writeln!(
        s,
        "  <tr id=\"{}-{{{{ item.id }}}}\">",
        n.singular.replace('_', "-")
    );
    for f in plan.fields {
        let _ = writeln!(s, "    <td>{}</td>", input(f, Some("item")));
    }
    let _ = writeln!(
        s,
        "    <td>\
         <button hx-put=\"{p}/{{{{ item.id }}}}\" hx-include=\"closest tr\" hx-target=\"closest tr\" hx-swap=\"outerHTML\">Save</button> \
         <a href=\"{p}\">Cancel</a></td>\n  </tr>",
        p = n.path
    );
    s.push_str("  {% endblock %}\n  </tbody>\n</table>\n{% endblock %}\n");
    s
}

fn test(plan: &Plan<'_>, krate: &str) -> String {
    let n = plan.names;
    let api = api_path(plan);
    let refs: Vec<(&Field, &Parent)> = plan.references().collect();
    // A body with every field's sample value, except those in `overrides`.
    let body_with = |overrides: &[(&str, String)]| -> String {
        let pairs: Vec<String> = plan
            .fields
            .iter()
            .map(|f| {
                let v = overrides
                    .iter()
                    .find(|(name, _)| *name == f.name)
                    .map_or_else(|| f.sample_json(), |(_, v)| v.clone());
                format!("\"{}\": {v}", f.name)
            })
            .collect();
        format!("{{{}}}", pairs.join(", "))
    };
    let body = body_with(&[]);
    // The field the PATCH test changes: the first that isn't a reference.
    let patched = plan.fields.iter().find(|f| f.parent.is_none());

    // Rows the resource refers to, inserted by `Factory` with made-up values;
    // a reference to the resource's own table starts out empty.
    let factories: Vec<&(&Field, &Parent)> =
        refs.iter().filter(|(f, _)| !plan.is_self(f)).collect();
    let mut setup = String::from("    let client = app(Database::sqlite_memory()).test();\n");
    if !factories.is_empty() {
        setup.push_str(
            "    let pool: sqlx::Pool<DefaultDb> = client.resources().await.require().unwrap();\n",
        );
        for (f, parent) in &factories {
            let _ = writeln!(
                setup,
                "    let {} = Factory::new(\"{}\").create(&pool).await.unwrap().int(\"id\").unwrap();",
                f.name, parent.names.plural
            );
        }
    }
    for (f, _) in refs.iter().filter(|(f, _)| plan.is_self(f)) {
        let _ = writeln!(setup, "    let {}: Option<i64> = None;", f.name);
    }
    let create = format!(
        "    let res = client.post(\"{api}\").json(&json!({body})).send().await;\n    \
         assert_eq!(res.status(), 201, \"{{}}\", res.text());\n    \
         let created: Value = res.json();\n    \
         let id = created[\"id\"].as_i64().unwrap();\n"
    );

    let mut s = String::new();
    let _ = writeln!(
        s,
        "//! `{api}` against a fresh in-memory database per test.\n\n\
         use serde_json::{{json, Value}};\n{}\n\
         use {krate}::app;\n",
        if factories.is_empty() {
            "use zapline::db::Database;"
        } else {
            "use zapline::db::{sqlx, Database, DefaultDb, Factory};"
        }
    );
    let _ = writeln!(
        s,
        "#[tokio::test]\nasync fn crud_round_trip() {{\n{setup}\n{create}    \
         // Every column comes back as it was sent.\n    \
         let sent = json!({body});\n    \
         for (field, value) in sent.as_object().unwrap() {{\n        \
         assert_eq!(&created[field], value, \"`{{field}}` did not round-trip\");\n    }}\n    \
         assert!(created[\"created_at\"].is_string(), \"{{created}}\");\n\n    \
         let res = client.get(&format!(\"{api}/{{id}}\")).send().await;\n    \
         assert_eq!(res.status(), 200);\n    \
         assert_eq!(res.json::<Value>(), created);\n\n    \
         let list: Value = client.get(\"{api}\").send().await.json();\n    \
         assert_eq!(list[\"total\"], 1);\n    \
         assert_eq!(list[\"items\"][0], created);\n\n    \
         let res = client.put(&format!(\"{api}/{{id}}\")).json(&sent).send().await;\n    \
         assert_eq!(res.status(), 200);\n    \
         let replaced: Value = res.json();\n    \
         for (field, value) in sent.as_object().unwrap() {{\n        \
         assert_eq!(&replaced[field], value, \"`{{field}}` did not round-trip\");\n    }}\n\n    \
         let res = client.delete(&format!(\"{api}/{{id}}\")).send().await;\n    \
         assert_eq!(res.status(), 204);\n    \
         let res = client.get(&format!(\"{api}/{{id}}\")).send().await;\n    \
         assert_eq!(res.status(), 404);\n}}\n",
    );
    if let Some(changed) = patched {
        let mut keep = String::new();
        for f in plan.fields.iter().filter(|f| f.name != changed.name) {
            let _ = write!(
                keep,
                "\n    assert_eq!(patched[\"{0}\"], created[\"{0}\"]);",
                f.name
            );
        }
        let _ = writeln!(
            s,
            "#[tokio::test]\nasync fn patch_changes_only_the_given_fields() {{\n{setup}\n{create}\n    \
             let res = client\n        .patch(&format!(\"{api}/{{id}}\"))\n        .json(&json!({{\"{c}\": {v}}}))\n        .send()\n        .await;\n    \
             assert_eq!(res.status(), 200, \"{{}}\", res.text());\n    \
             let patched: Value = res.json();\n    \
             assert_eq!(patched[\"{c}\"], json!({v}));{keep}\n    \
             assert_eq!(patched[\"created_at\"], created[\"created_at\"]);\n    \
             assert!(patched[\"updated_at\"].is_string());\n\n    \
             let res = client.patch(\"{api}/999999\").json(&json!({{}})).send().await;\n    \
             assert_eq!(res.status(), 404);\n}}\n",
            c = changed.name,
            v = changed.changed_json(),
        );
    }
    if let Some(s_field) = plan
        .fields
        .iter()
        .find(|f| f.ty == FieldType::String && !f.optional)
    {
        let invalid: Vec<String> = plan
            .fields
            .iter()
            .map(|f| {
                if f.name == s_field.name {
                    format!("\"{}\": \"\"", f.name)
                } else {
                    format!("\"{}\": {}", f.name, f.sample_json())
                }
            })
            .collect();
        let _ = writeln!(
            s,
            "#[tokio::test]\nasync fn invalid_input_is_422() {{\n{setup}    \
             let res = client.post(\"{api}\").json(&json!({{{}}})).send().await;\n    \
             assert_eq!(res.status(), 422);\n}}\n",
            invalid.join(", ")
        );
    }
    for (f, parent) in plan.nested() {
        let nested = nested_path(plan, parent).replace("{id}", &format!("{{{}}}", f.name));
        let missing = nested_path(plan, parent).replace("{id}", "999999");
        if plan.is_self(f) {
            // A row under the first one; unique columns need values of their own.
            let mut child: Vec<(&str, String)> = plan
                .fields
                .iter()
                .filter(|o| o.unique && o.parent.is_none())
                .map(|o| (o.name.as_str(), o.changed_json()))
                .collect();
            child.push((f.name.as_str(), "id".to_string()));
            let _ = writeln!(
                s,
                "#[tokio::test]\nasync fn listed_under_their_{role}() {{\n{setup}\n{create}\n    \
                 let res = client.post(\"{api}\").json(&json!({child})).send().await;\n    \
                 assert_eq!(res.status(), 201, \"{{}}\", res.text());\n    \
                 let child = res.json::<Value>()[\"id\"].as_i64().unwrap();\n\n    \
                 let list: Value = client.get(&format!(\"{under}\")).send().await.json();\n    \
                 assert_eq!(list[\"total\"], 1, \"{{list}}\");\n    \
                 assert_eq!(list[\"items\"][0][\"id\"], child);\n\n    \
                 let res = client.get(\"{missing}\").send().await;\n    \
                 assert_eq!(res.status(), 404);\n}}\n",
                role = parent.role,
                child = body_with(&child),
                under = nested_path(plan, parent),
            );
            continue;
        }
        let _ = writeln!(
            s,
            "#[tokio::test]\nasync fn listed_under_their_{role}() {{\n{setup}\n{create}\n    \
             let list: Value = client.get(&format!(\"{nested}\")).send().await.json();\n    \
             assert_eq!(list[\"total\"], 1, \"{{list}}\");\n    \
             assert_eq!(list[\"items\"][0][\"id\"], id);\n\n    \
             let res = client.get(\"{missing}\").send().await;\n    \
             assert_eq!(res.status(), 404);\n}}\n",
            role = parent.role,
        );
    }
    for (f, parent) in &refs {
        let _ = writeln!(
            s,
            "#[tokio::test]\nasync fn missing_{role}_is_422() {{\n{setup}\n{create}\n    \
             // A {h} that doesn't exist can't be referred to.\n    \
             let res = client.post(\"{api}\").json(&json!({bad})).send().await;\n    \
             assert_eq!(res.status(), 422, \"{{}}\", res.text());\n    \
             assert_eq!(res.json::<Value>()[\"code\"], \"{role}_not_found\");\n    \
             let res = client\n        .patch(&format!(\"{api}/{{id}}\"))\n        \
             .json(&json!({{\"{c}\": 999999}}))\n        .send()\n        .await;\n    \
             assert_eq!(res.status(), 422, \"{{}}\", res.text());\n}}\n",
            role = parent.role,
            h = parent.role.replace('_', " "),
            c = f.name,
            bad = body_with(&[(f.name.as_str(), "999999".to_string())]),
        );
    }
    let uniques: Vec<&Field> = plan.fields.iter().filter(|f| f.unique).collect();
    // Another row needs other values in every unique column; a reference
    // would need another parent, so those are left to the reader.
    if !uniques.iter().any(|f| f.parent.is_some()) {
        let others: Vec<(&str, String)> = uniques
            .iter()
            .map(|f| (f.name.as_str(), f.changed_json()))
            .collect();
        for f in &uniques {
            let _ = writeln!(
                s,
                "#[tokio::test]\nasync fn duplicate_{c}_is_409() {{\n{setup}\n{create}\n    \
                 // A second row, then the first one's {h} again.\n    \
                 let other = json!({other});\n    \
                 let res = client.post(\"{api}\").json(&other).send().await;\n    \
                 assert_eq!(res.status(), 201, \"{{}}\", res.text());\n    \
                 let res = client.post(\"{api}\").json(&json!({body})).send().await;\n    \
                 assert_eq!(res.status(), 409, \"{{}}\", res.text());\n    \
                 assert_eq!(res.json::<Value>()[\"code\"], \"{c}_taken\");\n    \
                 let res = client.put(&format!(\"{api}/{{id}}\")).json(&other).send().await;\n    \
                 assert_eq!(res.status(), 409, \"{{}}\", res.text());\n}}\n",
                c = f.name,
                h = f.name.replace('_', " "),
                other = body_with(&others),
            );
        }
    }
    // What a browser form sends for each field: nothing for a reference to
    // the resource itself, which has no other row to point at.
    let form_value = |f: &Field| -> String {
        if plan.is_self(f) {
            "String::new()".to_string()
        } else {
            f.changed_form()
        }
    };
    if plan.html {
        let _ = writeln!(
            s,
            "#[tokio::test]\nasync fn page_renders() {{\n    \
             let client = app(Database::sqlite_memory()).test();\n    \
             let res = client.get(\"{}\").send().await;\n    \
             assert_eq!(res.status(), 200);\n    \
             assert!(res.text().contains(\"<table>\"));\n}}\n",
            n.path
        );
        let form: Vec<String> = plan
            .fields
            .iter()
            .map(|f| format!("(\"{}\", {})", f.name, form_value(f)))
            .collect();
        let shown = patched
            .map(|f| match f.ty {
                // The page shows the stored form, which can differ from the input.
                FieldType::DateTime => "2026-02-28T18:45".to_string(),
                FieldType::Bool => "✓".to_string(),
                // `tojson` writes it compactly; escaped for the Rust string.
                FieldType::Json => "{\\\"n\\\":2}".to_string(),
                _ => f.changed_json().trim_matches('"').to_string(),
            })
            .filter(|v| v != "✓");
        let shown_check = match shown {
            Some(v) => format!("\n    assert!(row.contains(\"{v}\"), \"{{row}}\");"),
            None => String::new(),
        };
        let _ = writeln!(
            s,
            "#[tokio::test]\nasync fn show_and_edit_pages() {{\n{setup}\n{create}\n    \
             let res = client.get(&format!(\"{p}/{{id}}\")).send().await;\n    \
             assert_eq!(res.status(), 200);\n    \
             assert!(res.text().contains(\"<dl>\"), \"{{}}\", res.text());\n\n    \
             // The whole page for a visit, just the editable row for htmx.\n    \
             let page = client.get(&format!(\"{p}/{{id}}/edit\")).send().await.text();\n    \
             assert!(page.contains(\"<html\") && page.contains(\"hx-put\"), \"{{page}}\");\n    \
             let res = client\n        .get(&format!(\"{p}/{{id}}/edit\"))\n        .header(\"HX-Request\", \"true\")\n        .send()\n        .await;\n    \
             assert_eq!(res.status(), 200);\n    \
             let row = res.text();\n    \
             assert!(row.trim_start().starts_with(\"<tr\") && !row.contains(\"<html\"), \"{{row}}\");\n\n    \
             // Saving answers with the updated display row.\n    \
             let res = client\n        .put(&format!(\"{p}/{{id}}\"))\n        .header(\"HX-Request\", \"true\")\n        .form(&[{form}])\n        .send()\n        .await;\n    \
             assert_eq!(res.status(), 200, \"{{}}\", res.text());{flash_check}\n    \
             let row = res.text();\n    \
             assert!(row.contains(\"hx-get\"), \"{{row}}\");{shown_check}\n\n    \
             let res = client.get(\"{p}/999999\").send().await;\n    \
             assert_eq!(res.status(), 404);\n}}\n",
            p = n.path,
            form = form.join(", "),
            flash_check = if plan.flash {
                format!(
                    "\n    // Answered in place, so the flash message comes as an htmx event.\n    \
                     let trigger = res.header(\"hx-trigger\").unwrap_or_default();\n    \
                     assert!(trigger.contains(\"{} saved.\"), \"{{trigger}}\");",
                    title_case(&n.human)
                )
            } else {
                String::new()
            },
        );
        if let Some((field, bad, message)) = breakable(plan) {
            let broken: Vec<String> = plan
                .fields
                .iter()
                .map(|f| {
                    if f.name == field.name {
                        format!("(\"{}\", \"{bad}\".to_string())", f.name)
                    } else {
                        format!("(\"{}\", {})", f.name, form_value(f))
                    }
                })
                .collect();
            let _ = writeln!(
                s,
                "#[tokio::test]\nasync fn rejected_forms_come_back_with_errors() {{\n{setup}\n    \
                 // The create form: swapped into the form, not the list it targets.\n    \
                 let res = client\n        .post(\"{p}\")\n        .header(\"HX-Request\", \"true\")\n        \
                 .header(\"HX-Trigger\", \"{id}\")\n        .header(\"HX-Target\", \"{t}\")\n        \
                 .form(&[{broken}])\n        .send()\n        .await;\n    \
                 assert_eq!(res.status(), 422);\n    \
                 assert_eq!(res.header(\"hx-retarget\"), Some(\"#{id}\"));\n    \
                 let form = res.text();\n    \
                 assert!(form.contains(\"{message}\"), \"{{form}}\");\n    \
                 assert!(form.contains(\"value=\\\"{bad}\\\"\"), \"{{form}}\");\n    \
                 assert!(!form.contains(\"<table>\"), \"{{form}}\");\n    \
                 let list: Value = client.get(\"{api}\").send().await.json();\n    \
                 assert_eq!(list[\"total\"], 0);\n\n    \
                 // The edit row: the row again, with the message.\n{create}\n    \
                 let res = client\n        .put(&format!(\"{p}/{{id}}\"))\n        .header(\"HX-Request\", \"true\")\n        \
                 .form(&[{broken}])\n        .send()\n        .await;\n    \
                 assert_eq!(res.status(), 422);\n    \
                 let row = res.text();\n    \
                 assert!(row.trim_start().starts_with(\"<tr\") && row.contains(\"{message}\"), \"{{row}}\");\n}}\n",
                p = n.path,
                t = n.plural,
                id = form_id(plan),
                broken = broken.join(", "),
            );
        }
    }
    s.trim_end().to_string() + "\n"
}

/// A field to get wrong in a form post, the bad value and the message the
/// page then shows.
fn breakable<'a>(plan: &Plan<'a>) -> Option<(&'a Field, &'static str, String)> {
    let required_string = plan
        .fields
        .iter()
        .find(|f| f.ty == FieldType::String && !f.optional);
    if let Some(f) = required_string {
        return Some((
            f,
            "",
            format!("{} length must be between 1 and 200", f.label()),
        ));
    }
    plan.fields
        .iter()
        .find(|f| matches!(f.ty, FieldType::Int | FieldType::Float))
        .map(|f| (f, "abc", format!("{} must be a number", f.label())))
}

/// The label column of a table from its `CREATE TABLE` in `sql`: the first
/// of `name`, `title`, `label`, `email`, `username`, else the first
/// required text column, else `id`.
pub fn label_column(sql: &str, table: &str) -> String {
    let columns = table_columns(sql, table);
    for preferred in ["name", "title", "label", "email", "username"] {
        if columns.iter().any(|(c, _)| c == preferred) {
            return preferred.to_string();
        }
    }
    columns
        .iter()
        .find(|(_, def)| {
            let def = def.to_ascii_uppercase();
            def.starts_with("TEXT") && def.contains("NOT NULL")
        })
        .map(|(c, _)| c.clone())
        .unwrap_or_else(|| "id".to_string())
}

/// `(column, rest of its definition)` for each column of `CREATE TABLE <table>`.
fn table_columns(sql: &str, table: &str) -> Vec<(String, String)> {
    let upper = sql.to_ascii_uppercase();
    let needle = format!("CREATE TABLE {}", table.to_ascii_uppercase());
    let Some(start) = upper
        .find(&format!("{needle} ("))
        .or_else(|| upper.find(&format!("{needle}(")))
        .or_else(|| {
            upper.find(&format!(
                "CREATE TABLE IF NOT EXISTS {} (",
                table.to_ascii_uppercase()
            ))
        })
    else {
        return Vec::new();
    };
    let Some(open) = sql[start..].find('(').map(|i| start + i + 1) else {
        return Vec::new();
    };
    // The body up to the matching parenthesis.
    let mut depth = 1;
    let mut end = open;
    for (i, c) in sql[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for c in sql[open..end].chars().chain([',']) {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if c == ',' && depth == 0 {
            let def = current.trim();
            if let Some((name, rest)) = def.split_once(char::is_whitespace) {
                let name = name.trim_matches('"').to_ascii_lowercase();
                let keyword = matches!(
                    name.as_str(),
                    "primary" | "foreign" | "unique" | "check" | "constraint"
                );
                if !keyword {
                    out.push((name, rest.trim().to_string()));
                }
            }
            current.clear();
        } else {
            current.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Style;

    #[test]
    fn a_one_way_project_gets_no_down_file() {
        let names = names();
        let fields = parse_fields(&[]).unwrap();
        let mut plan = plan(&names, &fields, false);
        plan.test_crate = None;
        plan.migration = MigrationFiles::new("1_create_blog_posts", Style::OneWay);
        let paths: Vec<String> = files(&plan)
            .iter()
            .map(|f| f.path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            paths,
            [
                "migrations/1_create_blog_posts.sql",
                "src/routes/blog_posts.rs"
            ]
        );
    }

    fn names() -> ResourceNames {
        ResourceNames::parse("blog-post", None).unwrap()
    }

    fn plan<'a>(names: &'a ResourceNames, fields: &'a [Field], html: bool) -> Plan<'a> {
        Plan {
            names,
            fields,
            dialect: Dialect::Sqlite,
            html,
            flash: html,
            test_crate: Some("demo"),
            migration: MigrationFiles::new("1_create_blog_posts", Style::Reversible),
        }
    }

    #[test]
    fn sql_reserved_words_are_rejected() {
        let err = parse_fields(&["on:bool".into()]).unwrap_err().to_string();
        assert!(err.contains("reserved word in SQL"), "{err}");
        assert!(parse_fields(&["order".into()]).is_err());
        assert!(parse_fields(&["is_on:bool".into()]).is_ok());
    }

    #[test]
    fn fields_parse_with_types_and_optionality() {
        let f = parse_fields(&["title".into(), "views:int".into(), "bio:text?".into()]).unwrap();
        assert_eq!(f[0].ty, FieldType::String);
        assert_eq!(f[1].rust_type(), "i64");
        assert_eq!(f[2].rust_type(), "Option<String>");
        assert!(Field::parse("id:int").is_err());
        assert!(Field::parse("created_at:datetime").is_err());
        assert!(Field::parse("type:string").is_err());
        assert!(Field::parse("x:money").is_err());
        assert!(parse_fields(&["a".into(), "a:int".into()]).is_err());
        assert_eq!(parse_fields(&[]).unwrap()[0].name, "name");
        assert!(parse_field_list(&[]).unwrap().is_empty());
    }

    #[test]
    fn dates_times_and_uuids_parse() {
        let f = parse_fields(&[
            "due:date".into(),
            "starts_at:datetime?".into(),
            "seen:timestamp".into(),
            "token:uuid".into(),
        ])
        .unwrap();
        assert_eq!(f[0].ty, FieldType::Date);
        assert_eq!(f[0].rust_type(), "NaiveDate");
        assert_eq!(f[1].ty, FieldType::DateTime);
        assert_eq!(f[1].rust_type(), "Option<DateTime<Utc>>");
        assert_eq!(f[2].ty, FieldType::DateTime);
        assert_eq!(f[3].rust_type(), "Uuid");
        let err = Field::parse("x:money").unwrap_err().to_string();
        assert!(
            err.contains("date, datetime, uuid, json, references"),
            "{err}"
        );
    }

    #[test]
    fn every_type_has_a_column_type_in_each_dialect() {
        use Dialect::{Postgres, Sqlite};
        let cases = [
            (FieldType::String, "TEXT", "TEXT"),
            (FieldType::Text, "TEXT", "TEXT"),
            (FieldType::Int, "BIGINT", "BIGINT"),
            (FieldType::Float, "DOUBLE PRECISION", "DOUBLE PRECISION"),
            (FieldType::Bool, "BOOLEAN", "BOOLEAN"),
            (FieldType::Date, "TEXT", "DATE"),
            (FieldType::DateTime, "TEXT", "TIMESTAMPTZ"),
            (FieldType::Uuid, "BLOB", "UUID"),
            (FieldType::Json, "TEXT", "JSONB"),
            (FieldType::References, "BIGINT", "BIGINT"),
        ];
        for (ty, sqlite, postgres) in cases {
            assert_eq!(ty.sql(Sqlite), sqlite, "{ty:?}");
            assert_eq!(ty.sql(Postgres), postgres, "{ty:?}");
        }
        // And each parses from its name and gets a test value.
        for (spec, ty) in [
            ("a:string", FieldType::String),
            ("a:text", FieldType::Text),
            ("a:int", FieldType::Int),
            ("a:float", FieldType::Float),
            ("a:bool", FieldType::Bool),
            ("a:date", FieldType::Date),
            ("a:datetime", FieldType::DateTime),
            ("a:uuid", FieldType::Uuid),
            ("a:json", FieldType::Json),
            ("item:references", FieldType::References),
        ] {
            let f = Field::parse(spec).unwrap();
            assert_eq!(f.ty, ty);
            if ty != FieldType::References {
                assert_ne!(f.sample_json(), f.changed_json(), "{spec}");
            }
        }
    }

    #[test]
    fn references_point_at_the_parent_table() {
        let f = Field::parse("author:references").unwrap();
        assert_eq!(f.name, "author_id");
        assert_eq!(f.rust_type(), "i64");
        let parent = f.parent.as_ref().unwrap();
        assert_eq!(parent.names.plural, "authors");
        assert_eq!(parent.names.path, "/authors");
        assert_eq!(
            f.column_sql(Dialect::Postgres),
            "author_id BIGINT NOT NULL REFERENCES authors(id) ON DELETE CASCADE"
        );
        // `_id` is optional; an optional parent is set to NULL when deleted.
        let f = Field::parse("blog_category_id:references?").unwrap();
        assert_eq!(f.name, "blog_category_id");
        assert_eq!(
            f.column_sql(Dialect::Sqlite),
            "blog_category_id BIGINT REFERENCES blog_categories(id) ON DELETE SET NULL"
        );
        assert_eq!(f.label(), "Blog category");
    }

    #[test]
    fn migration_matches_the_dialect() {
        let names = names();
        let fields = parse_fields(&[
            "title".into(),
            "done:bool".into(),
            "score:float?".into(),
            "author:references".into(),
        ])
        .unwrap();
        let mut plan = plan(&names, &fields, false);
        let sql = migration(&plan);
        assert!(sql.contains("CREATE TABLE blog_posts"));
        assert!(sql.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"));
        assert!(sql.contains("title TEXT NOT NULL"));
        assert!(sql.contains("done BOOLEAN NOT NULL DEFAULT FALSE"));
        assert!(sql.contains("score DOUBLE PRECISION,\n"));
        assert!(
            sql.contains("author_id BIGINT NOT NULL REFERENCES authors(id) ON DELETE CASCADE,\n")
        );
        assert!(sql.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,\n"));
        assert!(sql.contains("updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\n);"));
        assert!(
            sql.ends_with(");\nCREATE INDEX blog_posts_author_id_idx ON blog_posts (author_id);\n")
        );
        plan.dialect = Dialect::Postgres;
        let sql = migration(&plan);
        assert!(sql.contains("id BIGSERIAL PRIMARY KEY"));
        assert!(sql.contains("created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP"));
    }

    #[test]
    fn api_moves_under_api_when_there_is_a_page() {
        let names = names();
        let fields = parse_fields(&[]).unwrap();
        let plan = plan(&names, &fields, true);
        let all = files(&plan);
        let paths: Vec<String> = all
            .iter()
            .map(|f| f.path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            paths,
            [
                "migrations/1_create_blog_posts.up.sql",
                "migrations/1_create_blog_posts.down.sql",
                "src/routes/blog_posts.rs",
                "templates/blog_posts/index.html",
                "templates/blog_posts/_row.html",
                "templates/blog_posts/show.html",
                "templates/blog_posts/edit.html",
                "tests/blog_posts.rs"
            ]
        );
        assert!(all[1].contents.contains("DROP TABLE blog_posts;"));
        let module = module(&plan);
        assert!(module.contains("#[get(\"/api/blog-posts\""));
        assert!(module.contains("#[get(\"/blog-posts\", tag = \"pages\")]"));
        assert!(module.contains("#[get(\"/blog-posts/{id}\", tag = \"pages\")]"));
        assert!(module.contains("#[get(\"/blog-posts/{id}/edit\", tag = \"pages\")]"));
        assert!(module.contains("#[put(\"/blog-posts/{id}\", tag = \"pages\")]"));
    }

    #[test]
    fn json_api_only_without_templates() {
        let names = names();
        let fields = parse_fields(&["title".into(), "author:references".into()]).unwrap();
        let plan = plan(&names, &fields, false);
        let paths: Vec<String> = files(&plan)
            .iter()
            .map(|f| f.path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            paths,
            [
                "migrations/1_create_blog_posts.up.sql",
                "migrations/1_create_blog_posts.down.sql",
                "src/routes/blog_posts.rs",
                "tests/blog_posts.rs"
            ]
        );
        let module = module(&plan);
        assert!(module.contains("#[get(\"/authors/{id}/blog-posts\", tag = \"blog_posts\")]"));
        assert!(!module.contains("Views"));
        assert!(!module.contains("Choice"));
    }

    #[test]
    fn timestamps_patch_and_typed_fields_in_the_module() {
        let names = names();
        let fields = parse_fields(&[
            "title".into(),
            "published_on:date".into(),
            "starts_at:datetime?".into(),
            "token:uuid".into(),
        ])
        .unwrap();
        let module = module(&plan(&names, &fields, true));
        assert!(module.contains("use zapline::chrono::{DateTime, NaiveDate, Utc};"));
        assert!(module.contains("use zapline::uuid::Uuid;"));
        assert!(module.contains("    pub published_on: NaiveDate,\n"));
        assert!(module.contains("    pub starts_at: Option<DateTime<Utc>>,\n"));
        assert!(module
            .contains("    pub created_at: DateTime<Utc>,\n    pub updated_at: DateTime<Utc>,\n"));
        // Forms send `datetime-local` values; the helper reads those and RFC 3339.
        assert!(module.contains(
            "#[serde(default, deserialize_with = \"zapline::extract::optional_datetime\")]\n    pub starts_at: Option<DateTime<Utc>>,"
        ));
        assert!(module.contains("pub struct UpdateBlogPost {"));
        assert!(module.contains("    pub token: Option<Uuid>,\n"));
        assert!(module.contains("#[patch(\"/api/blog-posts/{id}\", tag = \"blog_posts\")]"));
        assert!(module.contains(
            "UPDATE blog_posts SET title = COALESCE($1, title), published_on = COALESCE($2, published_on), \
             starts_at = COALESCE($3, starts_at), token = COALESCE($4, token), updated_at = CURRENT_TIMESTAMP WHERE id = $5"
        ));
        assert!(module.contains(
            "UPDATE blog_posts SET title = $1, published_on = $2, starts_at = $3, token = $4, updated_at = CURRENT_TIMESTAMP WHERE id = $5"
        ));
        assert!(module.contains(".sortable([\"title\", \"published_on\", \"starts_at\", \"token\", \"created_at\", \"updated_at\"])"));
        // Only types that compare as text, numbers or booleans get filters.
        assert!(module.contains(".filter_text(\"title\")"));
        assert!(!module.contains(".filter_text(\"published_on\")"));

        let without_dates = parse_fields(&["title".into()]).unwrap();
        let small = super::module(&plan(&names, &without_dates, false));
        assert!(small.contains("use zapline::chrono::{DateTime, Utc};"));
        assert!(!small.contains("Uuid"));
    }

    #[test]
    fn references_get_a_filter_a_nested_list_and_a_select() {
        let names = ResourceNames::parse("post", None).unwrap();
        let mut fields = parse_fields(&["title".into(), "author:references".into()]).unwrap();
        if let Some(parent) = fields[1].parent.as_mut() {
            parent.label = "name".into();
            parent.page = true;
        }
        let plan = plan(&names, &fields, true);
        let module = module(&plan);
        assert!(module.contains(".filter_int(\"author_id\")"));
        assert!(module.contains("#[get(\"/api/authors/{id}/posts\", tag = \"posts\")]"));
        assert!(module.contains("async fn list_for_author("));
        assert!(module.contains("let listing = listing().scope(\"author_id\", id);"));
        assert!(module.contains("SELECT id FROM authors WHERE id = $1"));
        assert!(module.contains(
            "SELECT id, CAST(name AS TEXT) AS label FROM authors ORDER BY name LIMIT 1000"
        ));
        assert!(module.contains("context! { items, author_choices }"));
        assert!(module.contains("list_for_author,"));

        let index = index_template(&plan);
        assert!(index.contains(
            "<select name=\"author_id\" aria-label=\"Author\" required{% if errors.author_id %}"
        ));
        assert!(index.contains("{% for c in author_choices %}<option value=\"{{ c.id }}\"{{ ' selected' if (c.id|string) == old.author_id }}>"));
        assert!(index.contains("{% include \"posts/_row.html\" %}"));
        let edit = edit_template(&plan);
        assert!(edit.contains(
            "{{ ' selected' if ((c.id|string) == old.author_id if old else c.id == item.author_id) }}"
        ));
        assert!(edit.contains("hx-put=\"/posts/{{ item.id }}\" hx-include=\"closest tr\""));
        // Rows show the author's label, linked to the author's page.
        let row = row_template(&plan);
        assert!(
            row.contains("<a href=\"/authors/{{ item.author_id }}\">"),
            "{row}"
        );
        assert!(row.contains("{% for c in author_choices if c.id == item.author_id %}"));

        let test = test(&plan, "demo");
        assert!(test.contains(
            "let author_id = Factory::new(\"authors\").create(&pool).await.unwrap().int(\"id\").unwrap();"
        ));
        assert!(test.contains("async fn listed_under_their_author()"));
        assert!(test.contains("client.get(&format!(\"/api/authors/{author_id}/posts\"))"));
        assert!(test.contains("client.get(\"/api/authors/999999/posts\")"));
        assert!(test.contains("\"author_id\": 999999"));
    }

    #[test]
    fn many_references_still_mount() {
        let names = names();
        let specs: Vec<String> = (0..12).map(|i| format!("p{i}:references")).collect();
        let fields = parse_fields(&specs).unwrap();
        let module = module(&plan(&names, &fields, true));
        // 6 JSON + 12 nested + 6 pages = 24 handlers: two tuples.
        assert_eq!(module.matches(".mount((").count(), 2, "{module}");
    }

    #[test]
    fn inputs_follow_the_field_type() {
        let f = |spec: &str| Field::parse(spec).unwrap();
        assert!(input(&f("d:date"), None).contains("type=\"date\""));
        assert!(input(&f("t:datetime"), None).contains("type=\"datetime-local\""));
        assert!(input(&f("t:datetime?"), Some("item")).starts_with(
            "<input name=\"t\" type=\"datetime-local\" aria-label=\"T (UTC)\" \
             value=\"{{ old.t if old else (item.t[:16] if item.t is not none) }}\"\
             {% if errors.t %} aria-invalid=\"true\"{% endif %}>"
        ));
        assert!(input(&f("n:int"), Some("item"))
            .contains("value=\"{{ old.n if old else item.n }}\" required"));
        assert!(input(&f("b:bool"), Some("item"))
            .contains("{{ ' checked' if (old.b if old else item.b) }}"));
        assert!(input(&f("body:text"), Some("item")).starts_with(
            "<textarea name=\"body\" placeholder=\"Body\" required\
             {% if errors.body %} aria-invalid=\"true\"{% endif %}>{{ old.body if old else item.body }}</textarea>"
        ));
        // A rejected create form keeps what was typed and says what is wrong.
        assert_eq!(
            input(&f("title"), None),
            "<input name=\"title\" placeholder=\"Title\" maxlength=\"200\" value=\"{{ old.title }}\" required\
             {% if errors.title %} aria-invalid=\"true\"{% endif %}>\
             {% if errors.title %}<small class=\"error\">Title {{ errors.title }}</small>{% endif %}"
        );
        assert!(input(&f("k:uuid"), None).contains("pattern="));
    }

    #[test]
    fn generated_tests_cover_patch_pages_and_nested_lists() {
        let names = names();
        let fields = parse_fields(&["title".into(), "views:int".into()]).unwrap();
        let test = test(&plan(&names, &fields, true), "demo");
        assert!(test.contains("async fn crud_round_trip()"));
        assert!(test.contains("async fn patch_changes_only_the_given_fields()"));
        assert!(test.contains(".json(&json!({\"title\": \"Changed\"}))"));
        assert!(test.contains("assert_eq!(patched[\"views\"], created[\"views\"]);"));
        assert!(test.contains("async fn invalid_input_is_422()"));
        assert!(test.contains("async fn page_renders()"));
        assert!(test.contains("async fn show_and_edit_pages()"));
        assert!(test.contains(".header(\"HX-Request\", \"true\")"));
        assert!(test.contains(
            ".form(&[(\"title\", \"Changed\".to_string()), (\"views\", \"7\".to_string())])"
        ));
        assert!(!test.contains("Factory"));
    }

    #[test]
    fn rejected_forms_show_errors_and_saves_flash() {
        let names = names();
        let fields =
            parse_fields(&["title".into(), "done:bool".into(), "score:float?".into()]).unwrap();
        let mut plan = plan(&names, &fields, true);
        let index = index_template(&plan);
        assert!(
            index.contains("<form id=\"blog-post-form\" method=\"post\""),
            "{index}"
        );
        assert!(index.contains("{% block form %}"));
        let edit = edit_template(&plan);
        assert!(edit.contains("{% block form %}"), "{edit}");
        assert!(edit.contains(
            "value=\"{{ old.score if old else (item.score if item.score is not none) }}\""
        ));

        let src = module(&plan);
        assert!(src.contains("ValidForm(form): ValidForm<NewBlogPost>"));
        assert!(src.contains("views.render_invalid(\"blog_posts/index.html\""));
        assert!(src.contains("views.render_invalid(\"blog_posts/edit.html\""));
        assert!(src.contains("views.page_or_block(&hx, \"blog_posts/edit.html\", \"form\""));
        assert!(src.contains("flash.success(\"Blog post saved.\");"));
        let test = test(&plan, "demo");
        assert!(test.contains("async fn rejected_forms_come_back_with_errors()"));
        assert!(test.contains("Title length must be between 1 and 200"));
        assert!(test.contains("Blog post saved."));

        // Without sessions: no flash.
        plan.flash = false;
        let src = module(&plan);
        assert!(!src.contains("Flash"), "{src}");
        assert!(!super::test(&plan, "demo").contains("saved."));
    }

    #[test]
    fn the_label_column_comes_from_the_parent_migration() {
        let sql = "-- x\nCREATE TABLE authors (\n    id INTEGER PRIMARY KEY AUTOINCREMENT,\n    \
                   bio TEXT,\n    handle TEXT NOT NULL,\n    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\n);\n";
        assert_eq!(label_column(sql, "authors"), "handle");
        assert_eq!(
            label_column(
                "CREATE TABLE authors (id BIGSERIAL PRIMARY KEY, email TEXT, name TEXT)",
                "authors"
            ),
            "name"
        );
        assert_eq!(
            label_column(
                "CREATE TABLE authors (id INTEGER PRIMARY KEY, n BIGINT NOT NULL)",
                "authors"
            ),
            "id"
        );
        assert_eq!(
            label_column("CREATE TABLE other (name TEXT)", "authors"),
            "id"
        );
        let cols = table_columns(
            "CREATE TABLE t (a NUMERIC(10, 2) NOT NULL, b TEXT, PRIMARY KEY (a))",
            "t",
        );
        assert_eq!(
            cols,
            [
                ("a".to_string(), "NUMERIC(10, 2) NOT NULL".to_string()),
                ("b".to_string(), "TEXT".to_string())
            ]
        );
    }

    #[test]
    fn modifiers_and_custom_tables_parse_and_round_trip() {
        let f = Field::parse("email:string:unique").unwrap();
        assert!(f.unique && !f.index && !f.optional);
        assert_eq!(f.spec(), "email:string:unique");
        let f = Field::parse("sku:string?:index").unwrap();
        assert!(f.index && f.optional && !f.unique);
        assert_eq!(f.spec(), "sku:string:index?");
        assert_eq!(Field::parse(&f.spec()).unwrap(), f);
        let f = Field::parse("rating:int:index").unwrap();
        assert_eq!(f.rust_type(), "i64");

        // `author:ref:users`: the column and the codes say `author`, the
        // foreign key and the `<select>` use `users`.
        let f = Field::parse("author:ref:users?").unwrap();
        assert_eq!(f.name, "author_id");
        assert!(f.optional);
        let p = f.parent.as_ref().unwrap();
        assert_eq!(
            (p.role.as_str(), p.names.plural.as_str()),
            ("author", "users")
        );
        assert_eq!(p.names.singular, "user");
        assert_eq!(f.label(), "Author");
        assert_eq!(
            f.column_sql(Dialect::Postgres),
            "author_id BIGINT REFERENCES users(id) ON DELETE SET NULL"
        );
        assert_eq!(f.spec(), "author:references:users?");
        assert_eq!(Field::parse(&f.spec()).unwrap(), f);
        // The default table is not repeated.
        assert_eq!(
            Field::parse("author:ref:authors").unwrap().spec(),
            "author:references"
        );

        let err = |spec: &str| Field::parse(spec).unwrap_err().to_string();
        assert!(err("done:bool:unique").contains("can't be unique"));
        assert!(err("x:string:sorted").contains("unknown modifier `sorted`"));
        assert!(err("a:ref:users:oops").contains("unknown modifier `oops`"));
        assert!(err("author:ref:user").contains("reserved word in SQL"));

        // The recipe line keeps them, so `g migration` regenerates the same code.
        let names = names();
        let fields =
            parse_fields(&["slug:string:unique".into(), "editor:ref:users?".into()]).unwrap();
        let line = recipe(&names, &fields);
        assert_eq!(
            line,
            "// zapline:resource blog_post slug:string:unique editor:references:users?"
        );
        let (_, back) = parse_recipe(&line).unwrap().unwrap();
        assert_eq!(back, fields);
    }

    #[test]
    fn unique_and_index_columns_get_indexes() {
        let fields = parse_fields(&[
            "email:string:unique".into(),
            "rating:int:index".into(),
            "author:references".into(),
        ])
        .unwrap();
        let sql = create_table_sql("reviews", &fields, Dialect::Postgres);
        assert!(sql.contains("    email TEXT NOT NULL,\n"), "{sql}");
        assert!(sql.ends_with(
            ");\n\
             CREATE UNIQUE INDEX reviews_email_key ON reviews (email);\n\
             CREATE INDEX reviews_rating_idx ON reviews (rating);\n\
             CREATE INDEX reviews_author_id_idx ON reviews (author_id);\n"
        ));
        assert_eq!(
            fields[0].drop_index_sql("reviews", true),
            "DROP INDEX IF EXISTS reviews_email_key;\n"
        );
        assert_eq!(
            fields[1].drop_index_sql("reviews", false),
            "DROP INDEX reviews_rating_idx;\n"
        );
        assert_eq!(
            Field::parse("x:int").unwrap().drop_index_sql("t", false),
            ""
        );
    }

    #[test]
    fn json_fields_bind_a_value_and_take_a_textarea() {
        let names = names();
        let fields =
            parse_fields(&["title".into(), "meta:json".into(), "extra:json?".into()]).unwrap();
        assert_eq!(fields[1].rust_type(), "JsonValue");
        assert_eq!(
            fields[1].column_sql(Dialect::Postgres),
            "meta JSONB NOT NULL"
        );
        assert_eq!(fields[2].column_sql(Dialect::Sqlite), "extra TEXT");
        let plan = plan(&names, &fields, true);
        let module = module(&plan);
        assert!(module.contains("use zapline::db::sqlx::types::JsonValue;"));
        assert!(module.contains(
            "    #[serde(deserialize_with = \"zapline::extract::json_text\")]\n    pub meta: JsonValue,\n"
        ));
        assert!(module.contains(
            "    #[serde(default, deserialize_with = \"zapline::extract::json_text\")]\n    pub extra: Option<JsonValue>,\n"
        ));
        assert!(module.contains("    pub meta: Option<JsonValue>,\n"));
        assert!(module.contains(".bind(&new.meta)"));
        // Not sortable or filterable.
        assert!(module.contains(".sortable([\"title\", \"created_at\", \"updated_at\"])"));
        assert!(!module.contains("filter_text(\"meta\")"));

        let edit = edit_template(&plan);
        assert!(edit.contains(
            "<textarea name=\"meta\" placeholder=\"Meta (JSON)\" required{% if errors.meta %} aria-invalid=\"true\"{% endif %}>\
             {{ old.meta if old else item.meta|tojson }}</textarea>"
        ), "{edit}");
        assert!(edit
            .contains("{{ old.extra if old else (item.extra|tojson if item.extra is not none) }}"));
        let row = row_template(&plan);
        assert!(
            row.contains("<td><code>{{ item.meta|tojson }}</code></td>"),
            "{row}"
        );
        assert!(row.contains(
            "{% if item.extra is not none %}<code>{{ item.extra|tojson }}</code>{% endif %}"
        ));

        let test = test(&plan, "demo");
        assert!(
            test.contains("\"meta\": {\"tags\": [\"a\", \"b\"], \"n\": 1}"),
            "{test}"
        );
        assert!(
            test.contains("(\"meta\", \"{\\\"n\\\": 2}\".to_string())"),
            "{test}"
        );
        // Without references or unique columns, writes keep the plain `?`.
        assert!(!module.contains("write_error"));
    }

    #[test]
    fn missing_parents_and_duplicates_get_named_errors() {
        let names = ResourceNames::parse("review", None).unwrap();
        let fields = parse_fields(&[
            "slug:string:unique".into(),
            "product:references".into(),
            "rating:int:index".into(),
        ])
        .unwrap();
        let plan = plan(&names, &fields, false);
        let module = module(&plan);
        assert!(module.contains(
            "async fn write_error(db: &Db, err: sqlx::Error, parents: [Option<i64>; 1]) -> Error {"
        ), "{module}");
        assert!(module.contains(
            "    if e.is_foreign_key_violation() {\n        let [product_id] = parents;\n        \
             if missing(db, \"products\", product_id).await {\n            \
             return Error::unprocessable(\"Product not found\").with_code(\"product_not_found\");\n        }\n    }\n"
        ), "{module}");
        assert!(module.contains(
            "    if e.is_unique_violation()\n        \
             && (e.constraint() == Some(\"reviews_slug_key\") || e.message().ends_with(\": reviews.slug\"))\n    {\n        \
             return Error::conflict(\"Slug is already taken\").with_code(\"slug_taken\");\n    }\n"
        ));
        // Every write goes through it; a PATCH passes what it was given.
        assert_eq!(
            module
                .matches("Err(err) => Err(write_error(db, err, [")
                .count(),
            3
        );
        assert!(module.contains("write_error(db, err, [Some(new.product_id)])"));
        assert!(module.contains("write_error(db, err, [changes.product_id])"));
        assert!(
            module.contains("async fn missing(db: &Db, table: &str, id: Option<i64>) -> bool {")
        );

        let test = test(&plan, "demo");
        assert!(test.contains("async fn missing_product_is_422()"), "{test}");
        assert!(test.contains("assert_eq!(res.json::<Value>()[\"code\"], \"product_not_found\");"));
        assert!(test.contains(".json(&json!({\"product_id\": 999999}))"));
        assert!(!test.contains("409, \"{}\", res.text());\n}\n\n#[tokio::test]\nasync fn missing"));
        assert!(test.contains("async fn duplicate_slug_is_409()"));
        assert!(test.contains("let other = json!({\"slug\": \"Changed\", \"product_id\": product_id, \"rating\": 42});"), "{test}");
        assert!(test.contains("assert_eq!(res.json::<Value>()[\"code\"], \"slug_taken\");"));
        assert!(test.contains("async fn listed_under_their_product()"));

        // Only unique columns: a plain function, no parent lookups.
        let fields = parse_fields(&["email:string:unique".into()]).unwrap();
        let module = super::module(&tests::plan(&names, &fields, false));
        assert!(module.contains("fn write_error(err: sqlx::Error) -> Error {"));
        assert!(module.contains(".await\n    .map_err(write_error)?;"));
        assert!(!module.contains("async fn missing"));
    }

    #[test]
    fn two_references_to_one_table_get_no_nested_route() {
        let names = ResourceNames::parse("post", None).unwrap();
        let fields = parse_fields(&[
            "title".into(),
            "author:ref:users".into(),
            "editor:ref:users?".into(),
            "category:references".into(),
        ])
        .unwrap();
        assert_eq!(
            ambiguous_references(&fields),
            [(
                "users".to_string(),
                vec!["author_id".to_string(), "editor_id".to_string()]
            )]
        );
        let plan = plan(&names, &fields, true);
        let module = module(&plan);
        assert!(!module.contains("list_for_author"), "{module}");
        assert!(!module.contains("list_for_editor"));
        assert!(module.contains("async fn list_for_category("));
        // One `<select>` list per table, shared by both fields.
        assert_eq!(module.matches("async fn user_choices(").count(), 1);
        assert!(module.contains("context! { items, user_choices, category_choices }"));
        let edit = edit_template(&plan);
        assert!(
            edit.contains(
                "<select name=\"editor_id\" aria-label=\"Editor\"{% if errors.editor_id %}"
            ),
            "{edit}"
        );
        assert_eq!(edit.matches("{% for c in user_choices %}").count(), 2);
        let test = test(&plan, "demo");
        assert!(test.contains("let author_id = Factory::new(\"users\")"));
        assert!(test.contains("let editor_id = Factory::new(\"users\")"));
        assert!(test.contains("async fn missing_author_is_422()"));
        assert!(test.contains("async fn missing_editor_is_422()"));
        assert!(!test.contains("listed_under_their_author"));
        assert!(test.contains("async fn listed_under_their_category()"));
    }

    #[test]
    fn an_optional_reference_to_the_own_table_lists_children() {
        let names = ResourceNames::parse("category", None).unwrap();
        let mut fields = parse_fields(&[
            "name".into(),
            "slug:string:unique".into(),
            "parent:ref:categories?".into(),
        ])
        .unwrap();
        if let Some(p) = fields[2].parent.as_mut() {
            p.label = "name".into();
            p.page = true;
        }
        let plan = plan(&names, &fields, true);
        let sql = create_table_sql("categories", &fields, Dialect::Sqlite);
        assert!(sql.contains("parent_id BIGINT REFERENCES categories(id) ON DELETE SET NULL"));
        let module = module(&plan);
        assert!(
            module.contains("#[get(\"/api/categories/{id}/categories\", tag = \"categories\")]")
        );
        assert!(module.contains("async fn list_for_parent("));
        assert!(module.contains("FROM categories ORDER BY name"));
        let test = test(&plan, "demo");
        assert!(
            test.contains("    let parent_id: Option<i64> = None;\n"),
            "{test}"
        );
        assert!(!test.contains("Factory"));
        assert!(test.contains("use zapline::db::Database;"));
        // A child of the first row, with a slug of its own.
        assert!(
            test.contains(
                ".json(&json!({\"name\": \"Example\", \"slug\": \"Changed\", \"parent_id\": id}))"
            ),
            "{test}"
        );
        assert!(test.contains("client.get(&format!(\"/api/categories/{id}/categories\"))"));
        assert!(test.contains("async fn missing_parent_is_422()"));
        // Forms send no parent.
        assert!(test.contains("(\"parent_id\", String::new())"));
    }

    #[test]
    fn the_label_field_prefers_names_then_required_text() {
        let f = |specs: &[&str]| {
            label_field(
                &parse_field_list(&specs.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                    .unwrap(),
            )
        };
        assert_eq!(f(&["body:text", "title"]), "title");
        assert_eq!(f(&["bio:text?", "handle", "n:int"]), "handle");
        assert_eq!(f(&["n:int", "due:date"]), "id");
    }
}
