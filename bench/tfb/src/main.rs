//! zapline's implementation of the TechEmpower Framework Benchmarks
//! (<https://github.com/TechEmpower/FrameworkBenchmarks>): plaintext, JSON,
//! single query, multiple queries, fortunes, updates and cached queries
//! against the TFB Postgres schema.
//!
//! Environment:
//!
//! | variable | default | |
//! |---|---|---|
//! | `DATABASE_URL` | `postgres://benchmarkdbuser:benchmarkdbpass@tfb-database/hello_world` | TFB's database host |
//! | `BIND` | `0.0.0.0:8080` | |
//! | `WORKERS` | CPU count | one single-threaded runtime per worker |
//! | `DB_POOL` | `per-worker` | `per-worker`, or `shared` for `App::database` + `Db` |
//! | `DB_POOL_SIZE` | `16` | connections per worker (`shared`: × workers, in one pool) |
//! | `DB_PING` | `0` | `1` pings each connection on checkout (sqlx's default, kept by `App::database`) |
//! | `TEMPLATES` | `./templates` | where `fortunes.html` lives |
//!
//! # Why per-worker pools
//!
//! zapline runs one single-threaded Tokio runtime per core. A sqlx
//! connection's socket belongs to the runtime that opened it, so with one
//! shared pool (what `App::database` gives you) a worker usually waits on a
//! socket driven by another thread and every query costs cross-thread
//! wake-ups. Here each worker lazily opens its own small pool, so a request
//! and its database I/O stay on one core. `DB_POOL=shared` runs the
//! canonical `App::database` + `Db` path instead, for comparison; see
//! `results.md` for both.

use std::cell::OnceCell;
use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;
use zapline::cache::Cache;
use zapline::db::sqlx::pool::PoolOptions;
use zapline::db::sqlx::postgres::{PgConnectOptions, PgPool};
use zapline::db::sqlx::{self, ConnectOptions, Connection, PgConnection};
use zapline::db::{Database, Db};
use zapline::extract::FromRequestParts;
use zapline::header::{HeaderName, SERVER};
use zapline::prelude::*;
use zapline::view::{context, Views};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEFAULT_DATABASE_URL: &str =
    "postgres://benchmarkdbuser:benchmarkdbpass@tfb-database/hello_world";
const WORLD_ROWS: i32 = 10_000;
/// Cached worlds never expire during a run.
const CACHE_TTL: Duration = Duration::from_secs(365 * 24 * 3600);

const SELECT_WORLD: &str = "SELECT id, randomnumber FROM world WHERE id = $1";
const SELECT_FORTUNES: &str = "SELECT id, message FROM fortune";
/// One statement for the whole batch; ids are sorted so concurrent updates
/// lock rows in the same order and cannot deadlock.
/// (No `SELECT` in it: TFB counts rows read from statements that say
/// `select`.)
const UPDATE_WORLDS: &str = "UPDATE world SET randomnumber = v.r \
     FROM unnest($1::int[], $2::int[]) AS v(i, r) WHERE world.id = v.i";

// ---------------------------------------------------------------------------
// Models

#[derive(Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq)]
struct World {
    id: i32,
    #[serde(rename = "randomNumber")]
    random_number: i32,
}

#[derive(Serialize)]
struct Fortune {
    id: i32,
    message: String,
}

#[derive(Serialize)]
struct Message {
    message: &'static str,
}

// ---------------------------------------------------------------------------
// Database access: one pool per worker thread, or the shared `Db` pool

/// How handlers reach Postgres. Registered as state at startup.
#[derive(Clone)]
enum PoolMode {
    /// A lazily created pool per worker thread (see the module docs).
    PerWorker {
        options: Box<PgConnectOptions>,
        size: u32,
        ping: bool,
    },
    /// The pool `App::database` registers, read through `Db`.
    Shared,
}

thread_local! {
    static WORKER_POOL: OnceCell<PgPool> = const { OnceCell::new() };
}

/// The pool for the current request: an extractor over [`PoolMode`].
struct Pg(PgPool);

