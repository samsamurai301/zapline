//! An admin panel for the app's database: every table, browsable, searchable
//! and sortable, with forms to create, edit and delete rows. No code per
//! table: it reads the schema, so new migrations show up on the next reload.
//!
//! ```no_run
//! use zapline::db::Database;
//! use zapline::prelude::*;
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .database(Database::from_env())
//!         .admin(|admin| admin) // at /admin, debug builds and this machine only
//!         .run("127.0.0.1:8080")
//! }
//! ```
//!
//! Without [`Admin::protect`], the panel behaves like the
//! [dev console](crate::dev): it exists only in debug builds and answers
//! only requests from this machine. To use it in production, protect it
//! with a middleware that decides who is an admin:
//!
//! ```no_run
//! use zapline::db::Database;
//! use zapline::middleware::Next;
//! use zapline::prelude::*;
//!
//! async fn admins_only(req: Request, next: Next) -> Response {
//!     let is_admin = req.headers().get("x-admin-token").is_some_and(|t| t == "s3cret");
//!     if !is_admin {
//!         return Error::forbidden().into_response();
//!     }
//!     next.run(req).await
//! }
//!
//! let app = App::new()
//!     .database(Database::from_env())
//!     .admin(|admin| admin.protect(admins_only).hide(["users.email"]));
//! ```
//!
//! What it shows: tables whose names are plain identifiers (letters, digits,
//! `_`), except the framework's own (`_sqlx_migrations`, `zapline_sessions`,
//! `zapline_jobs`) and those passed to [`Admin::exclude`]. Columns that look
//! like secrets (names containing `password`, `secret` or `token`, or ending
//! in `_hash`) are never shown or written, and neither are those passed to
//! [`Admin::hide`]. Rows are edited by their primary key; tables without a
//! single-column primary key are read-only.

use std::fmt::Write as _;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use sqlx::{Pool, QueryBuilder, Row};

use crate::db::{DefaultDb, ListParams};
use crate::dev::{escape, internal, local, DevPanel};
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::response::{Html, IntoResponse, Redirect, Response};
use crate::router::Routes;
use crate::{App, Error, Request, Result, StatusCode};

/// Tables the framework manages, never shown.
const INTERNAL_TABLES: &[&str] = &[
    "_sqlx_migrations",
    "zapline_sessions",
    "zapline_jobs",
    "sqlite_sequence",
];

const PAGE_SIZE: i64 = 50;

/// How the admin panel is set up: see [`App::admin`].
pub struct Admin {
    path: String,
    guard: Option<Arc<dyn Middleware>>,
    exclude: Vec<String>,
    hide: Vec<String>,
    read_only: bool,
}

impl Default for Admin {
    fn default() -> Self {
        Admin {
            path: "/admin".to_owned(),
            guard: None,
            exclude: Vec::new(),
            hide: Vec::new(),
            read_only: false,
        }
    }
}

impl Admin {
    /// Serve the panel under `path` instead of `/admin`.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// let app = App::new().admin(|admin| admin.path("/backoffice"));
    /// ```
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.path = format!("/{}", path.trim_matches('/'));
        self
    }

    /// Put `middleware` in front of every admin route, and serve the panel
    /// in release builds and to other machines too. The middleware decides
    /// who gets in, typically by checking the logged-in user.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # use zapline::middleware::Next;
    /// async fn admins_only(req: Request, next: Next) -> Response {
    ///     # let is_admin = false;
    ///     if !is_admin { return Error::forbidden().into_response(); }
    ///     next.run(req).await
    /// }
    /// let app = App::new().admin(|admin| admin.protect(admins_only));
    /// ```
    #[must_use]
    pub fn protect(mut self, middleware: impl Middleware) -> Self {
        self.guard = Some(Arc::new(middleware));
        self
    }

    /// Leave these tables out.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// let app = App::new().admin(|admin| admin.exclude(["password_resets", "audit_log"]));
    /// ```
    #[must_use]
    pub fn exclude<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exclude.extend(tables.into_iter().map(Into::into));
        self
    }

    /// Never show or write these columns: `"table.column"`, or `"column"`
    /// for every table. Secret-looking columns are hidden already.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// let app = App::new().admin(|admin| admin.hide(["users.email", "ip_address"]));
    /// ```
    #[must_use]
    pub fn hide<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.hide.extend(columns.into_iter().map(Into::into));
        self
    }

    /// Browse and search only: no create, edit or delete.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// let app = App::new().admin(|admin| admin.read_only());
    /// ```
    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    fn hidden(&self, table: &str, column: &str) -> bool {
        let lower = column.to_ascii_lowercase();
        ["password", "secret", "token"]
            .iter()
            .any(|s| lower.contains(s))
            || lower.ends_with("_hash")
            || self
                .hide
                .iter()
                .any(|h| h == column || h.split_once('.') == Some((table, column)))
    }

    fn shown(&self, table: &str) -> bool {
        is_ident(table)
            && !INTERNAL_TABLES.contains(&table)
            && !self.exclude.iter().any(|t| t == table)
    }
}

