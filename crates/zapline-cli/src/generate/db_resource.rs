//! `zapline generate resource <name> field:type ...` in a project with a
//! database: a SQL migration, a sqlx-backed CRUD module (JSON API, plus an
//! htmx page when the project has templates) and tests.
//!
//! The code is built here rather than from a `.tmpl` file because its shape
//! depends on the fields; it is formatted with rustfmt after writing.

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::naming::{is_keyword, snake_case, ResourceNames};
use crate::render::OutputFile;

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
}

impl FieldType {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "string" | "str" => FieldType::String,
            "text" => FieldType::Text,
            "int" | "integer" | "i64" => FieldType::Int,
            "float" | "f64" | "number" => FieldType::Float,
            "bool" | "boolean" => FieldType::Bool,
            other => bail!(
                "unknown field type `{other}`; use one of: string, text, int, float, bool \
                 (add `?` for optional, e.g. `bio:text?`)"
            ),
        })
    }

    fn rust(self) -> &'static str {
        match self {
            FieldType::String | FieldType::Text => "String",
            FieldType::Int => "i64",
            FieldType::Float => "f64",
            FieldType::Bool => "bool",
        }
    }

    fn sql(self) -> &'static str {
        match self {
            FieldType::String | FieldType::Text => "TEXT",
            FieldType::Int => "BIGINT",
            FieldType::Float => "DOUBLE PRECISION",
            FieldType::Bool => "BOOLEAN",
        }
    }
}

/// One `name:type` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Column and Rust field name (snake_case).
    pub name: String,
    /// Its type.
    pub ty: FieldType,
    /// `name:type?`: nullable column, `Option<T>` field.
    pub optional: bool,
}

impl Field {
    /// Parses `title`, `title:string`, `bio:text?`.
    pub fn parse(spec: &str) -> Result<Field> {
        let (name, ty) = spec.split_once(':').unwrap_or((spec, "string"));
        let (ty, optional) = match ty.strip_suffix('?') {
            Some(t) => (t, true),
            None => (ty, false),
        };
        let name = snake_case(name);
        if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
            bail!("`{spec}`: field names must start with a letter");
        }
        if is_keyword(&name) {
            bail!("`{name}` is a Rust keyword; pick another field name");
        }
        if name == "id" {
            bail!("`id` is added automatically");
        }
        if SQL_RESERVED.contains(&name.as_str()) {
            bail!(
                "`{name}` is a reserved word in SQL; pick another field name (e.g. `is_{name}` or `{name}_value`)"
            );
        }
        let ty = FieldType::parse(&ty.to_ascii_lowercase())?;
        // A missing checkbox already means `false`.
        let optional = optional && ty != FieldType::Bool;
        Ok(Field { name, ty, optional })
    }

    fn rust_type(&self) -> String {
        if self.optional {
            format!("Option<{}>", self.ty.rust())
        } else {
            self.ty.rust().to_string()
        }
    }

    fn label(&self) -> String {
        let mut words = self.name.replace('_', " ");
        if let Some(first) = words.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        words
    }

    /// A value for tests, as JSON.
    fn sample_json(&self) -> &'static str {
        match self.ty {
            FieldType::String => "\"Example\"",
            FieldType::Text => "\"Some longer text.\"",
            FieldType::Int => "42",
            FieldType::Float => "1.5",
            FieldType::Bool => "true",
        }
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

/// Parses all field arguments; defaults to `name:string` when there are none.
pub fn parse_fields(specs: &[String]) -> Result<Vec<Field>> {
    if specs.is_empty() {
        return Ok(vec![Field::parse("name:string")?]);
    }
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
    /// Columns besides `id`.
    pub fields: &'a [Field],
    /// SQL dialect of the migration.
    pub dialect: Dialect,
    /// Generate an htmx page (the project has `templates/base.html`).
    pub html: bool,
    /// Crate name for the test's imports, when a test can be generated.
    pub test_crate: Option<&'a str>,
    /// Migration file name (`<timestamp>_create_<plural>.sql`).
    pub migration_name: String,
}