impl FromRequestParts for Pg {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        match req.state::<PoolMode>() {
            Some(PoolMode::PerWorker {
                options,
                size,
                ping,
            }) => Ok(Pg(WORKER_POOL.with(|cell| {
                cell.get_or_init(|| {
                    pool_options(*size, *ping).connect_lazy_with((**options).clone())
                })
                .clone()
            }))),
            Some(PoolMode::Shared) => Db::from_request_parts(req).await.map(|db| Pg(db.0)),
            None => Err(Error::internal("no PoolMode registered")),
        }
    }
}

fn pool_options(size: u32, ping: bool) -> PoolOptions<sqlx::Postgres> {
    PoolOptions::new()
        .max_connections(size)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(10))
        // A ping per checkout is one more round trip per request; a broken
        // connection surfaces as a failed query instead.
        .test_before_acquire(ping)
}

// ---------------------------------------------------------------------------
// Helpers

/// TFB requires a `Server` header on every response.
fn tfb<T: IntoResponse>(body: T) -> impl IntoResponse {
    const NAME: HeaderValue = HeaderValue::from_static("zapline");
    let headers: [(HeaderName, HeaderValue); 1] = [(SERVER, NAME)];
    (headers, body)
}

/// The `name` query parameter as a TFB query count: missing, empty, zero,
/// negative or non-numeric means 1, and anything above 500 means 500.
fn count(uri: &Uri, name: &str) -> usize {
    let value = uri.query().and_then(|q| {
        q.split('&')
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    });
    match value {
        Some(v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => {
            // All digits but too long for u64 is still "more than 500".
            v.parse::<u64>().map_or(500, |n| n.clamp(1, 500) as usize)
        }
        _ => 1,
    }
}

fn random_id() -> i32 {
    fastrand::i32(1..=WORLD_ROWS)
}

async fn fetch_world(conn: &mut PgConnection, id: i32) -> Result<World> {
    let (id, random_number) = sqlx::query_as::<_, (i32, i32)>(SELECT_WORLD)
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    Ok(World { id, random_number })
}

// ---------------------------------------------------------------------------
// Handlers

async fn plaintext() -> impl IntoResponse {
    tfb("Hello, World!")
}

async fn json() -> impl IntoResponse {
    tfb(Json(Message {
        message: "Hello, World!",
    }))
}

async fn db(Pg(pool): Pg) -> Result<impl IntoResponse> {
    let mut conn = pool.acquire().await?;
    let world = fetch_world(&mut conn, random_id()).await?;
    Ok(tfb(Json(world)))
}

async fn queries(Pg(pool): Pg, uri: Uri) -> Result<impl IntoResponse> {
    let n = count(&uri, "queries");
    let mut conn = pool.acquire().await?;
    let mut worlds = Vec::with_capacity(n);
    for _ in 0..n {
        worlds.push(fetch_world(&mut conn, random_id()).await?);
    }
    Ok(tfb(Json(worlds)))
}

async fn fortunes(Pg(pool): Pg, views: Views) -> Result<impl IntoResponse> {
    let mut fortunes: Vec<Fortune> = sqlx::query_as::<_, (i32, String)>(SELECT_FORTUNES)
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|(id, message)| Fortune { id, message })
        .collect();
    fortunes.push(Fortune {
        id: 0,
        message: "Additional fortune added at request time.".to_owned(),
    });
    fortunes.sort_unstable_by(|a, b| a.message.cmp(&b.message));
    Ok(tfb(views.render("fortunes.html", context! { fortunes })?))
}

async fn updates(Pg(pool): Pg, uri: Uri) -> Result<impl IntoResponse> {
    let n = count(&uri, "queries");
    let mut conn = pool.acquire().await?;
    let mut worlds = Vec::with_capacity(n);
    for _ in 0..n {
        let mut world = fetch_world(&mut conn, random_id()).await?;
        world.random_number = random_id();
        worlds.push(world);
    }
    // A row that was drawn twice gets the same new number both times, so
    // the response matches what is stored.
    let mut batch: Vec<(i32, i32)> = Vec::with_capacity(n);
    for w in &mut worlds {
        match batch.iter().find(|(id, _)| *id == w.id) {
            Some(&(_, r)) => w.random_number = r,
            None => batch.push((w.id, w.random_number)),
        }
    }
    batch.sort_unstable_by_key(|(id, _)| *id);
    let (ids, numbers): (Vec<i32>, Vec<i32>) = batch.into_iter().unzip();
    sqlx::query(UPDATE_WORLDS)
        .bind(ids)
        .bind(numbers)
        .execute(&mut *conn)
        .await?;
    Ok(tfb(Json(worlds)))
}