impl App {
    /// Serve an [admin panel](crate::admin) for every table in the
    /// database, at `/admin`. Unless [protected](Admin::protect), only in
    /// debug builds and only to this machine. Calling it again replaces the
    /// earlier setup (so a plugin can protect an admin the app turned on).
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # use zapline::db::Database;
    /// let app = App::new()
    ///     .database(Database::from_env())
    ///     .admin(|admin| admin.exclude(["password_resets"]));
    /// ```
    pub fn admin(self, configure: impl FnOnce(Admin) -> Admin) -> Self {
        self.admin_in::<DefaultDb>(configure)
    }

    /// [`admin`](Self::admin) for a database other than [`DefaultDb`] (for
    /// apps with both backends enabled).
    pub fn admin_in<DB: AdminBackend>(mut self, configure: impl FnOnce(Admin) -> Admin) -> Self {
        let admin = configure(Admin::default());
        let first = self.extensions().get::<Slot>().is_none();
        let slot = self
            .extensions_mut()
            .get_or_insert_default::<Slot>()
            .clone();
        {
            let mut slot = slot.lock();
            if admin.guard.is_some() || cfg!(debug_assertions) {
                slot.path = Some(admin.path.clone());
                slot.build = Some(Box::new(move || routes::<DB>(Arc::new(admin))));
            } else {
                *slot = Pending::default();
            }
        }
        if !first {
            return self;
        }
        let panel = slot.clone();
        self.on_build(move |_| {
            let build = slot.lock().build.take();
            build.map_or_else(Routes::new, |build| build())
        })
        .dev_panel(DevPanel::new("admin", "Admin", move |_: &Request| {
            let html = match &panel.lock().path {
                Some(path) => format!(
                    "<p><a href=\"{0}\">{0}</a>: browse and edit every table.</p>",
                    escape(path)
                ),
                None => "<p class=dim>Off.</p>".to_owned(),
            };
            Box::pin(async move { html }) as BoxFuture<String>
        }))
    }
}

/// The admin to mount once routes are final: the last [`App::admin`] call wins.
#[derive(Clone, Default)]
struct Slot(Arc<Mutex<Pending>>);

#[derive(Default)]
struct Pending {
    path: Option<String>,
    build: Option<Box<dyn FnOnce() -> Routes + Send>>,
}

impl Slot {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone, Copy)]
enum Action {
    Index,
    List,
    New,
    Create,
    Edit,
    Update,
    Delete,
}

fn routes<DB: AdminBackend>(admin: Arc<Admin>) -> Routes {
    let handler = |action: Action| {
        let admin = admin.clone();
        move |req: Request| {
            let admin = admin.clone();
            async move { serve::<DB>(req, &admin, action).await }
        }
    };
    let p = if admin.path == "/" {
        ""
    } else {
        admin.path.as_str()
    };
    let mut routes = Routes::new()
        .get(if p.is_empty() { "/" } else { p }, handler(Action::Index))
        .get(&format!("{p}/{{table}}"), handler(Action::List))
        .get(&format!("{p}/{{table}}/{{id}}"), handler(Action::Edit));
    if !admin.read_only {
        routes = routes
            .get(&format!("{p}/{{table}}/new"), handler(Action::New))
            .post(&format!("{p}/{{table}}"), handler(Action::Create))
            .post(&format!("{p}/{{table}}/{{id}}"), handler(Action::Update))
            .post(
                &format!("{p}/{{table}}/{{id}}/delete"),
                handler(Action::Delete),
            );
    }
    if let Some(guard) = &admin.guard {
        let guard = guard.clone();
        routes = routes.wrap(move |req: Request, next: Next| guard.handle(req, next));
    }
    internal(routes)
}

async fn serve<DB: AdminBackend>(mut req: Request, admin: &Admin, action: Action) -> Response {
    if admin.guard.is_none() && !local(&req) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if req.method() == http::Method::POST && !same_origin(&req) {
        return Error::forbidden().with_code("cross_origin").into_response();
    }
    let Some(pool) = req.state::<Pool<DB>>().cloned() else {
        return Error::internal(format!(
            "the admin needs a {} database: call `.database(..)`",
            DB::NAME
        ))
        .into_response();
    };
    match run::<DB>(&mut req, admin, &pool, action).await {
        Ok(res) => res,
        Err(e) => e.into_response(),
    }
}

/// Form posts must come from this site (on top of `SameSite` cookies).
fn same_origin(req: &Request) -> bool {
    let header = |name| req.headers().get(name).and_then(|v| v.to_str().ok());
    match (header(http::header::ORIGIN), header(http::header::HOST)) {
        (None, _) => true,
        (Some(origin), Some(host)) => origin.split_once("://").map(|(_, h)| h) == Some(host),
        (Some(_), None) => false,
    }
}