/// The migration, the module, the template and the test.
pub fn files(plan: &Plan<'_>) -> Vec<OutputFile> {
    let n = plan.names;
    let mut out = vec![
        OutputFile {
            path: PathBuf::from(format!("migrations/{}", plan.migration_name)),
            contents: migration(plan),
        },
        OutputFile {
            path: PathBuf::from(format!("src/routes/{}.rs", n.plural)),
            contents: module(plan),
        },
    ];
    if plan.html {
        out.push(OutputFile {
            path: PathBuf::from(format!("templates/{}/index.html", n.plural)),
            contents: template(plan),
        });
    }
    if let Some(krate) = plan.test_crate {
        out.push(OutputFile {
            path: PathBuf::from(format!("tests/{}.rs", n.plural)),
            contents: test(plan, krate),
        });
    }
    out
}

fn migration(plan: &Plan<'_>) -> String {
    let id = match plan.dialect {
        Dialect::Sqlite => "id INTEGER PRIMARY KEY AUTOINCREMENT",
        Dialect::Postgres => "id BIGSERIAL PRIMARY KEY",
    };
    let mut cols = vec![format!("    {id}")];
    for f in plan.fields {
        let null = if f.optional { "" } else { " NOT NULL" };
        let default = if f.ty == FieldType::Bool {
            " DEFAULT FALSE"
        } else {
            ""
        };
        cols.push(format!("    {} {}{null}{default}", f.name, f.ty.sql()));
    }
    format!(
        "-- Created by `zapline generate resource {}`.\nCREATE TABLE {} (\n{}\n);\n",
        plan.names.singular,
        plan.names.plural,
        cols.join(",\n")
    )
}

/// `/api/posts` when there is an HTML page at `/posts`, otherwise `/posts`.
fn api_path(plan: &Plan<'_>) -> String {
    if plan.html {
        format!("/api{}", plan.names.path)
    } else {
        plan.names.path.clone()
    }
}

