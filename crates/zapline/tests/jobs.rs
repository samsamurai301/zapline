//! Durable jobs on SQLite (and on Postgres when `ZAPLINE_TEST_POSTGRES_URL`
//! is set).
#![cfg(all(feature = "sqlite", feature = "json"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zapline::db::sqlx::{self, Sqlite};
use zapline::db::Database;
use zapline::jobs::{Job, JobBackend, JobQueue, JobStats, Jobs};
use zapline::prelude::*;

/// Counts runs, shared with the test through app state.
#[derive(Clone, Default)]
struct Runs(Arc<AtomicUsize>);

impl Runs {
    fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Serialize, Deserialize)]
struct Add {
    n: usize,
}

impl Job for Add {
    const NAME: &'static str = "add";

    async fn run(self, res: Resources) -> Result<()> {
        res.require::<Runs>()?.0.fetch_add(self.n, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Flaky;

impl Job for Flaky {
    const NAME: &'static str = "flaky";
    const MAX_ATTEMPTS: u32 = 2;

    async fn run(self, res: Resources) -> Result<()> {
        res.require::<Runs>()?.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::bad_request("smtp is down"))
    }
}

#[derive(Serialize, Deserialize)]
struct Panics;

impl Job for Panics {
    const NAME: &'static str = "panics";
    const MAX_ATTEMPTS: u32 = 1;

    async fn run(self, _res: Resources) -> Result<()> {
        panic!("boom")
    }
}

fn app(runs: &Runs, workers: usize) -> App {
    App::new()
        .state(runs.clone())
        .database(Database::sqlite_memory())
        .jobs_in::<Sqlite>(|q| {
            q.register::<Add>()
                .register::<Flaky>()
                .register::<Panics>()
                .workers(workers)
                .poll_interval(Duration::from_millis(20))
        })
}

async fn jobs_of(client: &zapline::test::TestClient) -> Jobs<Sqlite> {
    client.resources().await.require().unwrap()
}

#[tokio::test]
async fn run_pending_runs_due_jobs_once() {
    let runs = Runs::default();
    let client = app(&runs, 0).test();
    let jobs = jobs_of(&client).await;

    jobs.enqueue(Add { n: 2 }).await.unwrap();
    jobs.enqueue(Add { n: 3 }).await.unwrap();
    assert_eq!(jobs.stats().await.unwrap().queued, 2);

    assert_eq!(jobs.run_pending().await.unwrap(), 2);
    assert_eq!(runs.get(), 5);
    assert_eq!(
        jobs.run_pending().await.unwrap(),
        0,
        "finished jobs are deleted"
    );
    assert_eq!(jobs.stats().await.unwrap(), JobStats::default());
}

#[tokio::test]
async fn delayed_jobs_wait_until_due() {
    let runs = Runs::default();
    let client = app(&runs, 0).test();
    let jobs = jobs_of(&client).await;

    jobs.enqueue_in(Add { n: 1 }, Duration::from_millis(150))
        .await
        .unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(jobs.run_pending().await.unwrap(), 1);
    assert_eq!(runs.get(), 1);
}

async fn last_error(jobs_db: &sqlx::Pool<Sqlite>) -> (String, i64, Option<String>) {
    sqlx::query_as("SELECT status, attempts, last_error FROM zapline_jobs")
        .fetch_one(jobs_db)
        .await
        .unwrap()
}

#[tokio::test]
async fn failures_back_off_then_fail_and_can_be_retried() {
    let runs = Runs::default();
    let client = app(&runs, 0).test();
    let jobs = jobs_of(&client).await;
    let pool: sqlx::Pool<Sqlite> = client.resources().await.require().unwrap();

    jobs.enqueue(Flaky).await.unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 1);
    let (status, attempts, error) = last_error(&pool).await;
    assert_eq!((status.as_str(), attempts), ("queued", 1));
    assert!(error.unwrap().contains("smtp is down"));
    // The retry is ~5 s away, so nothing is due yet.
    assert_eq!(jobs.run_pending().await.unwrap(), 0);

    // Make it due, run the last attempt: now it is failed for good.
    sqlx::query("UPDATE zapline_jobs SET run_at = 0")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 1);
    assert_eq!(last_error(&pool).await.0, "failed");
    assert_eq!(jobs.stats().await.unwrap().failed, 1);
    assert_eq!(runs.get(), 2);