async fn run<DB: AdminBackend>(
    req: &mut Request,
    admin: &Admin,
    pool: &Pool<DB>,
    action: Action,
) -> Result<Response> {
    let mut tables = DB::tables(pool).await?;
    tables.retain(|t| admin.shown(t));
    let ui = Ui {
        admin,
        tables: &tables,
    };
    if let Action::Index = action {
        let mut body = String::from("<h1>Tables</h1>");
        if tables.is_empty() {
            body.push_str("<p class=dim>No tables yet: add a migration.</p>");
        } else {
            body.push_str("<table><tr><th>Table<th>Rows</tr>");
            for table in &tables {
                let mut count = Stmt::default();
                count.sql(format!("SELECT COUNT(*) FROM {}", quote(table)));
                let rows = DB::count(pool, &count).await?;
                let _ = write!(
                    body,
                    "<tr><td><a href=\"{}/{table}\">{table}</a><td>{rows}</tr>",
                    ui.base()
                );
            }
            body.push_str("</table>");
        }
        return Ok(ui.page("Tables", None, &body).into_response());
    }

    let table = req.param("table").unwrap_or_default().into_owned();
    if !tables.contains(&table) {
        return Err(Error::not_found());
    }
    let mut columns = DB::columns(pool, &table).await?;
    columns.retain(|c| is_ident(&c.name));
    for c in &mut columns {
        c.hidden = admin.hidden(&table, &c.name);
    }
    if columns.iter().filter(|c| c.pk).count() != 1 {
        columns.iter_mut().for_each(|c| c.pk = false);
    }
    let t = Table {
        name: table,
        pk: columns.iter().position(|c| c.pk && !c.hidden),
        columns: columns.into_iter().filter(|c| !c.hidden).collect(),
    };
    let editable = !admin.read_only && t.pk.is_some();

    match action {
        Action::Index => unreachable!("handled above"),
        Action::List => list::<DB>(req, &ui, pool, &t, editable).await,
        Action::New => Ok(ui.form(&t, None, &[], None, editable).into_response()),
        Action::Edit | Action::Update | Action::Delete => {
            let Some(pk) = t.pk.map(|i| &t.columns[i]) else {
                return Err(Error::not_found());
            };
            let id = req.param("id").unwrap_or_default().into_owned();
            let mut select = Stmt::default();
            select
                .sql(format!(
                    "SELECT {} FROM {} WHERE {} = ",
                    t.select(),
                    quote(&t.name),
                    quote(&pk.name)
                ))
                .bind::<DB>(Some(id.clone()), pk);
            let Some(row) = DB::rows(pool, &select, t.columns.len()).await?.pop() else {
                return Err(Error::not_found());
            };
            match action {
                Action::Edit => Ok(ui.form(&t, Some(&id), &row, None, editable).into_response()),
                Action::Delete if editable => {
                    let mut delete = Stmt::default();
                    delete
                        .sql(format!(
                            "DELETE FROM {} WHERE {} = ",
                            quote(&t.name),
                            quote(&pk.name)
                        ))
                        .bind::<DB>(Some(id.clone()), pk);
                    match DB::execute(pool, &delete).await {
                        Ok(_) => {
                            Ok(Redirect::to(&format!("{}/{}", ui.base(), t.name)).into_response())
                        }
                        Err(e) => Ok(ui
                            .form(&t, Some(&id), &row, Some(&e.to_string()), editable)
                            .into_response()),
                    }
                }
                Action::Update if editable => {
                    let form = form_params(req).await?;
                    let values = match t.values_from(&form, false) {
                        Ok(values) => values,
                        Err(e) => {
                            let typed = t.typed_row(&form, &row);
                            return Ok(ui
                                .form(&t, Some(&id), &typed, Some(&e), editable)
                                .into_response());
                        }
                    };
                    let mut update = Stmt::default();
                    update.sql(format!("UPDATE {} SET ", quote(&t.name)));
                    let mut any = false;
                    for (i, value) in &values {
                        let c = &t.columns[*i];
                        update.sql(if any { ", " } else { "" });
                        update
                            .sql(format!("{} = ", quote(&c.name)))
                            .bind::<DB>(value.clone(), c);
                        any = true;
                    }
                    if any {
                        update
                            .sql(format!(" WHERE {} = ", quote(&pk.name)))
                            .bind::<DB>(Some(id.clone()), pk);
                        if let Err(e) = DB::execute(pool, &update).await {
                            let typed = t.typed_row(&form, &row);
                            return Ok(ui
                                .form(&t, Some(&id), &typed, Some(&e.to_string()), editable)
                                .into_response());
                        }
                    }
                    Ok(Redirect::to(&format!("{}/{}", ui.base(), t.name)).into_response())
                }
                _ => Err(Error::not_found()),
            }
        }
        Action::Create => {
            if !editable {
                return Err(Error::not_found());
            }
            let form = form_params(req).await?;
            let values = match t.values_from(&form, true) {
                Ok(values) => values,
                Err(e) => {
                    let typed = t.typed_row(&form, &[]);
                    return Ok(ui
                        .form(&t, None, &typed, Some(&e), editable)
                        .into_response());
                }
            };
            let mut insert = Stmt::default();
            insert.sql(format!("INSERT INTO {}", quote(&t.name)));
            if values.is_empty() {
                insert.sql(" DEFAULT VALUES");
            } else {
                let names: Vec<String> = values
                    .iter()
                    .map(|(i, _)| quote(&t.columns[*i].name))
                    .collect();
                insert.sql(format!(" ({}) VALUES (", names.join(", ")));
                for (n, (i, value)) in values.iter().enumerate() {
                    insert.sql(if n > 0 { ", " } else { "" });
                    insert.bind::<DB>(value.clone(), &t.columns[*i]);
                }
                insert.sql(")");
            }
            match DB::execute(pool, &insert).await {
                Ok(_) => Ok(Redirect::to(&format!("{}/{}", ui.base(), t.name)).into_response()),
                Err(e) => {
                    let typed = t.typed_row(&form, &[]);
                    Ok(ui
                        .form(&t, None, &typed, Some(&e.to_string()), editable)
                        .into_response())
                }
            }
        }
    }
}

