//! Durable jobs on SQLite (and on Postgres when `ZAPLINE_TEST_POSTGRES_URL`
//! is set).
#![cfg(all(feature = "sqlite", feature = "json"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zapline::db::sqlx::{self, Sqlite};
use zapline::db::Database;
use zapline::jobs::{Job, JobBackend, JobStats, Jobs};
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

// `JobBackend` is public so apps can name it in generic helpers.
#[allow(dead_code)]
fn takes_any_backend<DB: JobBackend>(_jobs: &Jobs<DB>) {}