fn module(plan: &Plan<'_>) -> String {
    let n = plan.names;
    let api = api_path(plan);
    let cols: Vec<&str> = plan.fields.iter().map(|f| f.name.as_str()).collect();
    let select_cols = format!("id, {}", cols.join(", "));
    let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
    let sets: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c} = ${}", i + 1))
        .collect();
    let id_param = cols.len() + 1;
    let binds: String = plan
        .fields
        .iter()
        .map(|f| {
            // Strings are borrowed; numbers and booleans are Copy.
            let borrow = matches!(f.ty, FieldType::String | FieldType::Text);
            format!(
                "\n    .bind({}new.{})",
                if borrow { "&" } else { "" },
                f.name
            )
        })
        .collect();

    let mut s = String::new();
    let _ = writeln!(
        s,
        "//! `{}` resource: SQL-backed CRUD. JSON API under `{api}`{}.",
        n.pascal,
        if plan.html {
            format!(", HTML page at `{}`", n.path)
        } else {
            String::new()
        }
    );
    let _ = writeln!(s, "//!");
    let _ = writeln!(
        s,
        "//! Table `{}` is created by the migration generated alongside this file.",
        n.plural
    );
    s.push('\n');
    s.push_str("use zapline::db::{sqlx, Db, ListParams, Listing, Paged};\n");
    s.push_str("use zapline::prelude::*;\n");
    if plan.html {
        s.push_str("use zapline::view::{context, HxRequest, Views};\n");
    }
    s.push('\n');

    // Model.
    let _ = writeln!(s, "/// A stored {}.", n.human);
    s.push_str("#[model]\n#[derive(sqlx::FromRow)]\n");
    let _ = writeln!(s, "pub struct {} {{", n.pascal);
    s.push_str("    pub id: i64,\n");
    for f in plan.fields {
        let _ = writeln!(s, "    pub {}: {},", f.name, f.rust_type());
    }
    s.push_str("}\n\n");

    // Input.
    let _ = writeln!(
        s,
        "/// Payload for creating or replacing {} (JSON or an HTML form).",
        n.a_human
    );
    s.push_str("#[model]\n");
    let _ = writeln!(s, "pub struct New{} {{", n.pascal);
    for f in plan.fields {
        match (f.ty, f.optional) {
            (FieldType::Bool, _) => s.push_str(
                "    #[serde(default, deserialize_with = \"zapline::extract::checkbox\")]\n",
            ),
            (_, true) => s.push_str(
                "    #[serde(default, deserialize_with = \"zapline::extract::empty_as_none\")]\n",
            ),
            (FieldType::String, false) => {
                s.push_str("    #[validate(length(min = 1, max = 200))]\n");
            }
            (FieldType::Text, false) => s.push_str("    #[validate(length(max = 100000))]\n"),
            _ => {}
        }
        let _ = writeln!(s, "    pub {}: {},", f.name, f.rust_type());
    }
    s.push_str("}\n\n");

    // What the list endpoint accepts.
    let sortable: Vec<String> = plan
        .fields
        .iter()
        .filter(|f| f.ty != FieldType::Text)
        .map(|f| format!("\"{}\"", f.name))
        .collect();
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
        t = n.plural,
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
            FieldType::Int => "filter_int",
            FieldType::Bool => "filter_bool",
            FieldType::String => "filter_text",
            FieldType::Text | FieldType::Float => continue,
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
    let mut handlers = vec!["list", "get_one", "create", "replace", "remove"];
    if plan.html {
        handlers.extend(["index_page", "create_form", "remove_form"]);
    }
    let _ = writeln!(s, "/// Adds the {} routes.", n.human);
    let _ = writeln!(
        s,
        "pub fn mount(app: App) -> App {{\n    app.mount(({}))\n}}\n",
        handlers.join(", ")
    );

    // Queries.
    s.push_str(
        "// ---- queries ---------------------------------------------------------------\n\n",
    );
    let _ = writeln!(
        s,
        "async fn insert(db: &Db, new: &New{p}) -> Result<{p}> {{\n    \
         let row = sqlx::query_as::<_, {p}>(\n        \"INSERT INTO {t} ({cols}) VALUES ({vals}) RETURNING {select_cols}\",\n    ){binds}\n    .fetch_one(&**db)\n    .await?;\n    Ok(row)\n}}\n",
        p = n.pascal,
        t = n.plural,
        cols = cols.join(", "),
        vals = placeholders.join(", "),
    );
    let _ = writeln!(
        s,
        "async fn delete_row(db: &Db, id: i64) -> Result<()> {{\n    \
         let done = sqlx::query(\"DELETE FROM {t} WHERE id = $1\")\n        .bind(id)\n        .execute(&**db)\n        .await?;\n    \
         if done.rows_affected() == 0 {{\n        return Err(Error::not_found());\n    }}\n    Ok(())\n}}\n",
        t = n.plural,
    );

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
        p = n.pascal,
        t = n.plural,
    );
    let _ = writeln!(
        s,
        "/// Fetch one {h}.\n#[get(\"{api}/{{id}}\", tag = \"{t}\")]\n\
         async fn get_one(db: Db, Path(id): Path<i64>) -> Result<Json<{p}>> {{\n    \
         let row = sqlx::query_as::<_, {p}>(\"SELECT {select_cols} FROM {t} WHERE id = $1\")\n        \
         .bind(id)\n        .fetch_one(&*db)\n        .await?;\n    Ok(Json(row))\n}}\n",
        h = n.human,
        p = n.pascal,
        t = n.plural,
    );
    let _ = writeln!(
        s,
        "/// Create {a}.\n#[post(\"{api}\", tag = \"{t}\", status = 201)]\n\
         async fn create(db: Db, Valid(Json(new)): Valid<Json<New{p}>>) -> Result<(StatusCode, Json<{p}>)> {{\n    \
         Ok((StatusCode::CREATED, Json(insert(&db, &new).await?)))\n}}\n",
        a = n.a_human,
        p = n.pascal,
        t = n.plural,
    );
    let _ = writeln!(
        s,
        "/// Replace {a}.\n#[put(\"{api}/{{id}}\", tag = \"{t}\")]\n\
         async fn replace(\n    db: Db,\n    Path(id): Path<i64>,\n    Valid(Json(new)): Valid<Json<New{p}>>,\n) -> Result<Json<{p}>> {{\n    \
         let row = sqlx::query_as::<_, {p}>(\n        \"UPDATE {t} SET {sets} WHERE id = ${id_param} RETURNING {select_cols}\",\n    ){binds}\n    .bind(id)\n    .fetch_one(&*db)\n    .await?;\n    Ok(Json(row))\n}}\n",
        a = n.a_human,
        p = n.pascal,
        t = n.plural,
        sets = sets.join(", "),
    );
    let _ = writeln!(
        s,
        "/// Delete {a}.\n#[delete(\"{api}/{{id}}\", tag = \"{t}\", status = 204)]\n\
         async fn remove(db: Db, Path(id): Path<i64>) -> Result<StatusCode> {{\n    \
         delete_row(&db, id).await?;\n    Ok(StatusCode::NO_CONTENT)\n}}\n",
        a = n.a_human,
        t = n.plural,
    );

    if plan.html {
        s.push_str(
            "// ---- HTML (htmx) -----------------------------------------------------------\n\n",
        );
        let _ = writeln!(
            s,
            "async fn render(views: &Views, db: &Db, hx: &HxRequest) -> Result<Html<String>> {{\n    \
             let page: Paged<{p}> = listing().page_size(200, 200).fetch(db, &ListParams::default()).await?;\n    \
             let items = page.items;\n    \
             views.page_or_block(hx, \"{t}/index.html\", \"list\", context! {{ items }})\n}}\n",
            p = n.pascal,
            t = n.plural,
        );
        let _ = writeln!(
            s,
            "/// The {ph} page (or just its list, for htmx).\n#[get(\"{path}\", tag = \"pages\")]\n\
             async fn index_page(views: Views, db: Db, hx: HxRequest) -> Result<Html<String>> {{\n    \
             render(&views, &db, &hx).await\n}}\n",
            ph = n.plural_human,
            path = n.path,
        );
        let _ = writeln!(
            s,
            "/// Add {a} from the page's form.\n#[post(\"{path}\", tag = \"pages\")]\n\
             async fn create_form(\n    views: Views,\n    db: Db,\n    hx: HxRequest,\n    Valid(Form(new)): Valid<Form<New{p}>>,\n) -> Result<Html<String>> {{\n    \
             insert(&db, &new).await?;\n    render(&views, &db, &hx).await\n}}\n",
            a = n.a_human,
            p = n.pascal,
            path = n.path,
        );
        let _ = writeln!(
            s,
            "/// Delete {a} from the page.\n#[delete(\"{path}/{{id}}\", tag = \"pages\")]\n\
             async fn remove_form(views: Views, db: Db, hx: HxRequest, Path(id): Path<i64>) -> Result<Html<String>> {{\n    \
             delete_row(&db, id).await?;\n    render(&views, &db, &hx).await\n}}\n",
            a = n.a_human,
            path = n.path,
        );
    }
    s.trim_end().to_string() + "\n"
}