async fn list<DB: AdminBackend>(
    req: &Request,
    ui: &Ui<'_>,
    pool: &Pool<DB>,
    t: &Table,
    editable: bool,
) -> Result<Response> {
    let params = ListParams::from_query(req.query_string().unwrap_or(""));
    let q = params.get("q").unwrap_or("").trim().to_owned();
    let offset = params
        .get("offset")
        .and_then(|o| o.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let sort = params.get("sort").unwrap_or("");
    let (sort_col, desc) = match sort.strip_prefix('-') {
        Some(name) => (name, true),
        None => (sort, false),
    };
    let order = match t.columns.iter().find(|c| c.name == sort_col) {
        Some(c) => Some((c, desc)),
        None => t.pk.map(|i| (&t.columns[i], true)),
    };

    let mut filter = Stmt::default();
    if !q.is_empty() {
        let pattern = format!("%{}%", like_escape(&q.to_lowercase()));
        let mut first = true;
        for c in &t.columns {
            let searchable = matches!(c.kind, Kind::Text | Kind::Json | Kind::Time);
            let exact = c.pk && matches!(c.kind, Kind::Int) && q.parse::<i64>().is_ok();
            if !(searchable || exact) {
                continue;
            }
            filter.sql(if first { " WHERE (" } else { " OR " });
            first = false;
            if exact {
                filter
                    .sql(format!("{} = ", quote(&c.name)))
                    .bind::<DB>(Some(q.clone()), c);
            } else {
                filter
                    .sql(format!("LOWER(CAST({} AS TEXT)) LIKE ", quote(&c.name)))
                    .text(pattern.clone())
                    .sql(" ESCAPE '\\'");
            }
        }
        if !first {
            filter.sql(")");
        }
    }

    let mut count = Stmt::default();
    count.sql(format!("SELECT COUNT(*) FROM {}", quote(&t.name)));
    count.extend(&filter);
    let total = DB::count(pool, &count).await?;

    let mut select = Stmt::default();
    select.sql(format!("SELECT {} FROM {}", t.select(), quote(&t.name)));
    select.extend(&filter);
    if let Some((c, desc)) = order {
        select.sql(format!(
            " ORDER BY {}{}",
            quote(&c.name),
            if desc { " DESC" } else { "" }
        ));
        if let Some(pk) = t.pk.map(|i| &t.columns[i]).filter(|pk| pk.name != c.name) {
            select.sql(format!(", {}", quote(&pk.name)));
        }
    }
    select.sql(format!(" LIMIT {PAGE_SIZE} OFFSET {offset}"));
    let rows = DB::rows(pool, &select, t.columns.len()).await?;

    let base = format!("{}/{}", ui.base(), t.name);
    let link = |sort: &str, offset: i64| {
        let mut url = format!("{base}?");
        if !q.is_empty() {
            let _ = write!(url, "q={}&", utf8_percent_encode(&q, NON_ALPHANUMERIC));
        }
        if !sort.is_empty() {
            let _ = write!(url, "sort={sort}&");
        }
        if offset > 0 {
            let _ = write!(url, "offset={offset}");
        }
        escape(url.trim_end_matches(['&', '?']))
    };

    let mut body = format!("<h1>{}</h1><div class=bar>", t.name);
    let _ = write!(
        body,
        "<form method=get action=\"{}\"><input type=search name=q value=\"{}\" placeholder=Search> <button>Search</button></form>",
        escape(&base),
        escape(&q)
    );
    if editable {
        let _ = write!(
            body,
            "<a class=button href=\"{}/new\">New row</a>",
            escape(&base)
        );
    }
    body.push_str("</div>");
    let _ = write!(
        body,
        "<p class=dim>{total} row{}{}</p>",
        if total == 1 { "" } else { "s" },
        if q.is_empty() {
            String::new()
        } else {
            format!(" matching “{}”", escape(&q))
        }
    );
    if rows.is_empty() {
        body.push_str("<p class=dim>Nothing here.</p>");
    } else {
        body.push_str("<div class=scroll><table><tr>");
        for c in &t.columns {
            let (next, arrow) = match order {
                Some((o, false)) if o.name == c.name => (format!("-{}", c.name), " ▲"),
                Some((o, true)) if o.name == c.name => (c.name.clone(), " ▼"),
                _ => (c.name.clone(), ""),
            };
            let _ = write!(
                body,
                "<th><a href=\"{}\">{}{arrow}</a>",
                link(&next, 0),
                c.name
            );
        }
        body.push_str("</tr>");
        for row in &rows {
            body.push_str("<tr>");
            for (i, (c, value)) in t.columns.iter().zip(row).enumerate() {
                let cell = cell(c, value.as_deref());
                match (t.pk == Some(i), value) {
                    (true, Some(id)) => {
                        let _ = write!(
                            body,
                            "<td><a href=\"{}/{}\">{cell}</a>",
                            escape(&base),
                            utf8_percent_encode(id, NON_ALPHANUMERIC)
                        );
                    }
                    _ => {
                        let _ = write!(body, "<td>{cell}");
                    }
                }
            }
            body.push_str("</tr>");
        }
        body.push_str("</table></div>");
    }
    let sort = if sort.is_empty() { "" } else { sort };
    body.push_str("<p class=pages>");
    if offset > 0 {
        let _ = write!(
            body,
            "<a href=\"{}\">← Previous</a> ",
            link(sort, (offset - PAGE_SIZE).max(0))
        );
    }
    if offset + PAGE_SIZE < total {
        let _ = write!(
            body,
            "<a href=\"{}\">Next →</a>",
            link(sort, offset + PAGE_SIZE)
        );
    }
    body.push_str("</p>");
    Ok(ui.page(&t.name, Some(&t.name), &body).into_response())
}

async fn form_params(req: &mut Request) -> Result<ListParams> {
    let body = req.body_bytes().await?;
    let body =
        std::str::from_utf8(&body).map_err(|_| Error::bad_request("the form is not UTF-8"))?;
    Ok(ListParams::from_query(body))
}

fn cell(c: &Column, value: Option<&str>) -> String {
    match (c.kind, value) {
        (_, None) => "<span class=dim>null</span>".to_owned(),
        (Kind::Bool, Some(v)) => (if truthy(v) { "✓" } else { "✗" }).to_owned(),
        (_, Some(v)) => {
            let mut short: String = v.chars().take(80).collect();
            if short.len() < v.len() {
                short.push('…');
            }
            escape(&short)
        }
    }
}

fn truthy(v: &str) -> bool {
    matches!(v, "1" | "t" | "true" | "TRUE" | "on")
}

struct Table {
    name: String,
    /// Index of the primary key in `columns`, when there is one to edit by.
    pk: Option<usize>,
    /// The visible columns.
    columns: Vec<Column>,
}

impl Table {
    /// Every visible column as text (binary columns as their size).
    fn select(&self) -> String {
        let exprs: Vec<String> = self
            .columns
            .iter()
            .map(|c| match c.kind {
                Kind::Binary => format!("'<' || LENGTH({0}) || ' bytes>'", quote(&c.name)),
                _ => format!("CAST({} AS TEXT)", quote(&c.name)),
            })
            .collect();
        exprs.join(", ")
    }

    fn writable(&self, i: usize, insert: bool) -> bool {
        let c = &self.columns[i];
        !c.generated && !matches!(c.kind, Kind::Binary) && (!c.pk || (insert && !c.has_default))
    }

    /// The column values a submitted form sets: `None` for SQL NULL.
    /// Empty fields fall back to the column default on insert, and to NULL
    /// when the column allows it; fields missing from an update are left
    /// alone. Numbers are checked here, since SQLite would store any text.
    fn values_from(
        &self,
        form: &ListParams,
        insert: bool,
    ) -> std::result::Result<Vec<(usize, Option<String>)>, String> {
        let mut values = Vec::new();
        for (i, c) in self.columns.iter().enumerate() {
            if !self.writable(i, insert) {
                continue;
            }
            if let Kind::Bool = c.kind {
                values.push((i, Some(form.get(&c.name).is_some_and(truthy).to_string())));
                continue;
            }
            let Some(raw) = form.get(&c.name).or(insert.then_some("")) else {
                continue;
            };
            let blank = raw.is_empty()
                || (raw.trim().is_empty() && !matches!(c.kind, Kind::Text | Kind::Json));
            let value = match (blank, insert && c.has_default, c.nullable) {
                (true, true, _) => continue,
                (true, false, true) => None,
                _ if matches!(c.kind, Kind::Text | Kind::Json) => Some(raw.to_owned()),
                _ => Some(raw.trim().to_owned()),
            };
            let number_ok = match (&value, c.kind) {
                (Some(v), Kind::Int) => v.parse::<i64>().is_ok(),
                (Some(v), Kind::Float) => v.parse::<f64>().is_ok_and(f64::is_finite),
                _ => true,
            };
            if !number_ok {
                return Err(format!("{} must be a number", c.name));
            }
            values.push((i, value));
        }
        Ok(values)
    }

    /// The row as the user typed it, to show the form again after an error.
    fn typed_row(&self, form: &ListParams, stored: &[Option<String>]) -> Vec<Option<String>> {
        self.columns
            .iter()
            .enumerate()
            .map(|(i, c)| match form.get(&c.name) {
                Some(v) => Some(v.to_owned()),
                None if matches!(c.kind, Kind::Bool) => Some("false".to_owned()),
                None => stored.get(i).cloned().flatten(),
            })
            .collect()
    }
}

struct Ui<'a> {
    admin: &'a Admin,
    tables: &'a [String],
}

