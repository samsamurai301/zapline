//! Test data in one line: [`Factory`] inserts a row, filling every required
//! column you don't set with a plausible value, read from the schema.

use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::Pool;

use crate::admin::{is_ident, quote, AdminBackend, Column, Kind, Stmt};
use crate::{Error, Result};

/// How deep required foreign keys are followed (a row, its parent, ...).
const MAX_DEPTH: usize = 4;

/// Makes rows for tests and seed data. Every column you don't
/// [`set`](Factory::set) that the database requires (`NOT NULL`, no default)
/// gets a value: unique numbers and text such as `title 7`, `user7@example.com`
/// for columns named like emails, `false`, the current time, `{}` for JSON.
/// A required foreign key gets a new parent row, made the same way.
///
/// Works on SQLite and Postgres, with the pool from [`Db`](crate::db::Db) or
/// from the test client's resources:
///
/// ```
/// use zapline::db::{sqlx, Database, Factory};
/// use zapline::prelude::*;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
/// # let dir = tempfile::tempdir().unwrap();
/// # std::fs::write(dir.path().join("1_init.sql"), "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE);\
/// #   CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), title TEXT NOT NULL, views INTEGER NOT NULL DEFAULT 0);").unwrap();
/// # let db = Database::sqlite_memory().migrations(dir.path());
/// let client = App::new().database(db).test();
/// let pool: sqlx::Pool<sqlx::Sqlite> = client.resources().await.require()?;
///
/// let post = Factory::new("posts").set("title", "Hello").create(&pool).await?;
/// assert_eq!(post.get("title"), Some("Hello"));
/// assert_eq!(post.int("views"), Some(0)); // the column default
/// // A user was created for the required `user_id`.
/// assert_eq!(post.int("user_id"), Some(1));
///
/// let more = Factory::new("posts").create_many(&pool, 3).await?;
/// assert_eq!(more.len(), 3);
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
#[must_use]
pub struct Factory {
    table: String,
    values: Vec<(String, Option<String>)>,
}

/// A row a [`Factory`] inserted, as the database stored it.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    columns: Vec<(String, Kind, Option<String>)>,
}

static SEQUENCE: AtomicU64 = AtomicU64::new(1);

impl Factory {
    /// A factory for rows of `table`.
    pub fn new(table: impl Into<String>) -> Self {
        Factory {
            table: table.into(),
            values: Vec::new(),
        }
    }

    /// Sets a column: any value that prints as the database expects it
    /// (`"text"`, `42`, `9.5`, `true`, `"2026-01-31"`).
    ///
    /// ```
    /// # use zapline::db::Factory;
    /// let done = Factory::new("todos").set("title", "Ship it").set("done", true);
    /// ```
    pub fn set(mut self, column: impl Into<String>, value: impl Display) -> Self {
        self.put(column.into(), Some(value.to_string()));
        self
    }

    /// Sets a column to SQL `NULL`.
    ///
    /// ```
    /// # use zapline::db::Factory;
    /// let anonymous = Factory::new("comments").null("user_id");
    /// ```
    pub fn null(mut self, column: impl Into<String>) -> Self {
        self.put(column.into(), None);
        self
    }

    fn put(&mut self, column: String, value: Option<String>) {
        self.values.retain(|(c, _)| *c != column);
        self.values.push((column, value));
    }

    /// Inserts one row and returns it.
    pub async fn create<DB: AdminBackend>(&self, pool: &Pool<DB>) -> Result<Record> {
        insert(pool, self.table.clone(), self.values.clone(), 0).await
    }

    /// Inserts `n` rows (each with its own generated values).
    ///
    /// ```no_run
    /// # use zapline::db::{sqlx, DefaultDb, Factory};
    /// # async fn f(pool: sqlx::Pool<DefaultDb>) -> zapline::Result<()> {
    /// let posts = Factory::new("posts").set("published", true).create_many(&pool, 20).await?;
    /// # Ok(()) }
    /// ```
    pub async fn create_many<DB: AdminBackend>(
        &self,
        pool: &Pool<DB>,
        n: usize,
    ) -> Result<Vec<Record>> {
        let mut rows = Vec::with_capacity(n);
        for _ in 0..n {
            rows.push(self.create(pool).await?);
        }
        Ok(rows)
    }
}

impl Record {
    /// A column's value as text (`None` for `NULL` or an unknown column).
    /// Booleans read `true`/`false` on every database.
    ///
    /// ```no_run
    /// # async fn f(post: zapline::db::Record) {
    /// assert_eq!(post.get("title"), Some("title 1"));
    /// # }
    /// ```
    pub fn get(&self, column: &str) -> Option<&str> {
        self.columns
            .iter()
            .find(|(name, _, _)| name == column)
            .and_then(|(_, _, value)| value.as_deref())
    }

    /// A column's value as an integer, e.g. the `id`.
    ///
    /// ```no_run
    /// # async fn f(post: zapline::db::Record) {
    /// let id = post.int("id").unwrap();
    /// # }
    /// ```
    pub fn int(&self, column: &str) -> Option<i64> {
        self.get(column)?.parse().ok()
    }

    /// The row as a JSON object, with numbers, booleans and JSON columns
    /// typed: compare it with what your API returns.
    ///
    /// ```no_run
    /// # async fn f(post: zapline::db::Record) {
    /// assert_eq!(post.json()["views"], 0);
    /// # }
    /// ```
    #[cfg(feature = "json")]
    pub fn json(&self) -> serde_json::Value {
        use serde_json::Value;
        let typed = |kind: Kind, text: &str| -> Value {
            let parsed = match kind {
                Kind::Int | Kind::Float | Kind::Json => serde_json::from_str(text).ok(),
                Kind::Bool => Some(Value::Bool(text == "true")),
                _ => None,
            };
            parsed.unwrap_or_else(|| Value::String(text.to_owned()))
        };
        Value::Object(
            self.columns
                .iter()
                .map(|(name, kind, value)| {
                    let v = value.as_deref().map_or(Value::Null, |t| typed(*kind, t));
                    (name.clone(), v)
                })
                .collect(),
        )
    }