async fn cached_queries(cache: Cache, uri: Uri) -> Result<impl IntoResponse> {
    let n = count(&uri, "count");
    let mut worlds = Vec::with_capacity(n);
    for _ in 0..n {
        let id = random_id();
        let world = cache
            .get::<World>(&world_key(id))
            .await
            .ok_or_else(|| Error::internal(format!("world {id} is not cached")))?;
        worlds.push(world);
    }
    Ok(tfb(Json(worlds)))
}

fn world_key(id: i32) -> String {
    format!("world:{id}")
}

// ---------------------------------------------------------------------------
// App

fn env_or<T: FromStr>(var: &str, default: T) -> T {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn app() -> std::io::Result<App> {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_owned());
    let options = PgConnectOptions::from_str(&url)
        .map_err(|e| std::io::Error::other(format!("bad DATABASE_URL: {e}")))?
        .disable_statement_logging();
    let workers = env_or(
        "WORKERS",
        std::thread::available_parallelism().map_or(1, |n| n.get()),
    );
    let size = env_or("DB_POOL_SIZE", 16u32).max(1);
    let shared = match std::env::var("DB_POOL").as_deref() {
        Ok("shared") => true,
        Ok("per-worker") | Err(_) => false,
        Ok(other) => {
            return Err(std::io::Error::other(format!(
                "DB_POOL must be `per-worker` or `shared`, not `{other}`"
            )))
        }
    };
    let ping = env_or("DB_PING", 0u8) == 1;
    let templates = std::env::var("TEMPLATES").unwrap_or_else(|_| "templates".to_owned());
    let cache = Cache::memory().max_entries(WORLD_ROWS as usize);

    let mut app = App::new()
        .workers(workers)
        .state(Views::new(templates, false))
        .cache(cache.clone())
        .get("/plaintext", plaintext)
        .get("/json", json)
        .get("/db", db)
        .get("/queries", queries)
        .get("/fortunes", fortunes)
        .get("/updates", updates)
        .get("/cached-queries", cached_queries);
    if shared {
        app = app.state(PoolMode::Shared).database(
            Database::url(url)
                .max_connections(size * workers as u32)
                .migrate(false),
        );
    } else {
        app = app.state(PoolMode::PerWorker {
            options: Box::new(options.clone()),
            size,
            ping,
        });
    }
    // Fail fast if the database is unreachable, and fill the cache.
    Ok(app.on_startup(move |_| async move {
        let mut conn = PgConnection::connect_with(&options).await?;
        let rows = sqlx::query_as::<_, (i32, i32)>("SELECT id, randomnumber FROM world")
            .fetch_all(&mut conn)
            .await?;
        for (id, random_number) in rows {
            let world = World { id, random_number };
            cache.set(&world_key(id), &world, CACHE_TTL).await?;
        }
        conn.close().await?;
        Ok(())
    }))
}

fn main() -> std::io::Result<()> {
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    app()?.run(bind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(query: &str, name: &str) -> usize {
        count(&format!("/x?{query}").parse().unwrap(), name)
    }

    #[test]
    fn query_counts_are_clamped_like_tfb_expects() {
        assert_eq!(n("queries=20", "queries"), 20);
        assert_eq!(n("queries=0", "queries"), 1);
        assert_eq!(n("queries=-5", "queries"), 1);
        assert_eq!(n("queries=", "queries"), 1);
        assert_eq!(n("queries=foo", "queries"), 1);
        assert_eq!(n("queries=501", "queries"), 500);
        assert_eq!(n("queries=99999999999999999999999", "queries"), 500);
        assert_eq!(n("a=1&count=7", "count"), 7);
        assert_eq!(n("count=7", "queries"), 1);
        assert_eq!(count(&"/x".parse().unwrap(), "queries"), 1);
    }
}