impl Ui<'_> {
    fn base(&self) -> &str {
        if self.admin.path == "/" {
            ""
        } else {
            &self.admin.path
        }
    }

    fn page(&self, title: &str, current: Option<&str>, body: &str) -> Html<String> {
        let mut page = format!(
            "<!doctype html><meta charset=utf-8><title>{} · Admin</title>\
             <meta name=viewport content=\"width=device-width,initial-scale=1\">\
             <style>:root{{color-scheme:light dark}}\
             body{{font:14px/1.5 system-ui,sans-serif;margin:0;display:flex;min-height:100vh}}\
             nav{{width:13rem;flex:none;padding:1rem;border-right:1px solid #8884}}\
             nav a{{display:block;padding:.15rem .5rem;border-radius:4px;text-decoration:none}}\
             nav a.on{{background:#8883;font-weight:600}}main{{flex:1;min-width:0;padding:1rem 2rem}}\
             .scroll{{overflow-x:auto}}table{{border-collapse:collapse;width:100%}}\
             td,th{{text-align:left;padding:.3rem .5rem;border-bottom:1px solid #8882;white-space:nowrap}}\
             th a{{color:inherit}}.dim{{color:#888}}.error{{color:#c22;border:1px solid #c225;padding:.5rem;border-radius:6px}}\
             .bar{{display:flex;gap:1rem;align-items:center;flex-wrap:wrap}}\
             label{{display:block;margin:.75rem 0 .25rem;font-weight:600}}\
             input:not([type=checkbox]),textarea{{font:inherit;width:100%;max-width:40rem;box-sizing:border-box;padding:.35rem}}.bar input{{width:16rem}}\
             textarea{{min-height:6rem}}button,.button{{font:inherit;padding:.35rem .9rem;cursor:pointer}}\
             .danger{{color:#c22}}@media(max-width:40rem){{body{{display:block}}nav{{width:auto;border:0}}}}</style>\
             <nav><a href=\"{}\"><b>Admin</b></a>",
            escape(title),
            escape(if self.base().is_empty() { "/" } else { self.base() })
        );
        for table in self.tables {
            let on = if Some(table.as_str()) == current {
                " class=on"
            } else {
                ""
            };
            let _ = write!(page, "<a{on} href=\"{}/{table}\">{table}</a>", self.base());
        }
        let _ = write!(page, "</nav><main>{body}</main>");
        Html(page)
    }

    fn form(
        &self,
        t: &Table,
        id: Option<&str>,
        row: &[Option<String>],
        error: Option<&str>,
        editable: bool,
    ) -> Response {
        let base = format!("{}/{}", self.base(), t.name);
        let title = match id {
            Some(id) => format!("{} {}", t.name, id),
            None => format!("New {}", t.name),
        };
        let mut body = format!(
            "<p><a href=\"{}\">← {}</a></p><h1>{}</h1>",
            escape(&base),
            t.name,
            escape(&title)
        );
        if let Some(error) = error {
            let _ = write!(body, "<p class=error role=alert>{}</p>", escape(error));
        }
        let action = match id {
            Some(id) => format!("{base}/{}", utf8_percent_encode(id, NON_ALPHANUMERIC)),
            None => base.clone(),
        };
        let _ = write!(body, "<form method=post action=\"{}\">", escape(&action));
        for (i, c) in t.columns.iter().enumerate() {
            let value = row.get(i).cloned().flatten();
            let read_only = !editable || !t.writable(i, id.is_none());
            if id.is_none() && read_only && (c.has_default || c.generated) {
                continue;
            }
            let disabled = if read_only { " disabled" } else { "" };
            let hint = match (!read_only && (c.nullable || c.has_default), c.kind) {
                (_, Kind::Bool) => String::new(),
                (true, _) => format!(
                    " <small class=dim>{} · optional</small>",
                    escape(&c.ty.to_lowercase())
                ),
                (false, _) => format!(" <small class=dim>{}</small>", escape(&c.ty.to_lowercase())),
            };
            let name = &c.name;
            let v = escape(value.as_deref().unwrap_or(""));
            match c.kind {
                Kind::Bool => {
                    let checked = if value.as_deref().is_some_and(truthy) {
                        " checked"
                    } else {
                        ""
                    };
                    let _ = write!(
                        body,
                        "<label><input type=checkbox name={name}{checked}{disabled}> {name}</label>"
                    );
                }
                Kind::Json => {
                    let _ = write!(body, "<label for={name}>{name}{hint}</label><textarea id={name} name={name}{disabled}>{v}</textarea>");
                }
                Kind::Text
                    if value
                        .as_deref()
                        .is_some_and(|v| v.len() > 60 || v.contains('\n')) =>
                {
                    let _ = write!(body, "<label for={name}>{name}{hint}</label><textarea id={name} name={name}{disabled}>{v}</textarea>");
                }
                kind => {
                    let input = match kind {
                        Kind::Int => " type=number step=1",
                        Kind::Float => " type=number step=any",
                        _ => "",
                    };
                    let required = if c.nullable || c.has_default || matches!(kind, Kind::Text) {
                        ""
                    } else {
                        " required"
                    };
                    let _ = write!(body, "<label for={name}>{name}{hint}</label><input id={name} name={name}{input} value=\"{v}\"{required}{disabled}>");
                }
            }
        }
        if editable {
            body.push_str("<p><button>Save</button></p>");
        }
        body.push_str("</form>");
        if let (Some(_), true) = (id, editable) {
            let _ = write!(
                body,
                "<form method=post action=\"{}/delete\" onsubmit=\"return confirm('Delete this row?')\"><button class=danger>Delete</button></form>",
                escape(&action)
            );
        }
        let status = if error.is_some() {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::OK
        };
        (status, self.page(&title, Some(&t.name), &body)).into_response()
    }
}