    /// The row as your own type, through its `Deserialize` (`#[model]`
    /// structs have one).
    ///
    /// ```no_run
    /// # #[derive(serde::Deserialize)] struct Post { id: i64, title: String }
    /// # async fn f(record: zapline::db::Record) -> zapline::Result<()> {
    /// let post: Post = record.parse()?;
    /// # Ok(()) }
    /// ```
    #[cfg(feature = "json")]
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.json())
            .map_err(|e| Error::internal(format!("a factory row does not fit the type: {e}")))
    }
}

fn insert<DB: AdminBackend>(
    pool: &Pool<DB>,
    table: String,
    values: Vec<(String, Option<String>)>,
    depth: usize,
) -> Pin<Box<dyn Future<Output = Result<Record>> + Send + '_>> {
    Box::pin(async move {
        if !is_ident(&table) {
            return Err(Error::internal(format!("not a table name: {table:?}")));
        }
        let columns = DB::columns(pool, &table).await?;
        if columns.is_empty() {
            return Err(Error::internal(format!("no table named {table}")));
        }
        if let Some((unknown, _)) = values
            .iter()
            .find(|(name, _)| !columns.iter().any(|c| c.name == *name))
        {
            return Err(Error::internal(format!("{table} has no column {unknown}")));
        }
        let foreign_keys = DB::foreign_keys(pool, &table).await?;

        let mut chosen: Vec<(&Column, Option<String>)> = Vec::new();
        for column in &columns {
            if let Some((_, value)) = values.iter().find(|(name, _)| *name == column.name) {
                chosen.push((column, value.clone()));
                continue;
            }
            if column.nullable || column.has_default || column.generated {
                continue;
            }
            let fk = foreign_keys
                .iter()
                .find(|(from, _, _)| *from == column.name);
            let value = match fk {
                Some((_, parent, parent_column)) => {
                    if depth >= MAX_DEPTH {
                        return Err(Error::internal(format!(
                            "{table}.{} needs a {parent} row: set it, the required \
                             foreign keys go deeper than {MAX_DEPTH} tables",
                            column.name
                        )));
                    }
                    let row = insert(pool, parent.clone(), Vec::new(), depth + 1).await?;
                    let key = match parent_column {
                        Some(c) => c.clone(),
                        None => primary_key(pool, parent).await?,
                    };
                    row.get(&key).map(str::to_owned)
                }
                None => Some(fake(column)),
            };
            chosen.push((column, value));
        }

        let returning = columns
            .iter()
            .map(|c| format!("CAST({} AS TEXT)", quote(&c.name)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = Stmt::default();
        if chosen.is_empty() {
            stmt.sql(format!("INSERT INTO {} DEFAULT VALUES", quote(&table)));
        } else {
            let names: Vec<String> = chosen.iter().map(|(c, _)| quote(&c.name)).collect();
            stmt.sql(format!(
                "INSERT INTO {} ({}) VALUES (",
                quote(&table),
                names.join(", ")
            ));
            for (i, (column, value)) in chosen.into_iter().enumerate() {
                if i > 0 {
                    stmt.sql(", ");
                }
                stmt.bind::<DB>(value, column);
            }
            stmt.sql(")");
        }
        stmt.sql(format!(" RETURNING {returning}"));
        let row = DB::rows(pool, &stmt, columns.len())
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::internal(format!("inserting into {table} returned no row")))?;
        Ok(Record {
            columns: columns
                .iter()
                .zip(row)
                .map(|(c, value)| {
                    let value = match c.kind {
                        Kind::Bool => value.map(|v| bool_text(&v).to_owned()),
                        _ => value,
                    };
                    (c.name.clone(), c.kind, value)
                })
                .collect(),
        })
    })
}

/// SQLite returns booleans as `1`/`0`, Postgres as `true`/`false`.
fn bool_text(v: &str) -> &'static str {
    if matches!(v, "1" | "t" | "true") {
        "true"
    } else {
        "false"
    }
}

async fn primary_key<DB: AdminBackend>(pool: &Pool<DB>, table: &str) -> Result<String> {
    DB::columns(pool, table)
        .await?
        .into_iter()
        .find(|c| c.pk)
        .map(|c| c.name)
        .ok_or_else(|| Error::internal(format!("{table} has no primary key to refer to")))
}

/// A value for a required column nobody set.
fn fake(column: &Column) -> String {
    let n = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = column.name.to_ascii_lowercase();
    let ty = column.ty.to_ascii_lowercase();
    match column.kind {
        Kind::Int => n.to_string(),
        Kind::Float => format!("{n}.5"),
        Kind::Bool => "false".into(),
        Kind::Json => "{}".into(),
        Kind::Binary => String::new(),
        Kind::Time => {
            let now = now();
            if ty.starts_with("time") && !ty.starts_with("timestamp") {
                now[11..].to_owned()
            } else if ty == "date" {
                now[..10].to_owned()
            } else {
                now
            }
        }
        Kind::Text if name.contains("email") => format!("user{n}@example.com"),
        Kind::Text if name.ends_with("url") => format!("https://example.com/{n}"),
        Kind::Text => format!("{} {n}", column.name.replace('_', " ")),
    }
}

/// The current UTC time as `YYYY-MM-DD HH:MM:SS`.
fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_a_timestamp() {
        let t = now();
        assert_eq!(t.len(), 19);
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[10..11], " ");
        assert!(t.starts_with("20"));
    }
}
