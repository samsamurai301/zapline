//! Shared pieces of the TechEmpower-style database tests, so every framework
//! runs the exact same queries through the same driver (sqlx, Postgres) and
//! only the HTTP layer differs.

use std::fmt::Write;

use serde::Serialize;
use sqlx::{PgConnection, PgPool, Row};

/// `GET /db`, `/queries`, `/updates` row: `{"id":..,"randomNumber":..}`.
#[derive(Serialize, Clone, Copy)]
pub struct World {
    pub id: i32,
    #[serde(rename = "randomNumber")]
    pub random_number: i32,
}

/// Rows in the `World` table.
const WORLD_ROWS: i32 = 10_000;

/// A random `World` id, 1..=10000.
#[inline]
pub fn random_id() -> i32 {
    fastrand::i32(1..=WORLD_ROWS)
}

/// The `queries` parameter as the TechEmpower rules want it: missing or not a
/// number means 1, and it is clamped to 1..=500.
pub fn query_count(query: Option<&str>) -> usize {
    query
        .unwrap_or("")
        .split('&')
        .find_map(|kv| kv.strip_prefix("queries="))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, 500) as usize
}

/// Database URL: `DATABASE_URL`, or the harness's local Postgres.
pub fn database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://benchmarkdbuser:benchmarkdbpass@127.0.0.1:55432/hello_world".into()
    })
}

/// Total pool size, shared by every worker (`POOL`, default 32).
pub fn pool_size() -> u32 {
    std::env::var("POOL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32)
}

pub fn workers() -> usize {
    std::env::var("WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

const SELECT_WORLD: &str = "SELECT id, randomnumber FROM world WHERE id = $1";

async fn fetch_world(conn: &mut PgConnection, id: i32) -> sqlx::Result<World> {
    let row = sqlx::query(SELECT_WORLD)
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    Ok(World {
        id: row.get(0),
        random_number: row.get(1),
    })
}

/// `GET /db`: one random row.
pub async fn single_query(pool: &PgPool) -> sqlx::Result<World> {
    let mut conn = pool.acquire().await?;
    fetch_world(&mut conn, random_id()).await
}

/// `GET /queries?queries=n`: `n` random rows, one query each.
pub async fn multiple_queries(pool: &PgPool, n: usize) -> sqlx::Result<Vec<World>> {
    let mut conn = pool.acquire().await?;
    let mut worlds = Vec::with_capacity(n);
    for _ in 0..n {
        worlds.push(fetch_world(&mut conn, random_id()).await?);
    }
    Ok(worlds)
}

/// `GET /updates?queries=n`: read `n` random rows one by one, give each a new
/// random number, and write them back in one batched statement.
pub async fn updates(pool: &PgPool, n: usize) -> sqlx::Result<Vec<World>> {
    let mut conn = pool.acquire().await?;
    let mut worlds = Vec::with_capacity(n);
    for _ in 0..n {
        let mut w = fetch_world(&mut conn, random_id()).await?;
        w.random_number = random_id();
        worlds.push(w);
    }
    // Lock rows in id order so concurrent batches cannot deadlock.
    let mut sorted = worlds.clone();
    sorted.sort_unstable_by_key(|w| w.id);
    let ids: Vec<i32> = sorted.iter().map(|w| w.id).collect();
    let numbers: Vec<i32> = sorted.iter().map(|w| w.random_number).collect();
    sqlx::query(
        "UPDATE world SET randomnumber = t.r \
         FROM UNNEST($1::int[], $2::int[]) AS t(id, r) WHERE world.id = t.id",
    )
    .bind(ids)
    .bind(numbers)
    .execute(&mut *conn)
    .await?;
    Ok(worlds)
}

/// `GET /fortunes`: every fortune plus one added at request time, sorted by
/// message, rendered as an escaped HTML table.
pub async fn fortunes(pool: &PgPool) -> sqlx::Result<String> {
    let mut conn = pool.acquire().await?;
    let rows = sqlx::query("SELECT id, message FROM fortune")
        .fetch_all(&mut *conn)
        .await?;
    let mut fortunes: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    fortunes.push((0, "Additional fortune added at request time.".into()));
    fortunes.sort_unstable_by(|a, b| a.1.cmp(&b.1));

    let mut html = String::with_capacity(2048);
    html.push_str(
        "<!DOCTYPE html><html><head><title>Fortunes</title></head><body>\
         <table><tr><th>id</th><th>message</th></tr>",
    );
    for (id, message) in &fortunes {
        let _ = write!(html, "<tr><td>{id}</td><td>");
        escape_into(&mut html, message);
        html.push_str("</td></tr>");
    }
    html.push_str("</table></body></html>");
    Ok(html)
}

fn escape_into(out: &mut String, s: &str) {
    let mut last = 0;
    for (i, b) in s.bytes().enumerate() {
        let rep = match b {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            b'\'' => "&#x27;",
            _ => continue,
        };
        out.push_str(&s[last..i]);
        out.push_str(rep);
        last = i + 1;
    }
    out.push_str(&s[last..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_count_follows_the_rules() {
        assert_eq!(query_count(None), 1);
        assert_eq!(query_count(Some("queries=foo")), 1);
        assert_eq!(query_count(Some("queries=0")), 1);
        assert_eq!(query_count(Some("queries=20")), 20);
        assert_eq!(query_count(Some("x=1&queries=501")), 500);
    }

    #[test]
    fn escapes_html() {
        let mut s = String::new();
        escape_into(&mut s, "<script>alert(\"x\" & 'y')</script>");
        assert_eq!(
            s,
            "&lt;script&gt;alert(&quot;x&quot; &amp; &#x27;y&#x27;)&lt;/script&gt;"
        );
    }
}