    assert_eq!(jobs.retry_failed().await.unwrap(), 1);
    assert_eq!(jobs.stats().await.unwrap().queued, 1);
    assert_eq!(jobs.run_pending().await.unwrap(), 1);
    assert_eq!(runs.get(), 3);
}

#[tokio::test]
async fn panics_count_as_failures() {
    let runs = Runs::default();
    let client = app(&runs, 0).test();
    let jobs = jobs_of(&client).await;
    let pool: sqlx::Pool<Sqlite> = client.resources().await.require().unwrap();

    jobs.enqueue(Panics).await.unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 1);
    let (status, _, error) = last_error(&pool).await;
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some("panicked: boom"));
}

#[tokio::test]
async fn unregistered_jobs_are_rejected_at_enqueue() {
    #[derive(Serialize, Deserialize)]
    struct Unknown;
    impl Job for Unknown {
        const NAME: &'static str = "unknown";
        async fn run(self, _res: Resources) -> Result<()> {
            Ok(())
        }
    }

    let client = app(&Runs::default(), 0).test();
    let err = jobs_of(&client).await.enqueue(Unknown).await.unwrap_err();
    assert!(err.message().contains("register::<"), "{}", err.message());
}

#[tokio::test]
async fn workers_run_jobs_enqueued_by_handlers() {
    async fn signup(jobs: Jobs<Sqlite>) -> Result<StatusCode> {
        jobs.enqueue(Add { n: 1 }).await?;
        Ok(StatusCode::ACCEPTED)
    }

    let runs = Runs::default();
    let client = app(&runs, 2).post("/signup", signup).test();
    for _ in 0..3 {
        client.post("/signup").send().await.assert_status(202);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while runs.get() < 3 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(runs.get(), 3);
    client.shutdown().await;
}

#[tokio::test]
async fn jobs_need_a_database() {
    let client = App::new()
        .jobs_in::<Sqlite>(|q| q.register::<Add>().workers(0))
        .test();
    let started = tokio::spawn(async move { client.start().await.routes().len() }).await;
    let panic = started.unwrap_err().into_panic();
    let msg = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(
        msg.contains("call `.database(..)` before `.jobs(..)`"),
        "{msg}"
    );
}

/// Two queues on one Postgres database never run the same job twice.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_workers_share_one_queue() {
    use zapline::db::sqlx::Postgres;

    let Ok(url) = std::env::var("ZAPLINE_TEST_POSTGRES_URL") else {
        return;
    };
    let runs = Runs::default();
    let make = || {
        App::new()
            .state(runs.clone())
            .database(Database::postgres(url.clone()).migrate(false))
            .jobs_in::<Postgres>(|q| q.register::<Add>().workers(0))
            .test()
    };
    let (a, b) = (make(), make());
    let ja: Jobs<Postgres> = a.resources().await.require().unwrap();
    let jb: Jobs<Postgres> = b.resources().await.require().unwrap();
    let pool: sqlx::Pool<Postgres> = a.resources().await.require().unwrap();
    sqlx::query("DELETE FROM zapline_jobs WHERE name = 'add'")
        .execute(&pool)
        .await
        .unwrap();

    for _ in 0..20 {
        ja.enqueue(Add { n: 1 }).await.unwrap();
    }
    let (ra, rb) = tokio::join!(ja.run_pending(), jb.run_pending());
    assert_eq!(ra.unwrap() + rb.unwrap(), 20);
    assert_eq!(runs.get(), 20);
}

