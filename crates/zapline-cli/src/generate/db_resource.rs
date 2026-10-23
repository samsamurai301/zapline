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
    /// The app has sessions, so the pages can set flash messages.
    pub flash: bool,
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
        out.push(OutputFile {
            path: PathBuf::from(format!("templates/{}/edit.html", n.plural)),
            contents: edit_template(plan),
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
        handlers.extend([
            "index_page",
            "create_form",
            "edit_page",
            "update_form",
            "remove_form",
        ]);
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
        "async fn fetch(db: &Db, id: i64) -> Result<{p}> {{\n    \
         let row = sqlx::query_as::<_, {p}>(\"SELECT {select_cols} FROM {t} WHERE id = $1\")\n        \
         .bind(id)\n        .fetch_one(&**db)\n        .await?;\n    Ok(row)\n}}\n",
        p = n.pascal,
        t = n.plural,
    );
    let _ = writeln!(
        s,
        "async fn update(db: &Db, id: i64, new: &New{p}) -> Result<{p}> {{\n    \
         let row = sqlx::query_as::<_, {p}>(\n        \"UPDATE {t} SET {sets} WHERE id = ${id_param} RETURNING {select_cols}\",\n    ){binds}\n    .bind(id)\n    .fetch_one(&**db)\n    .await?;\n    Ok(row)\n}}\n",
        p = n.pascal,
        t = n.plural,
        sets = sets.join(", "),
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
         Ok(Json(fetch(&db, id).await?))\n}}\n",
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
         Ok(Json(update(&db, id, &new).await?))\n}}\n",
        a = n.a_human,
        p = n.pascal,
        t = n.plural,
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
        s.push_str(
            "// Forms post with htmx (and work without JavaScript). A rejected form comes\n\
             // back as a 422 with its `form` block re-rendered: a message next to each\n\
             // field and the typed values kept.\n\n",
        );
        let _ = writeln!(
            s,
            "async fn items(db: &Db) -> Result<Vec<{p}>> {{\n    \
             let page: Paged<{p}> = listing().page_size(200, 200).fetch(db, &ListParams::default()).await?;\n    \
             Ok(page.items)\n}}\n",
            p = n.pascal,
        );
        let _ = writeln!(
            s,
            "async fn render(views: &Views, db: &Db, hx: &HxRequest) -> Result<Html<String>> {{\n    \
             let items = items(db).await?;\n    \
             views.page_or_block(hx, \"{t}/index.html\", \"list\", context! {{ items }})\n}}\n",
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
            "/// Add {a} from the page's form: the new list, or the form again with\n\
             /// what is wrong.\n#[post(\"{path}\", tag = \"pages\")]\n\
             async fn create_form(\n    views: Views,\n    db: Db,\n    hx: HxRequest,\n    ValidForm(form): ValidForm<New{p}>,\n) -> Result<Response> {{\n    \
             let new = match form {{\n        Ok(new) => new,\n        Err(invalid) => {{\n            \
             let items = items(&db).await?;\n            \
             return views.render_invalid(\"{t}/index.html\", context! {{ items }}, invalid);\n        }}\n    }};\n    \
             insert(&db, &new).await?;\n    Ok(render(&views, &db, &hx).await?.into_response())\n}}\n",
            a = n.a_human,
            p = n.pascal,
            t = n.plural,
            path = n.path,
        );
        let _ = writeln!(
            s,
            "/// The form to edit {a}.\n#[get(\"{path}/{{id}}/edit\", tag = \"pages\")]\n\
             async fn edit_page(views: Views, db: Db, Path(id): Path<i64>) -> Result<Html<String>> {{\n    \
             let item = fetch(&db, id).await?;\n    \
             views.render(\"{t}/edit.html\", context! {{ item }})\n}}\n",
            a = n.a_human,
            t = n.plural,
            path = n.path,
        );
        let human = capitalized(&n.human);
        let (flash_arg, hx_arg, done) = if plan.flash {
            (
                "\n    flash: Flash,",
                "",
                format!(
                    "flash.success(\"{human} saved.\");\n    Ok(flash.redirect(\"{}\"))",
                    n.path
                ),
            )
        } else {
            (
                "",
                "\n    hx: HxRequest,",
                format!(
                    "if hx.htmx {{\n        Ok(zapline::view::Hx::redirect(\"{p}\").into_response())\n    }} else {{\n        Ok(Redirect::to(\"{p}\").into_response())\n    }}",
                    p = n.path
                ),
            )
        };
        let _ = writeln!(
            s,
            "/// Save the edit form: back to the list{note}, or the form again\n\
             /// with what is wrong.\n#[post(\"{path}/{{id}}\", tag = \"pages\")]\n\
             async fn update_form(\n    views: Views,{flash_arg}{hx_arg}\n    db: Db,\n    Path(id): Path<i64>,\n    ValidForm(form): ValidForm<New{p}>,\n) -> Result<Response> {{\n    \
             let item = fetch(&db, id).await?;\n    \
             let new = match form {{\n        Ok(new) => new,\n        \
             Err(invalid) => return views.render_invalid(\"{t}/edit.html\", context! {{ item }}, invalid),\n    }};\n    \
             update(&db, id, &new).await?;\n    {done}\n}}\n",
            note = if plan.flash { " with a flash message" } else { "" },
            p = n.pascal,
            t = n.plural,
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

/// Where a form input gets its value from.
#[derive(Clone, Copy)]
enum Prefill {
    /// The create form: what the user typed, after a rejected post.
    Old,
    /// The edit form: what the user typed, or else the stored row.
    OldOrItem,
}

impl Prefill {
    /// A MiniJinja expression for the value of `f`.
    fn value(self, f: &Field) -> String {
        let n = &f.name;
        match (self, f.optional) {
            (Prefill::Old, _) => format!("old.{n}"),
            // `old` is empty (falsy) until a post is rejected.
            (Prefill::OldOrItem, false) => format!("old.{n} if old else item.{n}"),
            (Prefill::OldOrItem, true) => {
                format!("old.{n} if old else (item.{n} if item.{n} is not none)")
            }
        }
    }
}

/// The input for `f`, with its error message under it.
fn input(f: &Field, prefill: Prefill) -> String {
    let required = if f.optional { "" } else { " required" };
    let (n, l) = (&f.name, f.label());
    let value = prefill.value(f);
    let invalid = format!("{{% if errors.{n} %}} aria-invalid=\"true\"{{% endif %}}");
    let attr_value = format!("value=\"{{{{ {value} }}}}\"");
    let field = match f.ty {
        FieldType::String => format!(
            "<input name=\"{n}\" placeholder=\"{l}\" aria-label=\"{l}\" maxlength=\"200\" {attr_value}{required}{invalid}>"
        ),
        FieldType::Text => format!(
            "<textarea name=\"{n}\" placeholder=\"{l}\" aria-label=\"{l}\"{required}{invalid}>{{{{ {value} }}}}</textarea>"
        ),
        FieldType::Int => format!(
            "<input name=\"{n}\" type=\"number\" step=\"1\" placeholder=\"{l}\" aria-label=\"{l}\" {attr_value}{required}{invalid}>"
        ),
        FieldType::Float => format!(
            "<input name=\"{n}\" type=\"number\" step=\"any\" placeholder=\"{l}\" aria-label=\"{l}\" {attr_value}{required}{invalid}>"
        ),
        FieldType::Bool => format!(
            "<label><input name=\"{n}\" type=\"checkbox\"{{% if ({value}) %}} checked{{% endif %}}> {l}</label>"
        ),
    };
    format!(
        "{field}\n  {{% if errors.{n} %}}<small class=\"error\">{l} {{{{ errors.{n} }}}}</small>{{% endif %}}"
    )
}

/// `id` of the create form, so a rejected post is swapped into the form
/// rather than into the list it targets.
fn form_id(plan: &Plan<'_>) -> String {
    format!("{}-form", plan.names.singular.replace('_', "-"))
}

fn capitalized(s: &str) -> String {
    let mut out = s.to_string();
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
}

fn edit_template(plan: &Plan<'_>) -> String {
    let n = plan.names;
    let title = format!("Edit {}", n.human);
    let mut s = String::new();
    s.push_str("{% extends \"base.html\" %}\n");
    let _ = writeln!(s, "{{% block title %}}{title}{{% endblock %}}");
    s.push_str("{% block content %}\n");
    let _ = writeln!(s, "<h1>{title}</h1>\n");
    let _ = writeln!(
        s,
        "<form method=\"post\" action=\"{p}/{{{{ item.id }}}}\" hx-post=\"{p}/{{{{ item.id }}}}\">\n  {{% block form %}}",
        p = n.path,
    );
    for f in plan.fields {
        let _ = writeln!(s, "  {}", input(f, Prefill::OldOrItem));
    }
    let _ = writeln!(
        s,
        "  <button>Save</button> <a href=\"{}\">Cancel</a>\n  {{% endblock %}}\n</form>\n{{% endblock %}}",
        n.path
    );
    s
}

fn template(plan: &Plan<'_>) -> String {
    let n = plan.names;
    let title = capitalized(&n.plural_human);
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
        let _ = writeln!(s, "  {}", input(f, Prefill::Old));
    }
    s.push_str("  <button>Add</button>\n  {% endblock %}\n</form>\n\n");
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
        "<td><a href=\"{p}/{{{{ item.id }}}}/edit\">Edit</a> <button class=\"link\" hx-delete=\"{p}/{{{{ item.id }}}}\" hx-target=\"#{t}\" aria-label=\"Delete\">✕</button></td></tr>",
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
        html_tests(plan, &mut s);
    }
    s.trim_end().to_string() + "\n"
}

