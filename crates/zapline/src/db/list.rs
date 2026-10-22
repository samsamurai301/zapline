//! List endpoints: pagination, sorting, filtering and search from the query
//! string, safely.

use std::future::Future;

use serde::Serialize;
use sqlx::{FromRow, Pool, QueryBuilder};

use super::Db;
use crate::extract::FromRequestParts;
use crate::{Error, Request, Result};

/// The raw query string of a list request, read by [`Listing::fetch`]:
/// `limit`, `offset`, `sort`, `q`, and one parameter per filter.
///
/// ```
/// use zapline::db::ListParams;
///
/// let params = ListParams::from_query("limit=10&sort=-created_at&q=rust+web&done=true");
/// assert_eq!(params.get("q"), Some("rust web"));
/// ```
#[derive(Clone, Debug, Default)]
pub struct ListParams {
    pairs: Vec<(String, String)>,
}

impl ListParams {
    /// Parse a URL query string (without the `?`).
    ///
    /// ```
    /// let params = zapline::db::ListParams::from_query("offset=20");
    /// assert_eq!(params.get("offset"), Some("20"));
    /// ```
    pub fn from_query(query: &str) -> Self {
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(&s.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        };
        let pairs = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                (decode(k), decode(v))
            })
            .collect();
        ListParams { pairs }
    }

    /// The first value of `name`.
    ///
    /// ```
    /// let params = zapline::db::ListParams::from_query("a=1&a=2");
    /// assert_eq!(params.get("a"), Some("1"));
    /// ```
    pub fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

impl FromRequestParts for ListParams {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        Ok(ListParams::from_query(req.query_string().unwrap_or("")))
    }
}

#[cfg(feature = "openapi")]
impl crate::openapi::ApiInput for ListParams {
    fn describe_input(op: &mut crate::openapi::OperationDoc) {
        use crate::openapi::ParamIn::Query;
        op.param::<u32>(Query, "limit", false)
            .param::<u32>(Query, "offset", false)
            .param::<String>(Query, "sort", false)
            .param::<String>(Query, "q", false)
            .error_response(400, "Invalid sort, filter or page");
    }
}

/// One page of a list, as JSON:
/// `{"items": [...], "total": 42, "limit": 20, "offset": 0}`.
#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "openapi", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", schemars(rename = "Paged{T}"))]
pub struct Paged<T> {
    /// This page's items.
    pub items: Vec<T>,
    /// How many items match, across all pages.
    pub total: i64,
    /// The page size used.
    pub limit: i64,
    /// How many matching items come before this page.
    pub offset: i64,
}

impl<T> Paged<T> {
    /// The `offset` of the next page, if there is one.
    ///
    /// ```
    /// let page = zapline::db::Paged { items: vec![1, 2], total: 5, limit: 2, offset: 0 };
    /// assert_eq!(page.next_offset(), Some(2));
    /// ```
    pub fn next_offset(&self) -> Option<i64> {
        let next = self.offset + self.limit;
        (next < self.total).then_some(next)
    }
}

/// A value a filter or scope compares against.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// An integer column.
    Int(i64),
    /// A boolean column.
    Bool(bool),
    /// A text column.
    Text(String),
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int(v.into())
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_owned())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Int,
    Bool,
    Text,
}