// ---------------------------------------------------------------------------
// Unique keys, priorities, named queues, scheduling and discarding

/// The names of the jobs that ran, in order.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn push(&self, entry: String) {
        self.0.lock().unwrap().push(entry);
    }

    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Serialize, Deserialize)]
struct Reindex {
    product: i64,
}

impl Job for Reindex {
    const NAME: &'static str = "reindex";

    fn unique_key(&self) -> Option<String> {
        Some(self.product.to_string())
    }

    async fn run(self, res: Resources) -> Result<()> {
        res.require::<Log>()?
            .push(format!("reindex {}", self.product));
        Ok(())
    }
}

/// Same key space as `Reindex`, different job type.
#[derive(Serialize, Deserialize)]
struct Refresh {
    product: i64,
}

impl Job for Refresh {
    const NAME: &'static str = "refresh";

    fn unique_key(&self) -> Option<String> {
        Some(self.product.to_string())
    }

    async fn run(self, _res: Resources) -> Result<()> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Urgent(String);

impl Job for Urgent {
    const NAME: &'static str = "urgent";
    const PRIORITY: i32 = 10;

    async fn run(self, res: Resources) -> Result<()> {
        res.require::<Log>()?.push(format!("urgent {}", self.0));
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Normal(String);

impl Job for Normal {
    const NAME: &'static str = "normal";

    async fn run(self, res: Resources) -> Result<()> {
        res.require::<Log>()?.push(format!("normal {}", self.0));
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Whenever(String);

impl Job for Whenever {
    const NAME: &'static str = "whenever";
    const PRIORITY: i32 = -5;

    async fn run(self, res: Resources) -> Result<()> {
        res.require::<Log>()?.push(format!("whenever {}", self.0));
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Mail(String);

impl Job for Mail {
    const NAME: &'static str = "mail";
    const QUEUE: &'static str = "mail";

    async fn run(self, res: Resources) -> Result<()> {
        res.require::<Log>()?.push(format!("mail {}", self.0));
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct BrokenUnique {
    key: String,
}

impl Job for BrokenUnique {
    const NAME: &'static str = "broken_unique";
    const MAX_ATTEMPTS: u32 = 1;

    fn unique_key(&self) -> Option<String> {
        Some(self.key.clone())
    }

    async fn run(self, _res: Resources) -> Result<()> {
        Err(Error::bad_request("always fails"))
    }
}

fn ops_app(log: &Log, configure: impl FnOnce(JobQueue) -> JobQueue) -> App {
    App::new()
        .state(log.clone())
        .state(Runs::default())
        .database(Database::sqlite_memory())
        .jobs_in::<Sqlite>(|q| {
            configure(
                q.register::<Reindex>()
                    .register::<Refresh>()
                    .register::<Urgent>()
                    .register::<Normal>()
                    .register::<Whenever>()
                    .register::<Mail>()
                    .register::<BrokenUnique>()
                    .register::<Flaky>()
                    .poll_interval(Duration::from_millis(20)),
            )
        })
}

async fn wait_for(what: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !what() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_unique_key_is_enqueued_once_while_it_waits() {
    let log = Log::default();
    let client = ops_app(&log, |q| q.workers(0)).test();
    let jobs = jobs_of(&client).await;

    assert!(jobs.enqueue(Reindex { product: 1 }).await.unwrap());
    assert!(!jobs.enqueue(Reindex { product: 1 }).await.unwrap());
    assert!(!jobs
        .enqueue_in(Reindex { product: 1 }, Duration::from_secs(60))
        .await
        .unwrap());
    assert!(jobs.enqueue(Reindex { product: 2 }).await.unwrap());
    // Keys are per job type.
    assert!(jobs.enqueue(Refresh { product: 1 }).await.unwrap());
    assert_eq!(jobs.stats().await.unwrap().queued, 3);

    assert_eq!(jobs.run_pending().await.unwrap(), 3);
    assert_eq!(log.entries(), ["reindex 1", "reindex 2"]);
    // Once it ran, the key is free again.
    assert!(jobs.enqueue(Reindex { product: 1 }).await.unwrap());
}

#[tokio::test]
async fn a_failed_unique_job_frees_its_key_and_stays_failed_on_retry() {
    let log = Log::default();
    let client = ops_app(&log, |q| q.workers(0)).test();
    let jobs = jobs_of(&client).await;

    let job = || BrokenUnique { key: "a".into() };
    assert!(jobs.enqueue(job()).await.unwrap());
    jobs.run_pending().await.unwrap();
    assert_eq!(jobs.stats().await.unwrap().failed, 1);
    // The failed job no longer holds the key.
    assert!(jobs.enqueue(job()).await.unwrap());
    // Retrying would make two live jobs with one key: the old one stays failed.
    assert_eq!(jobs.retry_failed().await.unwrap(), 0);
    jobs.run_pending().await.unwrap();
    // Two failed with the same key: only the newest is requeued.
    assert_eq!(jobs.stats().await.unwrap().failed, 2);
    assert_eq!(jobs.retry_failed().await.unwrap(), 1);
    let stats = jobs.stats().await.unwrap();
    assert_eq!((stats.queued, stats.failed), (1, 1));
}

#[tokio::test]
async fn higher_priority_jobs_are_claimed_first() {
    let log = Log::default();
    let client = ops_app(&log, |q| q.workers(0)).test();
    let jobs = jobs_of(&client).await;

    jobs.enqueue(Whenever("a".into())).await.unwrap();
    jobs.enqueue(Normal("b".into())).await.unwrap();
    jobs.enqueue(Urgent("c".into())).await.unwrap();
    jobs.enqueue(Normal("d".into())).await.unwrap();
    jobs.enqueue(Urgent("e".into())).await.unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 5);
    assert_eq!(
        log.entries(),
        ["urgent c", "urgent e", "normal b", "normal d", "whenever a"]
    );
}

#[tokio::test]
async fn queues_get_their_own_workers() {
    let log = Log::default();
    // Only the mail queue is worked by this instance.
    let client = ops_app(&log, |q| q.queues([("mail", 1)])).test();
    let jobs = jobs_of(&client).await;

    jobs.enqueue(Normal("waits".into())).await.unwrap();
    jobs.enqueue(Mail("sent".into())).await.unwrap();
    wait_for(|| log.entries().contains(&"mail sent".to_string())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(log.entries(), ["mail sent"]);
    assert_eq!(
        jobs.stats().await.unwrap().queued,
        1,
        "default is untouched"
    );
    client.shutdown().await;

    // Every listed queue is worked, a bare name with `.workers(n)` workers.
    let log = Log::default();
    let client = ops_app(&log, |q| q.queues(["default", "mail"]).workers(1)).test();
    let jobs = jobs_of(&client).await;
    jobs.enqueue(Normal("one".into())).await.unwrap();
    jobs.enqueue(Mail("two".into())).await.unwrap();
    wait_for(|| log.entries().len() == 2).await;
    let mut ran = log.entries();
    ran.sort();
    assert_eq!(ran, ["mail two", "normal one"]);
    client.shutdown().await;
}

#[tokio::test]
async fn without_named_queues_workers_take_every_queue() {
    let log = Log::default();
    let client = ops_app(&log, |q| q.workers(1)).test();
    let jobs = jobs_of(&client).await;
    jobs.enqueue(Mail("hi".into())).await.unwrap();
    wait_for(|| !log.entries().is_empty()).await;
    assert_eq!(log.entries(), ["mail hi"]);
    client.shutdown().await;
}

#[tokio::test]
async fn enqueue_at_runs_once_the_time_comes() {
    use std::time::SystemTime;

    let log = Log::default();
    let client = ops_app(&log, |q| q.workers(0)).test();
    let jobs = jobs_of(&client).await;

    let soon = SystemTime::now() + Duration::from_millis(150);
    jobs.enqueue_at(Normal("later".into()), soon).await.unwrap();
    let past = SystemTime::now() - Duration::from_secs(60);
    jobs.enqueue_at(Normal("overdue".into()), past)
        .await
        .unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 1);
    assert_eq!(log.entries(), ["normal overdue"]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(jobs.run_pending().await.unwrap(), 1);
    assert_eq!(log.entries(), ["normal overdue", "normal later"]);
}

#[tokio::test]
async fn failed_jobs_are_listed_and_discarded() {
    let log = Log::default();
    let client = ops_app(&log, |q| q.workers(0)).test();
    let jobs = jobs_of(&client).await;

    jobs.enqueue(BrokenUnique { key: "k".into() })
        .await
        .unwrap();
    jobs.run_pending().await.unwrap();
    let failed = jobs.failed(10).await.unwrap();
    assert_eq!(failed.len(), 1);
    let job = &failed[0];
    assert_eq!(
        (job.name.as_str(), job.queue.as_str()),
        ("broken_unique", "default")
    );
    assert_eq!(job.attempts, 1);
    assert_eq!(job.payload, r#"{"key":"k"}"#);
    assert!(job.last_error.as_deref().unwrap().contains("always fails"));

    assert!(jobs.discard(job.id).await.unwrap());
    assert!(!jobs.discard(job.id).await.unwrap(), "already gone");
    assert!(jobs.failed(10).await.unwrap().is_empty());
    assert_eq!(jobs.stats().await.unwrap(), JobStats::default());

    // A queued job can be discarded (cancelled) too.
    jobs.enqueue_in(Normal("never".into()), Duration::from_secs(60))
        .await
        .unwrap();
    let pool: sqlx::Pool<Sqlite> = client.resources().await.require().unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM zapline_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(jobs.discard(id).await.unwrap());
    assert_eq!(jobs.stats().await.unwrap().queued, 0);
}

/// The table as zapline created it before queues, priorities and unique keys.
const OLD_SCHEMA: [&str; 3] = [
    "CREATE TABLE zapline_jobs (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
     payload TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued', attempts BIGINT NOT NULL DEFAULT 0, \
     max_attempts BIGINT NOT NULL, run_at BIGINT NOT NULL, locked_until BIGINT NOT NULL DEFAULT 0, \
     last_error TEXT, created_at BIGINT NOT NULL)",
    "CREATE INDEX zapline_jobs_due ON zapline_jobs (status, run_at)",
    "INSERT INTO zapline_jobs (name, payload, max_attempts, run_at, created_at) \
     VALUES ('normal', '\"from before\"', 5, 0, 0)",
];

#[tokio::test]
async fn an_old_jobs_table_is_upgraded_in_place() {
    let log = Log::default();
    let client = App::new()
        .state(log.clone())
        .database(Database::sqlite_memory())
        // Runs before the jobs' startup hook: the table an older zapline left.
        .on_startup(|res: Resources| async move {
            let pool: sqlx::Pool<Sqlite> = res.require()?;
            for sql in OLD_SCHEMA {
                sqlx::query(sql).execute(&pool).await?;
            }
            Ok(())
        })
        .jobs_in::<Sqlite>(|q| q.register::<Normal>().register::<Reindex>().workers(0))
        .test();
    let jobs = jobs_of(&client).await;
    let pool: sqlx::Pool<Sqlite> = client.resources().await.require().unwrap();

    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('zapline_jobs')")
            .fetch_all(&pool)
            .await
            .unwrap();
    for column in ["queue", "priority", "unique_key"] {
        assert!(columns.iter().any(|c| c == column), "{column}: {columns:?}");
    }
    let (queue, priority): (String, i64) =
        sqlx::query_as("SELECT queue, priority FROM zapline_jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((queue.as_str(), priority), ("default", 0));

    // The old row runs, and the new features work on the upgraded table.
    assert!(jobs.enqueue(Reindex { product: 9 }).await.unwrap());
    assert!(!jobs.enqueue(Reindex { product: 9 }).await.unwrap());
    assert_eq!(jobs.run_pending().await.unwrap(), 2);
    assert_eq!(log.entries(), ["normal from before", "reindex 9"]);
}

/// Unique keys, priorities and the upgrade on Postgres, in a database of
/// its own.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_unique_priority_and_upgrade() {
    use zapline::db::sqlx::{PgPool, Postgres};

    let Ok(base) = std::env::var("ZAPLINE_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = PgPool::connect(&base).await.unwrap();
    let name = format!(
        "zl_jobs_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.split('?').next().unwrap().rsplit_once('/').unwrap();
    let url = format!("{prefix}/{name}");
    {
        let pool = PgPool::connect(&url).await.unwrap();
        for sql in OLD_SCHEMA {
            let sql = sql.replace("INTEGER PRIMARY KEY AUTOINCREMENT", "BIGSERIAL PRIMARY KEY");
            sqlx::query(&sql).execute(&pool).await.unwrap();
        }
        pool.close().await;
    }

    let log = Log::default();
    let client = App::new()
        .state(log.clone())
        .database(Database::postgres(url).migrate(false))
        .jobs_in::<Postgres>(|q| {
            q.register::<Normal>()
                .register::<Urgent>()
                .register::<Reindex>()
                .workers(0)
        })
        .test();
    let jobs: Jobs<Postgres> = client.resources().await.require().unwrap();
    assert!(jobs.enqueue(Reindex { product: 1 }).await.unwrap());
    assert!(!jobs.enqueue(Reindex { product: 1 }).await.unwrap());
    jobs.enqueue(Urgent("first".into())).await.unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 3);
    assert_eq!(
        log.entries(),
        ["urgent first", "normal from before", "reindex 1"]
    );
    client.shutdown().await;
    drop(client);
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
}

// `JobBackend` is public so apps can name it in generic helpers.
#[allow(dead_code)]
fn takes_any_backend<DB: JobBackend>(_jobs: &Jobs<DB>) {}

// ---- metrics ------------------------------------------------------------------

/// Every job event shows up in `/metrics`, labelled by job name.
#[tokio::test]
async fn job_metrics_are_exported_when_the_metrics_plugin_is_on() {
    use zapline::metrics::{Metrics, Registry};

    let runs = Runs::default();
    let client = app(&runs, 0)
        .plugin(Metrics::new().registry(Registry::new()))
        .test();
    let jobs = jobs_of(&client).await;
    let pool: sqlx::Pool<Sqlite> = client.resources().await.require().unwrap();

    jobs.enqueue(Add { n: 1 }).await.unwrap();
    jobs.enqueue(Add { n: 2 }).await.unwrap();
    jobs.enqueue(Flaky).await.unwrap();
    jobs.enqueue(Panics).await.unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 4);
    // Flaky's second and last attempt.
    sqlx::query("UPDATE zapline_jobs SET run_at = 0 WHERE name = 'flaky'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 1);

    let body = client.get("/metrics").send().await.text();
    for line in [
        "# TYPE jobs_enqueued_total counter",
        r#"jobs_enqueued_total{job="add"} 2"#,
        r#"jobs_enqueued_total{job="flaky"} 1"#,
        r#"jobs_enqueued_total{job="panics"} 1"#,
        r#"jobs_started_total{job="add"} 2"#,
        r#"jobs_started_total{job="flaky"} 2"#,
        r#"jobs_succeeded_total{job="add"} 2"#,
        r#"jobs_failed_total{job="flaky"} 2"#,
        r#"jobs_failed_total{job="panics"} 1"#,
        r#"jobs_retried_total{job="flaky"} 1"#,
        r#"jobs_dead_total{job="flaky"} 1"#,
        r#"jobs_dead_total{job="panics"} 1"#,
        "# TYPE jobs_duration_seconds histogram",
        r#"jobs_duration_seconds_count{job="add"} 2"#,
        r#"jobs_duration_seconds_count{job="flaky"} 2"#,
        r#"jobs_duration_seconds_bucket{job="add",le="+Inf"} 2"#,
    ] {
        assert!(body.contains(line), "missing `{line}` in:\n{body}");
    }
    assert!(!body.contains(r#"jobs_succeeded_total{job="flaky"}"#));
}

#[tokio::test]
async fn no_job_metrics_without_the_plugin() {
    let runs = Runs::default();
    let client = app(&runs, 0).test();
    let jobs = jobs_of(&client).await;
    jobs.enqueue(Add { n: 1 }).await.unwrap();
    jobs.run_pending().await.unwrap();
    // Every other test here renders into its own registry.
    assert!(!zapline::metrics::registry().render().contains("jobs_"));
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn the_dev_console_shows_job_metrics_and_cron_jobs() {
    use zapline::metrics::{Metrics, Registry};

    let runs = Runs::default();
    let client = App::new()
        .state(runs.clone())
        .database(Database::sqlite_memory())
        .jobs_in::<Sqlite>(|q| {
            q.register::<Flaky>()
                .cron("0 3 * * *", Add { n: 1 })
                .workers(0)
        })
        .plugin(Metrics::new().registry(Registry::new()))
        .dev_console()
        .test();
    let jobs = jobs_of(&client).await;
    jobs.enqueue(Add { n: 1 }).await.unwrap();
    jobs.run_pending().await.unwrap();

    client
        .get("/_dev")
        .send()
        .await
        .assert_status(200)
        .assert_contains("<th>Enqueued<th>Succeeded<th>Failed<th>Retried<th>Dead<th>Avg time")
        .assert_contains("<tr><td><code>add</code><td>1<td>1<td>0<td>0<td>0<td>")
        .assert_contains("<tr><td><code>flaky</code><td>0<td>0<td>0<td>0<td>0<td>-</tr>")
        .assert_contains("<th>Cron job<th>Schedule<th>Next enqueue")
        .assert_contains("<td><code>add</code><td><code>0 3 * * *</code><td>");
}

// ---- cron jobs --------------------------------------------------------------------

#[test]
#[should_panic(expected = "cron job `add`: invalid cron expression `0 0 31 2 *`")]
fn cron_jobs_reject_bad_expressions() {
    let _ = App::new().jobs_in::<Sqlite>(|q| q.cron("0 0 31 2 *", Add { n: 1 }));
}

#[test]
#[should_panic(expected = "cron job `add` is scheduled twice on `@daily`")]
fn cron_jobs_reject_duplicates() {
    let _ = App::new()
        .jobs_in::<Sqlite>(|q| q.cron("@daily", Add { n: 1 }).cron("@daily", Add { n: 2 }));
}

/// Waits until `runs` reaches `n` (up to 5 s) and returns (runs, seconds).
async fn wait_for_runs(runs: &Runs, n: usize, started: std::time::Instant) -> (usize, f64) {
    for _ in 0..500 {
        if runs.get() >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (runs.get(), started.elapsed().as_secs_f64())
}

/// Two instances on one database: every tick is enqueued (and run) once.
#[tokio::test]
async fn cron_jobs_run_once_per_tick_across_instances() {
    use zapline::metrics::{Metrics, Registry};

    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("jobs.db").display());
    let runs = Runs::default();
    let registry = Registry::new();
    let make = || {
        App::new()
            .state(runs.clone())
            .database(Database::sqlite(url.clone()))
            .jobs_in::<Sqlite>(|q| {
                q.cron("* * * * * *", Add { n: 1 })
                    .workers(1)
                    .poll_interval(Duration::from_millis(20))
            })
            .plugin(Metrics::new().registry(registry.clone()))
            .test()
    };
    let (a, b) = (make(), make());
    let started = std::time::Instant::now();
    a.start().await;
    b.start().await;
    let (ran, elapsed) = wait_for_runs(&runs, 3, started).await;
    assert!(ran >= 3, "ran {ran} times in {elapsed:.1}s");
    // One run per tick: never more runs than seconds elapsed (plus the tick
    // in progress); duplicates would double the count.
    assert!(
        (ran as f64) <= elapsed.ceil() + 1.0,
        "ran {ran} times in {elapsed:.1}s"
    );
    let pool: sqlx::Pool<Sqlite> = a.resources().await.require().unwrap();
    let (key, next_at): (String, i64) = sqlx::query_as("SELECT key, next_at FROM zapline_cron")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(key, "add * * * * * *");
    assert_eq!(next_at % 1000, 0, "ticks fall on whole seconds");
    a.shutdown().await;
    b.shutdown().await;
    // Both instances count into one registry here: one enqueue per run (the
    // last tick may be enqueued but not run yet).
    let ran = runs.get();
    let enqueued: usize = registry
        .render()
        .lines()
        .find_map(|l| l.strip_prefix(r#"jobs_enqueued_total{job="add"} "#))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        enqueued >= ran && enqueued <= ran + 1,
        "{enqueued} enqueued, {ran} ran"
    );
}

/// A web instance with `.workers(0)` never enqueues cron jobs.
#[tokio::test]
async fn cron_jobs_are_not_enqueued_without_workers() {
    let runs = Runs::default();
    let client = App::new()
        .state(runs.clone())
        .database(Database::sqlite_memory())
        .jobs_in::<Sqlite>(|q| q.cron("* * * * * *", Add { n: 1 }).workers(0))
        .test();
    let jobs = jobs_of(&client).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(jobs.stats().await.unwrap(), JobStats::default());
}

/// The same race on Postgres, with three instances.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_cron_jobs_run_once_per_tick() {
    use zapline::db::sqlx::Postgres;

    let Ok(url) = std::env::var("ZAPLINE_TEST_POSTGRES_URL") else {
        return;
    };

    #[derive(Serialize, Deserialize)]
    struct PgTick;

    impl Job for PgTick {
        const NAME: &'static str = "pg_tick";

        async fn run(self, res: Resources) -> Result<()> {
            res.require::<Runs>()?.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // A database of its own: these workers would otherwise also run (and
    // fail) the jobs other tests enqueue in the shared one.
    let setup = App::new()
        .database(Database::postgres(url.clone()).migrate(false))
        .test();
    let pool: sqlx::Pool<Postgres> = setup.resources().await.require().unwrap();
    // Fails harmlessly when it already exists.
    let _ = sqlx::query("CREATE DATABASE zapline_cron_test")
        .execute(&pool)
        .await;
    let (base, _) = url.rsplit_once('/').unwrap();
    let url = format!("{base}/zapline_cron_test");
    let own = App::new()
        .database(Database::postgres(url.clone()).migrate(false))
        .test();
    let pool: sqlx::Pool<Postgres> = own.resources().await.require().unwrap();
    for sql in [
        "DROP TABLE IF EXISTS zapline_cron",
        "DROP TABLE IF EXISTS zapline_jobs",
    ] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }

    let runs = Runs::default();
    let make = || {
        App::new()
            .state(runs.clone())
            .database(Database::postgres(url.clone()).migrate(false))
            .jobs_in::<Postgres>(|q| {
                q.cron("* * * * * *", PgTick)
                    .workers(2)
                    .poll_interval(Duration::from_millis(20))
            })
            .test()
    };
    let (a, b, c) = (make(), make(), make());
    let started = std::time::Instant::now();
    a.start().await;
    b.start().await;
    c.start().await;
    let (ran, elapsed) = wait_for_runs(&runs, 3, started).await;
    assert!(
        ran >= 3 && (ran as f64) <= elapsed.ceil() + 1.0,
        "ran {ran} times in {elapsed:.1}s"
    );
    for client in [a, b, c] {
        client.shutdown().await;
    }
}