fn input(f: &Field) -> String {
    let required = if f.optional { "" } else { " required" };
    match f.ty {
        FieldType::String => format!(
            "<input name=\"{n}\" placeholder=\"{l}\" maxlength=\"200\"{required}>",
            n = f.name,
            l = f.label()
        ),
        FieldType::Text => format!(
            "<textarea name=\"{n}\" placeholder=\"{l}\"{required}></textarea>",
            n = f.name,
            l = f.label()
        ),
        FieldType::Int => format!(
            "<input name=\"{n}\" type=\"number\" step=\"1\" placeholder=\"{l}\"{required}>",
            n = f.name,
            l = f.label()
        ),
        FieldType::Float => format!(
            "<input name=\"{n}\" type=\"number\" step=\"any\" placeholder=\"{l}\"{required}>",
            n = f.name,
            l = f.label()
        ),
        FieldType::Bool => format!(
            "<label><input name=\"{n}\" type=\"checkbox\"> {l}</label>",
            n = f.name,
            l = f.label()
        ),
    }
}

fn template(plan: &Plan<'_>) -> String {
    let n = plan.names;
    let title = {
        let mut t = n.plural_human.clone();
        if let Some(first) = t.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        t
    };
    let mut s = String::new();
    s.push_str("{% extends \"base.html\" %}\n");
    let _ = writeln!(s, "{{% block title %}}{title}{{% endblock %}}");
    s.push_str("{% block content %}\n");
    let _ = writeln!(s, "<h1>{title}</h1>\n");
    let _ = writeln!(
        s,
        "<form hx-post=\"{p}\" hx-target=\"#{t}\" hx-on::after-request=\"if (event.detail.successful) this.reset()\">",
        p = n.path,
        t = n.plural
    );
    for f in plan.fields {
        let _ = writeln!(s, "  {}", input(f));
    }
    s.push_str("  <button>Add</button>\n</form>\n\n");
    s.push_str("<table>\n  <thead><tr>");
    for f in plan.fields {
        let _ = write!(s, "<th>{}</th>", f.label());
    }
    s.push_str("<th></th></tr></thead>\n");
    let _ = writeln!(s, "  <tbody id=\"{}\">", n.plural);
    s.push_str("  {% block list %}\n  {% for item in items %}\n    <tr>");
    for f in plan.fields {
        let cell = match (f.ty, f.optional) {
            (FieldType::Bool, _) => format!("{{{{ '✓' if item.{} }}}}", f.name),
            (_, true) => format!("{{{{ item.{} if item.{} is not none }}}}", f.name, f.name),
            _ => format!("{{{{ item.{} }}}}", f.name),
        };
        let _ = write!(s, "<td>{cell}</td>");
    }
    let _ = writeln!(
        s,
        "<td><button class=\"link\" hx-delete=\"{p}/{{{{ item.id }}}}\" hx-target=\"#{t}\" aria-label=\"Delete\">✕</button></td></tr>",
        p = n.path,
        t = n.plural
    );
    let _ = writeln!(
        s,
        "  {{% else %}}\n    <tr><td colspan=\"{}\">No {} yet.</td></tr>\n  {{% endfor %}}\n  {{% endblock %}}",
        plan.fields.len() + 1,
        n.plural_human
    );
    s.push_str("  </tbody>\n</table>\n{% endblock %}\n");
    s
}