/// A list endpoint over one table: which columns callers may sort by,
/// filter on and search, with a page size limit. Column names come from
/// your code; values from the query string are always bound as parameters,
/// and sort columns must be on the allow-list, so nothing from the request
/// reaches the SQL text.
///
/// ```no_run
/// # #[cfg(feature = "sqlite")] {
/// use zapline::db::{sqlx, Db, ListParams, Listing, Paged};
/// use zapline::prelude::*;
///
/// #[model]
/// #[derive(sqlx::FromRow)]
/// struct Post { id: i64, title: String, published: bool }
///
/// // GET /posts?q=rust&published=true&sort=-id&limit=20&offset=40
/// #[get("/posts")]
/// async fn posts(db: Db, params: ListParams) -> Result<Json<Paged<Post>>> {
///     let page = Listing::new("posts", "id, title, published")
///         .sortable(["id", "title"])
///         .search(["title"])
///         .filter_bool("published")
///         .fetch(&db, &params)
///         .await?;
///     Ok(Json(page))
/// }
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct Listing {
    table: &'static str,
    columns: &'static str,
    key: &'static str,
    sortable: Vec<&'static str>,
    search: Vec<&'static str>,
    filters: Vec<(&'static str, Kind)>,
    scopes: Vec<(&'static str, Value)>,
    default_sort: &'static str,
    default_limit: i64,
    max_limit: i64,
}

impl Listing {
    /// List `columns` (SQL, e.g. `"id, title"`) of `table`, sorted by
    /// `id` unless the request says otherwise, 50 per page (at most 200).
    ///
    /// ```
    /// let listing = zapline::db::Listing::new("posts", "id, title");
    /// ```
    pub fn new(table: &'static str, columns: &'static str) -> Self {
        Listing {
            table,
            columns,
            key: "id",
            sortable: Vec::new(),
            search: Vec::new(),
            filters: Vec::new(),
            scopes: Vec::new(),
            default_sort: "id",
            default_limit: 50,
            max_limit: 200,
        }
    }

    /// The unique column that breaks ties in sorting (default `id`).
    ///
    /// ```
    /// let listing = zapline::db::Listing::new("users", "email, name").key("email");
    /// ```
    #[must_use]
    pub fn key(mut self, column: &'static str) -> Self {
        self.key = column;
        self.default_sort = column;
        self
    }

    /// Columns `?sort=` may name: `sort=title`, `sort=-created_at` (descending),
    /// `sort=-done,title`.
    ///
    /// ```
    /// let listing = zapline::db::Listing::new("posts", "id, title").sortable(["id", "title"]);
    /// ```
    #[must_use]
    pub fn sortable(mut self, columns: impl IntoIterator<Item = &'static str>) -> Self {
        self.sortable.extend(columns);
        self
    }

    /// The sort when the request gives none, in `?sort=` syntax (default:
    /// the key, ascending).
    ///
    /// ```
    /// let listing = zapline::db::Listing::new("posts", "id, title").default_sort("-id");
    /// ```
    #[must_use]
    pub fn default_sort(mut self, sort: &'static str) -> Self {
        self.default_sort = sort;
        self
    }

    /// Text columns `?q=` searches: rows where any of them contains the
    /// words, ignoring case.
    ///
    /// ```
    /// let listing = zapline::db::Listing::new("posts", "id, title").search(["title", "body"]);
    /// ```
    #[must_use]
    pub fn search(mut self, columns: impl IntoIterator<Item = &'static str>) -> Self {
        self.search.extend(columns);
        self
    }

    /// `?column=42` keeps rows where the integer column equals it.
    ///
    /// ```
    /// let listing = zapline::db::Listing::new("posts", "id, title").filter_int("author_id");
    /// ```
    #[must_use]
    pub fn filter_int(mut self, column: &'static str) -> Self {
        self.filters.push((column, Kind::Int));
        self
    }

    /// `?column=true` (or `false`, `1`, `0`) keeps rows where the boolean
    /// column matches.
    ///
    /// ```
    /// let listing = zapline::db::Listing::new("todos", "id, title").filter_bool("done");
    /// ```
    #[must_use]
    pub fn filter_bool(mut self, column: &'static str) -> Self {
        self.filters.push((column, Kind::Bool));
        self
    }

    /// `?column=text` keeps rows where the text column equals it exactly.
    ///
    /// ```
    /// let listing = zapline::db::Listing::new("orders", "id, status").filter_text("status");
    /// ```
    #[must_use]
    pub fn filter_text(mut self, column: &'static str) -> Self {
        self.filters.push((column, Kind::Text));
        self
    }

    /// Always keep only rows where `column` equals `value`, whatever the
    /// request says: the current user's rows, one tenant's.
    ///
    /// ```
    /// let user_id = 7;
    /// let listing = zapline::db::Listing::new("todos", "id, title").scope("user_id", user_id);
    /// ```
    #[must_use]
    pub fn scope(mut self, column: &'static str, value: impl Into<Value>) -> Self {
        self.scopes.push((column, value.into()));
        self
    }

    /// The default and the largest page size (default 50 and 200).
    ///
    /// ```
    /// let listing = zapline::db::Listing::new("posts", "id").page_size(20, 100);
    /// ```
    #[must_use]
    pub fn page_size(mut self, default: i64, max: i64) -> Self {
        self.max_limit = max.max(1);
        self.default_limit = default.clamp(1, self.max_limit);
        self
    }

    /// Run the list query and a count for `params`, on a [`Db`] or a pool
    /// of either backend.
    ///
    /// # Errors
    ///
    /// `400` for a sort column that isn't [sortable](Self::sortable)
    /// (`invalid_sort`), a filter value of the wrong type
    /// (`invalid_filter`), or a negative or non-numeric `limit`/`offset`
    /// (`invalid_page`); database errors otherwise.
    pub async fn fetch<DB, T>(&self, db: &impl AsPool<DB>, params: &ListParams) -> Result<Paged<T>>
    where
        DB: ListBackend,
        T: for<'r> FromRow<'r, DB::Row> + Send + Unpin,
    {
        let limit = match params.get("limit") {
            None | Some("") => self.default_limit,
            Some(v) => parse_page(v, "limit")?.clamp(1, self.max_limit),
        };
        let offset = match params.get("offset") {
            None | Some("") => 0,
            Some(v) => parse_page(v, "offset")?,
        };
        let plan = Plan {
            table: self.table,
            columns: self.columns,
            order: self.order_by(params.get("sort").filter(|s| !s.is_empty()))?,
            conditions: self.conditions(params)?,
            limit,
            offset,
        };
        let (items, total) = DB::run(db.as_pool(), &plan).await?;
        Ok(Paged {
            items,
            total,
            limit,
            offset,
        })
    }

    fn order_by(&self, sort: Option<&str>) -> Result<String> {
        let sort = sort.unwrap_or(self.default_sort);
        let mut parts = Vec::new();
        let mut has_key = false;
        for item in sort.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (column, dir) = match item.strip_prefix('-') {
                Some(c) => (c, "DESC"),
                None => (item.strip_prefix('+').unwrap_or(item), "ASC"),
            };
            let allowed = column == self.key || self.sortable.contains(&column);
            if !allowed {
                let mut names: Vec<&str> = self.sortable.clone();
                if !names.contains(&self.key) {
                    names.push(self.key);
                }
                return Err(Error::bad_request(format!(
                    "can't sort by {column:?}; sortable: {}",
                    names.join(", ")
                ))
                .with_code("invalid_sort"));
            }
            has_key |= column == self.key;
            parts.push(format!("{column} {dir}"));
        }
        if !has_key {
            parts.push(format!("{} ASC", self.key));
        }
        Ok(parts.join(", "))
    }

    fn conditions(&self, params: &ListParams) -> Result<Vec<Condition>> {
        let mut out: Vec<Condition> = self
            .scopes
            .iter()
            .map(|(column, value)| Condition::Eq(column, value.clone()))
            .collect();
        for (column, kind) in &self.filters {
            let Some(raw) = params.get(column).filter(|v| !v.is_empty()) else {
                continue;
            };
            let value = match kind {
                Kind::Int => raw.parse::<i64>().ok().map(Value::Int),
                Kind::Bool => match raw {
                    "true" | "1" | "on" | "yes" => Some(Value::Bool(true)),
                    "false" | "0" | "off" | "no" => Some(Value::Bool(false)),
                    _ => None,
                },
                Kind::Text => Some(Value::Text(raw.to_owned())),
            };
            let Some(value) = value else {
                return Err(Error::bad_request(format!(
                    "filter {column:?} expects {}",
                    match kind {
                        Kind::Int => "an integer",
                        Kind::Bool => "true or false",
                        Kind::Text => "text",
                    }
                ))
                .with_code("invalid_filter"));
            };
            out.push(Condition::Eq(column, value));
        }
        if let Some(q) = params.get("q").map(str::trim).filter(|q| !q.is_empty()) {
            if !self.search.is_empty() {
                let pattern = format!("%{}%", escape_like(&q.to_lowercase()));
                out.push(Condition::Search(self.search.clone(), pattern));
            }
        }
        Ok(out)
    }
}

#[derive(Debug)]
enum Condition {
    Eq(&'static str, Value),
    Search(Vec<&'static str>, String),
}

/// Everything a backend needs to run a list query.
#[doc(hidden)]
pub struct Plan {
    table: &'static str,
    columns: &'static str,
    order: String,
    conditions: Vec<Condition>,
    limit: i64,
    offset: i64,
}

/// [`Db`] or a sqlx pool, for [`Listing::fetch`].
pub trait AsPool<DB: sqlx::Database> {
    /// The pool.
    fn as_pool(&self) -> &Pool<DB>;
}

impl<DB: sqlx::Database> AsPool<DB> for Pool<DB> {
    fn as_pool(&self) -> &Pool<DB> {
        self
    }
}

impl<DB: sqlx::Database> AsPool<DB> for Db<DB> {
    fn as_pool(&self) -> &Pool<DB> {
        &self.0
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A database [`Listing`] runs on: SQLite and Postgres.
#[allow(async_fn_in_trait)]
pub trait ListBackend: sqlx::Database + sealed::Sealed {
    #[doc(hidden)]
    fn run<T>(pool: &Pool<Self>, plan: &Plan) -> impl Future<Output = Result<(Vec<T>, i64)>> + Send
    where
        T: for<'r> FromRow<'r, Self::Row> + Send + Unpin;
}

macro_rules! list_backend {
    ($db:ty) => {
        impl sealed::Sealed for $db {}

        impl ListBackend for $db {
            async fn run<T>(pool: &Pool<Self>, plan: &Plan) -> Result<(Vec<T>, i64)>
            where
                T: for<'r> FromRow<'r, Self::Row> + Send + Unpin,
            {
                fn push_where(query: &mut QueryBuilder<'_, $db>, conditions: &[Condition]) {
                    for (i, condition) in conditions.iter().enumerate() {
                        query.push(if i == 0 { " WHERE " } else { " AND " });
                        match condition {
                            Condition::Eq(column, value) => {
                                query.push(*column).push(" = ");
                                match value {
                                    Value::Int(v) => query.push_bind(*v),
                                    Value::Bool(v) => query.push_bind(*v),
                                    Value::Text(v) => query.push_bind(v.clone()),
                                };
                            }
                            Condition::Search(columns, pattern) => {
                                query.push("(");
                                for (j, column) in columns.iter().enumerate() {
                                    if j > 0 {
                                        query.push(" OR ");
                                    }
                                    query.push("LOWER(").push(*column).push(") LIKE ");
                                    query.push_bind(pattern.clone()).push(" ESCAPE '\\'");
                                }
                                query.push(")");
                            }
                        }
                    }
                }

                let mut count =
                    QueryBuilder::<$db>::new(format!("SELECT COUNT(*) FROM {}", plan.table));
                push_where(&mut count, &plan.conditions);
                let total: i64 = count.build_query_scalar().fetch_one(pool).await?;

                let mut query = QueryBuilder::<$db>::new(format!(
                    "SELECT {} FROM {}",
                    plan.columns, plan.table
                ));
                push_where(&mut query, &plan.conditions);
                query.push(" ORDER BY ").push(&plan.order).push(" LIMIT ");
                query
                    .push_bind(plan.limit)
                    .push(" OFFSET ")
                    .push_bind(plan.offset);
                let items = query.build_query_as::<T>().fetch_all(pool).await?;
                Ok((items, total))
            }
        }
    };
}

#[cfg(feature = "sqlite")]
list_backend!(sqlx::Sqlite);
#[cfg(feature = "postgres")]
list_backend!(sqlx::Postgres);

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn parse_page(v: &str, name: &str) -> Result<i64> {
    match v.parse::<i64>() {
        Ok(n) if n >= 0 => Ok(n),
        _ => Err(
            Error::bad_request(format!("{name} must be a non-negative integer"))
                .with_code("invalid_page"),
        ),
    }
}