/// A column as the admin sees it.
#[doc(hidden)]
pub struct Column {
    pub(crate) name: String,
    /// The declared type, as the database spells it.
    pub(crate) ty: String,
    pub(crate) kind: Kind,
    pub(crate) nullable: bool,
    pub(crate) has_default: bool,
    pub(crate) pk: bool,
    pub(crate) generated: bool,
    hidden: bool,
}

impl Column {
    fn new(
        name: String,
        ty: String,
        nullable: bool,
        has_default: bool,
        pk: bool,
        generated: bool,
    ) -> Self {
        Column {
            kind: Kind::of(&ty),
            name,
            ty,
            nullable,
            has_default,
            pk,
            generated,
            hidden: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Kind {
    Bool,
    Int,
    Float,
    Time,
    Json,
    Binary,
    Text,
}

impl Kind {
    fn of(ty: &str) -> Kind {
        let t = ty.to_ascii_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| t.contains(w));
        if has(&["bool"]) {
            Kind::Bool
        } else if has(&["int", "serial"]) && !has(&["interval", "point"]) {
            Kind::Int
        } else if has(&["real", "floa", "doub", "numeric", "decimal"]) {
            Kind::Float
        } else if has(&["blob", "bytea"]) {
            Kind::Binary
        } else if has(&["json"]) {
            Kind::Json
        } else if has(&["date", "time"]) {
            Kind::Time
        } else {
            Kind::Text
        }
    }
}

/// A statement under construction: SQL text and text parameters.
#[doc(hidden)]
#[derive(Default)]
pub struct Stmt {
    parts: Vec<Part>,
}

enum Part {
    Sql(String),
    /// A value, converted to the column's type where the database needs it.
    Bind(Option<String>, Option<String>),
}

impl Stmt {
    pub(crate) fn sql(&mut self, sql: impl Into<String>) -> &mut Self {
        self.parts.push(Part::Sql(sql.into()));
        self
    }

    /// A value for `column`.
    pub(crate) fn bind<DB: AdminBackend>(
        &mut self,
        value: Option<String>,
        column: &Column,
    ) -> &mut Self {
        let value = match (column.kind, value) {
            (Kind::Bool, Some(v)) => Some(DB::bool_text(truthy(&v)).to_owned()),
            (_, v) => v,
        };
        let cast = DB::CAST_PARAMS.then(|| column.ty.clone());
        self.parts.push(Part::Bind(value, cast));
        self
    }

    /// A text value.
    fn text(&mut self, value: String) -> &mut Self {
        self.parts.push(Part::Bind(Some(value), None));
        self
    }

    fn extend(&mut self, other: &Stmt) {
        for part in &other.parts {
            self.parts.push(match part {
                Part::Sql(s) => Part::Sql(s.clone()),
                Part::Bind(v, c) => Part::Bind(v.clone(), c.clone()),
            });
        }
    }
}

pub(crate) fn is_ident(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}

pub(crate) fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

mod sealed {
    pub trait Sealed {}
}

/// A [`Stmt`] as a query for one backend.
trait BuildFor<DB: sqlx::Database> {
    fn query(&self) -> QueryBuilder<'_, DB>;
}

/// A database the admin panel works with: SQLite and Postgres.
pub trait AdminBackend: sqlx::Database + sealed::Sealed {
    #[doc(hidden)]
    const CAST_PARAMS: bool;
    #[doc(hidden)]
    fn bool_text(b: bool) -> &'static str;
    #[doc(hidden)]
    fn tables(pool: &Pool<Self>) -> impl Future<Output = Result<Vec<String>>> + Send;
    #[doc(hidden)]
    fn columns(pool: &Pool<Self>, table: &str) -> impl Future<Output = Result<Vec<Column>>> + Send;
    #[doc(hidden)]
    fn count(pool: &Pool<Self>, stmt: &Stmt) -> impl Future<Output = Result<i64>> + Send;
    #[doc(hidden)]
    fn rows(
        pool: &Pool<Self>,
        stmt: &Stmt,
        width: usize,
    ) -> impl Future<Output = Result<Vec<Vec<Option<String>>>>> + Send;
    #[doc(hidden)]
    fn execute(pool: &Pool<Self>, stmt: &Stmt) -> impl Future<Output = sqlx::Result<u64>> + Send;
    /// Single-column foreign keys of `table`: (column, parent table, parent
    /// column, or `None` for the parent's primary key).
    #[doc(hidden)]
    fn foreign_keys(
        pool: &Pool<Self>,
        table: &str,
    ) -> impl Future<Output = Result<Vec<(String, String, Option<String>)>>> + Send;
}

macro_rules! admin_backend {
    ($db:ty, cast: $cast:expr, bools: ($t:expr, $f:expr), tables: $tables:expr, columns: $columns:expr, foreign_keys: $fks:expr) => {
        impl sealed::Sealed for $db {}

        impl BuildFor<$db> for Stmt {
            fn query(&self) -> QueryBuilder<'_, $db> {
                let mut q = QueryBuilder::new("");
                for part in &self.parts {
                    match part {
                        Part::Sql(sql) => {
                            q.push(sql);
                        }
                        Part::Bind(value, Some(ty)) => {
                            q.push("CAST(")
                                .push_bind(value.clone())
                                .push(" AS ")
                                .push(ty)
                                .push(")");
                        }
                        Part::Bind(value, None) => {
                            q.push_bind(value.clone());
                        }
                    }
                }
                q
            }
        }

        impl AdminBackend for $db {
            const CAST_PARAMS: bool = $cast;

            fn bool_text(b: bool) -> &'static str {
                if b {
                    $t
                } else {
                    $f
                }
            }

            async fn tables(pool: &Pool<Self>) -> Result<Vec<String>> {
                Ok(sqlx::query_scalar::<_, String>($tables)
                    .fetch_all(pool)
                    .await?)
            }

            async fn columns(pool: &Pool<Self>, table: &str) -> Result<Vec<Column>> {
                let rows: Vec<(String, String, bool, bool, bool, bool)> =
                    sqlx::query_as($columns).bind(table).fetch_all(pool).await?;
                Ok(rows
                    .into_iter()
                    .map(|(name, ty, not_null, default, pk, generated)| {
                        Column::new(name, ty, !not_null, default, pk, generated)
                    })
                    .collect())
            }

            async fn count(pool: &Pool<Self>, stmt: &Stmt) -> Result<i64> {
                Ok(BuildFor::<$db>::query(stmt)
                    .build_query_scalar::<i64>()
                    .fetch_one(pool)
                    .await?)
            }

            async fn rows(
                pool: &Pool<Self>,
                stmt: &Stmt,
                width: usize,
            ) -> Result<Vec<Vec<Option<String>>>> {
                let rows = BuildFor::<$db>::query(stmt).build().fetch_all(pool).await?;
                rows.iter()
                    .map(|row| {
                        (0..width)
                            .map(|i| row.try_get::<Option<String>, _>(i).map_err(Error::from))
                            .collect()
                    })
                    .collect()
            }

            async fn execute(pool: &Pool<Self>, stmt: &Stmt) -> sqlx::Result<u64> {
                Ok(BuildFor::<$db>::query(stmt)
                    .build()
                    .execute(pool)
                    .await?
                    .rows_affected())
            }

            async fn foreign_keys(
                pool: &Pool<Self>,
                table: &str,
            ) -> Result<Vec<(String, String, Option<String>)>> {
                Ok(sqlx::query_as($fks).bind(table).fetch_all(pool).await?)
            }
        }
    };
}