fn test(plan: &Plan<'_>, krate: &str) -> String {
    let n = plan.names;
    let api = api_path(plan);
    let body: Vec<String> = plan
        .fields
        .iter()
        .map(|f| format!("\"{}\": {}", f.name, f.sample_json()))
        .collect();
    let body = format!("{{{}}}", body.join(", "));
    let first = &plan.fields[0];
    let mut s = String::new();
    let _ = writeln!(
        s,
        "//! `{api}` against a fresh in-memory database per test.\n\n\
         use serde_json::{{json, Value}};\nuse zapline::db::Database;\n\n\
         use {krate}::app;\n"
    );
    let _ = writeln!(
        s,
        "#[tokio::test]\nasync fn crud_round_trip() {{\n    \
         let client = app(Database::sqlite_memory()).test();\n\n    \
         let res = client.post(\"{api}\").json(&json!({body})).send().await;\n    \
         assert_eq!(res.status(), 201, \"{{}}\", res.text());\n    \
         let created: Value = res.json();\n    \
         let id = created[\"id\"].as_i64().unwrap();\n\n    \
         let res = client.get(&format!(\"{api}/{{id}}\")).send().await;\n    \
         assert_eq!(res.status(), 200);\n    \
         assert_eq!(res.json::<Value>()[\"{f}\"], created[\"{f}\"]);\n\n    \
         let list: Value = client.get(\"{api}\").send().await.json();\n    \
         assert_eq!(list[\"total\"], 1);\n    \
         assert_eq!(list[\"items\"][0][\"id\"], id);\n\n    \
         let res = client.put(&format!(\"{api}/{{id}}\")).json(&json!({body})).send().await;\n    \
         assert_eq!(res.status(), 200);\n\n    \
         let res = client.delete(&format!(\"{api}/{{id}}\")).send().await;\n    \
         assert_eq!(res.status(), 204);\n    \
         let res = client.get(&format!(\"{api}/{{id}}\")).send().await;\n    \
         assert_eq!(res.status(), 404);\n}}\n",
        f = first.name,
    );
    if plan
        .fields
        .iter()
        .any(|f| f.ty == FieldType::String && !f.optional)
    {
        let s_field = plan
            .fields
            .iter()
            .find(|f| f.ty == FieldType::String && !f.optional)
            .map(|f| f.name.as_str())
            .unwrap_or("name");
        let invalid: Vec<String> = plan
            .fields
            .iter()
            .map(|f| {
                if f.name == s_field {
                    format!("\"{}\": \"\"", f.name)
                } else {
                    format!("\"{}\": {}", f.name, f.sample_json())
                }
            })
            .collect();
        let _ = writeln!(
            s,
            "#[tokio::test]\nasync fn invalid_input_is_422() {{\n    \
             let client = app(Database::sqlite_memory()).test();\n    \
             let res = client.post(\"{api}\").json(&json!({{{}}})).send().await;\n    \
             assert_eq!(res.status(), 422);\n}}\n",
            invalid.join(", ")
        );
    }
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
    }
    s.trim_end().to_string() + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> ResourceNames {
        ResourceNames::parse("blog-post", None).unwrap()
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
        assert!(Field::parse("type:string").is_err());
        assert!(Field::parse("x:date").is_err());
        assert!(parse_fields(&["a".into(), "a:int".into()]).is_err());
        assert_eq!(parse_fields(&[]).unwrap()[0].name, "name");
    }

    #[test]
    fn migration_matches_the_dialect() {
        let names = names();
        let fields =
            parse_fields(&["title".into(), "done:bool".into(), "score:float?".into()]).unwrap();
        let mut plan = Plan {
            names: &names,
            fields: &fields,
            dialect: Dialect::Sqlite,
            html: false,
            test_crate: None,
            migration_name: "1_create_blog_posts.sql".into(),
        };
        let sql = migration(&plan);
        assert!(sql.contains("CREATE TABLE blog_posts"));
        assert!(sql.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"));
        assert!(sql.contains("title TEXT NOT NULL"));
        assert!(sql.contains("done BOOLEAN NOT NULL DEFAULT FALSE"));
        assert!(sql.contains("score DOUBLE PRECISION\n"));
        plan.dialect = Dialect::Postgres;
        assert!(migration(&plan).contains("id BIGSERIAL PRIMARY KEY"));
    }

    #[test]
    fn api_moves_under_api_when_there_is_a_page() {
        let names = names();
        let fields = parse_fields(&[]).unwrap();
        let plan = Plan {
            names: &names,
            fields: &fields,
            dialect: Dialect::Sqlite,
            html: true,
            test_crate: Some("demo"),
            migration_name: "1_create_blog_posts.sql".into(),
        };
        let paths: Vec<String> = files(&plan)
            .iter()
            .map(|f| f.path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            paths,
            [
                "migrations/1_create_blog_posts.sql",
                "src/routes/blog_posts.rs",
                "templates/blog_posts/index.html",
                "tests/blog_posts.rs"
            ]
        );
        let module = module(&plan);
        assert!(module.contains("#[get(\"/api/blog-posts\""));
        assert!(module.contains("#[get(\"/blog-posts\", tag = \"pages\")]"));
    }
}