/// A value for an HTML form post.
fn sample_form(f: &Field) -> &'static str {
    match f.ty {
        FieldType::String => "Example",
        FieldType::Text => "Some longer text.",
        FieldType::Int => "42",
        FieldType::Float => "1.5",
        FieldType::Bool => "on",
    }
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

/// `&[("title", "Example"), ...]`, with `broken` set to `bad`.
fn form_pairs(plan: &Plan<'_>, broken: Option<(&str, &str)>) -> String {
    let pairs: Vec<String> = plan
        .fields
        .iter()
        .map(|f| {
            let value = match broken {
                Some((name, bad)) if name == f.name => bad,
                _ => sample_form(f),
            };
            format!("(\"{}\", \"{value}\")", f.name)
        })
        .collect();
    format!("&[{}]", pairs.join(", "))
}

/// Tests for the pages: a rejected htmx post comes back with the messages
/// in the form, and the edit form saves (with a flash, given sessions).
fn html_tests(plan: &Plan<'_>, s: &mut String) {
    let n = plan.names;
    let api = api_path(plan);
    let body: Vec<String> = plan
        .fields
        .iter()
        .map(|f| format!("\"{}\": {}", f.name, f.sample_json()))
        .collect();
    let body = format!("{{{}}}", body.join(", "));
    let id = form_id(plan);
    if let Some((field, bad, message)) = breakable(plan) {
        let pairs = form_pairs(plan, Some((&field.name, bad)));
        let _ = writeln!(
            s,
            "#[tokio::test]\nasync fn invalid_htmx_post_re_renders_the_form_with_errors() {{\n    \
             let client = app(Database::sqlite_memory()).test();\n    \
             let res = client\n        .post(\"{path}\")\n        .header(\"hx-request\", \"true\")\n        \
             .header(\"hx-trigger\", \"{id}\")\n        .header(\"hx-target\", \"{t}\")\n        \
             .form({pairs})\n        .send()\n        .await;\n    \
             assert_eq!(res.status(), 422);\n    \
             // Swapped into the form, not the list it targets.\n    \
             assert_eq!(res.header(\"hx-retarget\"), Some(\"#{id}\"));\n    \
             let form = res.text();\n    \
             assert!(form.contains(\"{message}\"), \"{{form}}\");\n    \
             assert!(form.contains(\"value=\\\"{bad}\\\"\"), \"{{form}}\");\n    \
             assert!(!form.contains(\"<table>\"), \"{{form}}\");\n    \
             let list: Value = client.get(\"{api}\").send().await.json();\n    \
             assert_eq!(list[\"total\"], 0);\n}}\n",
            path = n.path,
            t = n.plural,
        );
    }
    let mut edit = format!(
        "#[tokio::test]\nasync fn edit_form_saves_or_shows_the_errors() {{\n    \
         let client = app(Database::sqlite_memory()).test().with_cookies();\n    \
         let created: Value = client.post(\"{api}\").json(&json!({body})).send().await.json();\n    \
         let id = created[\"id\"].as_i64().unwrap();\n    \
         let page = client.get(&format!(\"{path}/{{id}}/edit\")).send().await;\n    \
         assert_eq!(page.status(), 200);\n    \
         assert!(page.text().contains(\"{sample}\"), \"{{}}\", page.text());\n\n",
        path = n.path,
        sample = sample_form(&plan.fields[0]),
    );
    if let Some((field, bad, message)) = breakable(plan) {
        let pairs = form_pairs(plan, Some((&field.name, bad)));
        let _ = write!(
            edit,
            "    let res = client.post(&format!(\"{path}/{{id}}\")).form({pairs}).send().await;\n    \
             assert_eq!(res.status(), 422);\n    \
             assert!(res.text().contains(\"{message}\"), \"{{}}\", res.text());\n\n",
            path = n.path,
        );
    }
    let pairs = form_pairs(plan, None);
    let _ = write!(
        edit,
        "    let res = client.post(&format!(\"{path}/{{id}}\")).form({pairs}).send().await;\n    \
         assert_eq!(res.status(), 303);\n    \
         assert_eq!(res.header(\"location\"), Some(\"{path}\"));\n",
        path = n.path,
    );
    if plan.flash {
        let _ = write!(
            edit,
            "    // The list page shows the flash message once.\n    \
             let html = client.get(\"{path}\").send().await.text();\n    \
             assert!(html.contains(\"{human} saved.\"), \"{{html}}\");\n    \
             let html = client.get(\"{path}\").send().await.text();\n    \
             assert!(!html.contains(\"{human} saved.\"), \"{{html}}\");\n",
            path = n.path,
            human = capitalized(&n.human),
        );
    }
    edit.push_str("}\n");
    s.push_str(&edit);
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
            flash: false,
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
            flash: true,
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
                "templates/blog_posts/edit.html",
                "tests/blog_posts.rs"
            ]
        );
        let module = module(&plan);
        assert!(module.contains("#[get(\"/api/blog-posts\""));
        assert!(module.contains("#[get(\"/blog-posts\", tag = \"pages\")]"));
    }

    #[test]
    fn forms_show_errors_and_old_values_and_edits_flash() {
        let names = names();
        let fields =
            parse_fields(&["title".into(), "done:bool".into(), "score:float?".into()]).unwrap();
        let mut plan = Plan {
            names: &names,
            fields: &fields,
            dialect: Dialect::Sqlite,
            html: true,
            flash: true,
            test_crate: Some("demo"),
            migration_name: "1_create_blog_posts.sql".into(),
        };
        let index = template(&plan);
        assert!(index.contains("<form id=\"blog-post-form\""), "{index}");
        assert!(index.contains("value=\"{{ old.title }}\""), "{index}");
        assert!(index.contains(
            "{% if errors.title %}<small class=\"error\">Title {{ errors.title }}</small>{% endif %}"
        ));
        assert!(
            index.contains("{% if (old.done) %} checked{% endif %}"),
            "{index}"
        );
        let edit = edit_template(&plan);
        assert!(
            edit.contains("value=\"{{ old.title if old else item.title }}\""),
            "{edit}"
        );
        assert!(edit.contains(
            "value=\"{{ old.score if old else (item.score if item.score is not none) }}\""
        ));
        assert!(edit.contains("{% block form %}"));

        let src = module(&plan);
        assert!(src.contains("ValidForm(form): ValidForm<NewBlogPost>"));
        assert!(src.contains("views.render_invalid(\"blog_posts/index.html\""));
        assert!(src.contains("flash.success(\"Blog post saved.\");"));
        let test = test(&plan, "demo");
        assert!(test.contains("async fn invalid_htmx_post_re_renders_the_form_with_errors()"));
        assert!(test.contains("Title length must be between 1 and 200"));
        assert!(test.contains("Blog post saved."));

        // Without sessions: a plain redirect, no flash.
        plan.flash = false;
        let src = module(&plan);
        assert!(!src.contains("Flash"));
        assert!(src.contains("Redirect::to(\"/blog-posts\")"));
    }
}