#[cfg(feature = "sqlite")]
admin_backend!(
    sqlx::Sqlite,
    cast: false,
    bools: ("1", "0"),
    tables: "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
    columns: "SELECT name, type, \"notnull\" <> 0, dflt_value IS NOT NULL \
              OR (pk = 1 AND UPPER(type) = 'INTEGER'), pk > 0, hidden IN (2, 3) \
              FROM pragma_table_xinfo(?) WHERE hidden <> 1 ORDER BY cid",
    foreign_keys: "SELECT \"from\", \"table\", \"to\" FROM pragma_foreign_key_list(?1) f \
                   WHERE (SELECT COUNT(*) FROM pragma_foreign_key_list(?1) g WHERE g.id = f.id) = 1"
);

#[cfg(feature = "postgres")]
admin_backend!(
    sqlx::Postgres,
    cast: true,
    bools: ("true", "false"),
    tables: "SELECT c.relname::text FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p') AND n.nspname = current_schema() ORDER BY 1",
    columns: "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
              a.atthasdef OR a.attidentity <> '', \
              EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid \
                      AND i.indisprimary AND a.attnum = ANY(i.indkey)), \
              a.attgenerated <> '' \
              FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
              JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = current_schema() AND c.relname = $1 \
              AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
    foreign_keys: "SELECT a.attname::text, p.relname::text, pa.attname::text \
                   FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                   JOIN pg_class p ON p.oid = k.confrelid \
                   JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = k.conkey[1] \
                   JOIN pg_attribute pa ON pa.attrelid = k.confrelid AND pa.attnum = k.confkey[1] \
                   WHERE k.contype = 'f' AND cardinality(k.conkey) = 1 \
                   AND n.nspname = current_schema() AND c.relname = $1"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_declared_types() {
        for (ty, kind) in [
            ("INTEGER", Kind::Int),
            ("bigint", Kind::Int),
            ("BOOLEAN", Kind::Bool),
            ("timestamp with time zone", Kind::Time),
            ("interval", Kind::Text),
            ("point", Kind::Text),
            ("double precision", Kind::Float),
            ("numeric(10,2)", Kind::Float),
            ("jsonb", Kind::Json),
            ("BLOB", Kind::Binary),
            ("character varying(255)", Kind::Text),
            ("TEXT", Kind::Text),
        ] {
            assert_eq!(Kind::of(ty), kind, "{ty}");
        }
    }

    #[test]
    fn secrets_are_hidden() {
        let admin = Admin::default().hide(["users.email", "ip"]);
        for column in [
            "password_hash",
            "api_token",
            "client_secret",
            "token_hash",
            "ip",
        ] {
            assert!(admin.hidden("users", column), "{column}");
        }
        assert!(admin.hidden("users", "email"));
        assert!(!admin.hidden("posts", "email"));
        assert!(!admin.hidden("users", "name"));
    }

    #[test]
    fn only_plain_identifiers() {
        assert!(is_ident("users") && is_ident("_x1"));
        for bad in ["", "1abc", "a b", "a\"b", "a;b", "ü"] {
            assert!(!is_ident(bad), "{bad}");
        }
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(like_escape("50%_\\"), "50\\%\\_\\\\");
    }
}
