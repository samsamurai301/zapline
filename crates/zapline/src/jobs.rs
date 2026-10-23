//! Durable background jobs stored in the app's database: they survive
//! restarts, are retried with backoff, and are shared by every instance of
//! the app.
//!
//! A job is a serializable struct that implements [`Job`]. Register its type
//! with [`App::jobs`] (after [`App::database`]) and enqueue it from a handler
//! with the [`Jobs`] extractor:
//!
//! ```no_run
//! # #[cfg(feature = "sqlite")] {
//! use zapline::db::Database;
//! use zapline::jobs::{Job, Jobs};
//! use zapline::prelude::*;
//!
//! #[model]
//! struct SendWelcome {
//!     email: String,
//! }
//!
//! impl Job for SendWelcome {
//!     const NAME: &'static str = "send_welcome";
//!
//!     async fn run(self, _res: Resources) -> Result<()> {
//!         println!("welcome, {}!", self.email); // send the email here
//!         Ok(())
//!     }
//! }
//!
//! #[post("/signup")]
//! async fn signup(jobs: Jobs) -> Result<&'static str> {
//!     jobs.enqueue(SendWelcome { email: "ada@example.com".into() }).await?;
//!     Ok("check your inbox")
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .database(Database::from_env())
//!         .jobs(|queue| queue.register::<SendWelcome>())
//!         .mount(signup)
//!         .run("0.0.0.0:8080")
//! }
//! # }
//! ```
//!
//! # How it works
//!
//! Jobs live in the table `zapline_jobs`, created at startup. Workers (4 by
//! default, see [`JobQueue::workers`]) claim due jobs one at a time; on
//! Postgres with `FOR UPDATE SKIP LOCKED`, so any number of app instances
//! can share one queue. A job that returns `Err` or panics is retried with
//! exponential backoff (5 s, 20 s, 80 s, ... capped at one hour) until it has
//! run [`Job::MAX_ATTEMPTS`] times, then it is kept with status `failed`
//! and its last error; [`Jobs::retry_failed`] queues those again. A finished
//! job is deleted.
//!
//! Delivery is at least once: a job whose worker dies mid-run is picked up
//! again once its lease ([`JobQueue::timeout`] plus 30 s) expires, so make
//! jobs safe to repeat.
//!
//! # Cron jobs
//!
//! [`JobQueue::cron`] enqueues a job on a [cron expression](crate::cron).
//! Unlike [`App::schedule`], which runs in every instance, a cron job is
//! enqueued **once per tick for the whole cluster**: instances race to
//! advance the tick in the `zapline_cron` table, and only the winner
//! enqueues. The job then gets the usual retries.
//!
//! ```no_run
//! # #[cfg(feature = "sqlite")] {
//! # use zapline::db::Database;
//! # use zapline::jobs::Job;
//! # use zapline::prelude::*;
//! #[model]
//! struct NightlyReport {}
//!
//! impl Job for NightlyReport {
//!     const NAME: &'static str = "nightly_report";
//!     async fn run(self, _res: Resources) -> Result<()> { Ok(()) }
//! }
//!
//! let app = App::new()
//!     .database(Database::from_env())
//!     .jobs(|queue| queue.cron("CRON_TZ=+01:00 0 2 * * *", NightlyReport {}));
//! # }
//! ```
//!
//! # Metrics
//!
//! With the [`Metrics`](crate::metrics::Metrics) plugin installed, the queue
//! exports counters labelled by job name: `jobs_enqueued_total`,
//! `jobs_started_total`, `jobs_succeeded_total`, `jobs_failed_total` (every
//! failed run), `jobs_retried_total`, `jobs_dead_total` (out of attempts),
//! and the histogram `jobs_duration_seconds`. Without the plugin nothing is
//! recorded.
//!
//! # Tests
//!
//! With `.workers(0)` nothing runs in the background, and
//! [`Jobs::run_pending`] runs the due jobs on the spot:
//!
//! ```
//! # #[cfg(feature = "sqlite")] {
//! # use zapline::db::Database;
//! # use zapline::jobs::{Job, Jobs};
//! # use zapline::prelude::*;
//! # #[model] struct Ping {}
//! # impl Job for Ping {
//! #     const NAME: &'static str = "ping";
//! #     async fn run(self, _res: Resources) -> Result<()> { Ok(()) }
//! # }
//! # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
//! let client = App::new()
//!     .database(Database::sqlite_memory())
//!     .jobs(|queue| queue.register::<Ping>().workers(0))
//!     .test();
//! let jobs: Jobs = client.resources().await.require()?;
//! jobs.enqueue(Ping {}).await?;
//! assert_eq!(jobs.run_pending().await?, 1);
//! # Ok(()) }
//! # }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sqlx::Pool;
use tokio::sync::Notify;

use crate::cron::{Cron, IntoCron};
use crate::db::DefaultDb;
use crate::dev::{escape, DevPanel};
use crate::extract::FromRequestParts;
use crate::handler::BoxFuture;
use crate::lifecycle::WeakResources;
use crate::metrics::{Counter, Histogram, MetricsHandle, Registry};
use crate::tasks::{http_date, until, Shutdown, RUN_BUCKETS};
use crate::{App, Error, Request, Resources, Result};

/// A unit of background work that is stored in the database until it has run.
///
/// The struct is the job's payload (stored as JSON, so keep it small: ids
/// rather than whole records). `#[model]` derives what it needs.
///
/// ```
/// use zapline::jobs::Job;
/// use zapline::prelude::*;
///
/// #[model]
/// struct ResizeImage {
///     upload_id: i64,
/// }
///
/// impl Job for ResizeImage {
///     const NAME: &'static str = "resize_image";
///     const MAX_ATTEMPTS: u32 = 3;
///
///     async fn run(self, res: Resources) -> Result<()> {
///         // look up the upload with res.require::<...>()?, resize, store...
///         Ok(())
///     }
/// }
/// ```
pub trait Job: Serialize + DeserializeOwned + Send + 'static {
    /// The name stored with each job. It identifies the job type across
    /// deploys, so keep it stable when you rename or move the struct.
    const NAME: &'static str;

    /// How many times the job runs before it is marked `failed` (default 5).
    const MAX_ATTEMPTS: u32 = 5;

    /// Do the work. `res` holds the app's resources (pools, services); an
    /// `Err` or a panic schedules a retry.
    fn run(self, res: Resources) -> impl Future<Output = Result<()>> + Send;
}

// ---------------------------------------------------------------------------
// Configuration

type Runner = Arc<dyn Fn(&str, Resources) -> BoxFuture<Result<()>> + Send + Sync>;

/// The job types this app runs and how its workers behave. Configure it in
/// the closure given to [`App::jobs`].
pub struct JobQueue {
    runners: HashMap<&'static str, Runner>,
    crons: Vec<CronEntry>,
    workers: usize,
    poll_interval: Duration,
    timeout: Duration,
}

/// A job enqueued on a cron schedule.
struct CronEntry {
    /// Row key in `zapline_cron`: the job name and the expression.
    key: String,
    name: &'static str,
    cron: Cron,
    payload: String,
    max_attempts: i64,
}

impl CronEntry {
    /// The first tick strictly after `t_ms`, in ms (`i64::MAX` if none).
    fn next_ms(&self, t_ms: i64) -> i64 {
        let t = UNIX_EPOCH + Duration::from_millis(u64::try_from(t_ms).unwrap_or(0));
        self.cron
            .next_after(t)
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map_or(i64::MAX, ms)
    }
}

impl fmt::Debug for JobQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<_> = self.runners.keys().collect();
        names.sort();
        let crons: Vec<&str> = self.crons.iter().map(|c| c.key.as_str()).collect();
        f.debug_struct("JobQueue")
            .field("jobs", &names)
            .field("crons", &crons)
            .field("workers", &self.workers)
            .field("poll_interval", &self.poll_interval)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl JobQueue {
    fn new() -> Self {
        JobQueue {
            runners: HashMap::new(),
            crons: Vec::new(),
            workers: 4,
            poll_interval: Duration::from_secs(1),
            timeout: Duration::from_secs(300),
        }
    }

    /// Run jobs of type `J` in this app.
    ///
    /// # Panics
    ///
    /// If another registered job type has the same [`Job::NAME`].
    pub fn register<J: Job>(mut self) -> Self {
        let runner: Runner = Arc::new(|payload: &str, res: Resources| {
            let job = serde_json::from_str::<J>(payload);
            Box::pin(async move {
                let job = job.map_err(|e| {
                    Error::internal(format!("cannot decode job `{}`: {e}", J::NAME))
                })?;
                job.run(res).await
            })
        });
        let previous = self.runners.insert(J::NAME, runner);
        assert!(
            previous.is_none(),
            "job name `{}` is registered twice",
            J::NAME
        );
        self
    }

    /// Enqueue `job` every time the [cron expression](crate::cron) matches,
    /// once per tick across every instance that shares the database.
    /// Registers `J` if it isn't yet.
    ///
    /// Ticks are kept in the table `zapline_cron`. Only instances with
    /// workers (see [`workers`](Self::workers)) enqueue. If every instance
    /// was down at a tick, the job is enqueued once when the first one
    /// starts again, then the schedule resumes.
    ///
    /// ```
    /// # #[cfg(feature = "sqlite")] {
    /// # use zapline::db::Database;
    /// use zapline::jobs::Job;
    /// use zapline::prelude::*;
    ///
    /// #[model]
    /// struct PurgeSessions {
    ///     older_than_days: u32,
    /// }
    ///
    /// impl Job for PurgeSessions {
    ///     const NAME: &'static str = "purge_sessions";
    ///     async fn run(self, _res: Resources) -> Result<()> { Ok(()) }
    /// }
    ///
    /// let app = App::new()
    ///     .database(Database::sqlite_memory())
    ///     .jobs(|queue| {
    ///         queue
    ///             .cron("*/30 * * * *", PurgeSessions { older_than_days: 30 })
    ///             .cron("@weekly", PurgeSessions { older_than_days: 365 })
    ///     });
    /// # }
    /// ```
    ///
    /// # Panics
    ///
    /// If the expression is invalid, the job can't be encoded, or the same
    /// job type is already scheduled on the same expression.
    pub fn cron<J: Job>(mut self, expr: impl IntoCron, job: J) -> Self {
        let cron = expr
            .into_cron()
            .unwrap_or_else(|e| panic!("cron job `{}`: {e}", J::NAME));
        let payload = serde_json::to_string(&job)
            .unwrap_or_else(|e| panic!("cron job `{}`: cannot encode the job: {e}", J::NAME));
        let key = format!("{} {cron}", J::NAME);
        assert!(
            !self.crons.iter().any(|c| c.key == key),
            "cron job `{}` is scheduled twice on `{cron}`",
            J::NAME
        );
        if !self.runners.contains_key(J::NAME) {
            self = self.register::<J>();
        }
        self.crons.push(CronEntry {
            key,
            name: J::NAME,
            cron,
            payload,
            max_attempts: i64::from(J::MAX_ATTEMPTS.max(1)),
        });
        self
    }

    /// How many jobs run at once in this instance (default 4). `0` starts
    /// no workers: jobs are only stored, e.g. in tests that call
    /// [`Jobs::run_pending`], or in a web instance when a separate worker
    /// instance runs them.
    pub fn workers(mut self, n: usize) -> Self {
        self.workers = n;
        self
    }

    /// How often an idle worker checks for due jobs (default 1 s). Jobs
    /// enqueued by this instance wake a worker immediately.
    pub fn poll_interval(mut self, every: Duration) -> Self {
        self.poll_interval = every.max(Duration::from_millis(10));
        self
    }

    /// The longest one run may take before it counts as failed (default
    /// 5 minutes).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

// ---------------------------------------------------------------------------
// The handle

struct Shared {
    runners: HashMap<&'static str, Runner>,
    crons: Vec<CronEntry>,
    timeout: Duration,
    poll_interval: Duration,
    wake: Notify,
    /// Set at startup when the app has the metrics plugin.
    metrics: OnceLock<JobMetrics>,
}

/// The queue's Prometheus series, all labelled by job name.
struct JobMetrics {
    enqueued: Counter,
    started: Counter,
    succeeded: Counter,
    failed: Counter,
    retried: Counter,
    dead: Counter,
    duration: Histogram,
}

impl JobMetrics {
    fn new(registry: &Registry) -> Self {
        let counter = |name: &str, help: &str| registry.counter(name, help, &["job"]);
        JobMetrics {
            enqueued: counter("jobs_enqueued_total", "Jobs added to the queue."),
            started: counter("jobs_started_total", "Job runs started."),
            succeeded: counter("jobs_succeeded_total", "Job runs that succeeded."),
            failed: counter(
                "jobs_failed_total",
                "Job runs that failed (returned an error, panicked or timed out).",
            ),
            retried: counter("jobs_retried_total", "Failed jobs scheduled to run again."),
            dead: counter("jobs_dead_total", "Jobs that ran out of attempts."),
            duration: registry.histogram_with_buckets(
                "jobs_duration_seconds",
                "How long job runs took.",
                &["job"],
                RUN_BUCKETS,
            ),
        }
    }
}

impl Shared {
    /// Count one event if metrics are on.
    fn count(&self, which: fn(&JobMetrics) -> &Counter, job: &str) {
        if let Some(m) = self.metrics.get() {
            which(m).with_labels(&[job]).inc();
        }
    }
}

/// Enqueue jobs, as a handler argument or with
/// `res.require::<Jobs>()` anywhere [`Resources`] are available (including
/// inside a job). Available once [`App::jobs`] is registered.
pub struct Jobs<DB: JobBackend = DefaultDb> {
    pool: Pool<DB>,
    // Weak: `Jobs` itself lives in the resources.
    resources: WeakResources,
    shared: Arc<Shared>,
}

impl<DB: JobBackend> Clone for Jobs<DB> {
    fn clone(&self) -> Self {
        Jobs {
            pool: self.pool.clone(),
            resources: self.resources.clone(),
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<DB: JobBackend> fmt::Debug for Jobs<DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jobs").field("database", &DB::NAME).finish()
    }
}

/// How many jobs are in each state, from [`Jobs::stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JobStats {
    /// Waiting to run (including retries not yet due).
    pub queued: u64,
    /// Claimed by a worker.
    pub running: u64,
    /// Out of attempts.
    pub failed: u64,
}

/// A failed job for the dev console: id, name, attempts, last error.
#[doc(hidden)]
pub type FailedRow = (i64, String, i64, Option<String>);

/// A row as claimed by a worker.
#[doc(hidden)]
#[derive(Debug)]
pub struct Claimed {
    id: i64,
    name: String,
    payload: String,
    attempts: i64,
    max_attempts: i64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Delay before attempt `attempts + 1`: 5 s, 20 s, 80 s, ... capped at 1 h.
fn backoff(attempts: i64) -> Duration {
    let exp = u32::try_from(attempts.saturating_sub(1).clamp(0, 10)).unwrap_or(10);
    Duration::from_secs(5u64.saturating_mul(4u64.saturating_pow(exp)).min(3600))
}

impl<DB: JobBackend> Jobs<DB> {
    /// Store `job` to run as soon as a worker is free.
    ///
    /// # Errors
    ///
    /// If the job can't be encoded or the insert fails.
    pub async fn enqueue<J: Job>(&self, job: J) -> Result<()> {
        self.enqueue_in(job, Duration::ZERO).await
    }

    /// Store `job` to run once `delay` has passed.
    ///
    /// # Errors
    ///
    /// If the job can't be encoded or the insert fails.
    pub async fn enqueue_in<J: Job>(&self, job: J, delay: Duration) -> Result<()> {
        if !self.shared.runners.contains_key(J::NAME) {
            return Err(Error::internal(format!(
                "job `{}` is not registered: add `.register::<{}>()` in `.jobs(..)`",
                J::NAME,
                std::any::type_name::<J>()
            )));
        }
        let payload = serde_json::to_string(&job).map_err(|e| {
            Error::internal(format!("cannot encode job `{}`", J::NAME)).with_source(e)
        })?;
        let now = now_ms();
        DB::insert(
            &self.pool,
            J::NAME,
            &payload,
            i64::from(J::MAX_ATTEMPTS.max(1)),
            now.saturating_add(ms(delay)),
            now,
        )
        .await?;
        self.shared.count(|m| &m.enqueued, J::NAME);
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Run every job that is due now, one after another, and return how
    /// many ran (whether they succeeded or not). Meant for tests and
    /// scripts; workers do this continuously.
    ///
    /// # Errors
    ///
    /// If the database fails. A failing job is not an error here: it is
    /// rescheduled or marked failed as usual.
    pub async fn run_pending(&self) -> Result<usize> {
        let mut ran = 0;
        while self.run_one().await? {
            ran += 1;
        }
        Ok(ran)
    }

    /// Count the jobs in each state.
    ///
    /// # Errors
    ///
    /// If the query fails.
    pub async fn stats(&self) -> Result<JobStats> {
        let rows = DB::counts(&self.pool).await?;
        let mut stats = JobStats::default();
        for (status, n) in rows {
            let n = u64::try_from(n).unwrap_or(0);
            match status.as_str() {
                "queued" => stats.queued = n,
                "running" => stats.running = n,
                "failed" => stats.failed = n,
                _ => {}
            }
        }
        Ok(stats)
    }

    /// Queue every failed job again with a fresh set of attempts, and
    /// return how many.
    ///
    /// # Errors
    ///
    /// If the update fails.
    pub async fn retry_failed(&self) -> Result<u64> {
        let n = DB::retry_failed(&self.pool, now_ms()).await?;
        if n > 0 {
            self.shared.wake.notify_waiters();
        }
        Ok(n)
    }

    /// The most recent failed jobs: id, name, attempts and last error.
    async fn failed(&self, limit: i64) -> Result<Vec<FailedRow>> {
        DB::failed(&self.pool, limit).await
    }

    /// The dev console's view of the queue.
    async fn panel(&self) -> String {
        let (stats, failed) = match (self.stats().await, self.failed(20).await) {
            (Ok(stats), Ok(failed)) => (stats, failed),
            (Err(e), _) | (_, Err(e)) => {
                return format!("<p class=s5>{}</p>", escape(&error_text(&e)));
            }
        };
        let mut names: Vec<&str> = self.shared.runners.keys().copied().collect();
        names.sort_unstable();
        let mut html = format!(
            "<p>{} queued · {} running · {} failed<br><small>registered: {}</small></p>",
            stats.queued,
            stats.running,
            stats.failed,
            escape(&names.join(", "))
        );
        if let Some(m) = self.shared.metrics.get() {
            html.push_str(
                "<table><tr><th>Job<th>Enqueued<th>Succeeded<th>Failed<th>Retried<th>Dead<th>Avg time</tr>",
            );
            for &name in &names {
                let get = |c: &Counter| c.with_labels(&[name]).get();
                let duration = m.duration.with_labels(&[name]);
                let avg = match duration.count() {
                    0 => "-".to_owned(),
                    n => format!("{:.1?}", Duration::from_secs_f64(duration.sum() / n as f64)),
                };
                html.push_str(&format!(
                    "<tr><td><code>{}</code><td>{}<td>{}<td>{}<td>{}<td>{}<td>{avg}</tr>",
                    escape(name),
                    get(&m.enqueued),
                    get(&m.succeeded),
                    get(&m.failed),
                    get(&m.retried),
                    get(&m.dead),
                ));
            }
            html.push_str("</table>");
        }
        if !self.shared.crons.is_empty() {
            let ticks: HashMap<String, i64> = DB::cron_rows(&self.pool)
                .await
                .map(|rows| rows.into_iter().collect())
                .unwrap_or_default();
            let now = SystemTime::now();
            html.push_str("<table><tr><th>Cron job<th>Schedule<th>Next enqueue</tr>");
            for entry in &self.shared.crons {
                let next = ticks
                    .get(&entry.key)
                    .and_then(|&at| u64::try_from(at).ok())
                    .map(|at| UNIX_EPOCH + Duration::from_millis(at))
                    .or_else(|| entry.cron.next_after(now))
                    .map_or_else(
                        || "never".to_owned(),
                        |at| format!("{} (in {})", http_date(at), until(now, at)),
                    );
                html.push_str(&format!(
                    "<tr><td><code>{}</code><td><code>{}</code><td>{}</tr>",
                    escape(entry.name),
                    escape(&entry.cron.to_string()),
                    escape(&next)
                ));
            }
            html.push_str("</table>");
        }
        if !failed.is_empty() {
            html.push_str("<table><tr><th>Id<th>Job<th>Attempts<th>Last error</tr>");
            for (id, name, attempts, error) in failed {
                html.push_str(&format!(
                    "<tr><td>{id}<td><code>{}</code><td>{attempts}<td><code>{}</code></tr>",
                    escape(&name),
                    escape(error.as_deref().unwrap_or(""))
                ));
            }
            html.push_str("</table>");
        }
        html
    }

    /// Claim and run one due job. `Ok(false)` when none is due.
    async fn run_one(&self) -> Result<bool> {
        let now = now_ms();
        let lease = ms(self.shared.timeout).saturating_add(30_000);
        let Some(job) = DB::claim(&self.pool, now, now.saturating_add(lease)).await? else {
            for name in DB::expire(&self.pool, now).await? {
                self.shared.count(|m| &m.dead, &name);
            }
            return Ok(false);
        };
        self.shared.count(|m| &m.started, &job.name);
        let clock = Instant::now();
        let outcome = match self.shared.runners.get(job.name.as_str()) {
            None => Err(format!(
                "no job named `{}` is registered in this app",
                job.name
            )),
            Some(runner) => {
                let res = self.resources.upgrade().unwrap_or_default();
                let run = runner(&job.payload, res);
                match tokio::time::timeout(
                    self.shared.timeout,
                    AssertUnwindSafe(run).catch_unwind(),
                )
                .await
                {
                    Ok(Ok(Ok(()))) => Ok(()),
                    Ok(Ok(Err(e))) => Err(error_text(&e)),
                    Ok(Err(panic)) => Err(format!("panicked: {}", panic_text(&*panic))),
                    Err(_) => Err(format!("timed out after {:?}", self.shared.timeout)),
                }
            }
        };
        if let Some(m) = self.shared.metrics.get() {
            let labels = [job.name.as_str()];
            m.duration
                .with_labels(&labels)
                .observe(clock.elapsed().as_secs_f64());
            match &outcome {
                Ok(()) => m.succeeded.with_labels(&labels).inc(),
                Err(_) => {
                    m.failed.with_labels(&labels).inc();
                    if job.attempts >= job.max_attempts {
                        m.dead.with_labels(&labels).inc();
                    } else {
                        m.retried.with_labels(&labels).inc();
                    }
                }
            }
        }
        match outcome {
            Ok(()) => DB::complete(&self.pool, job.id).await?,
            Err(error) if job.attempts >= job.max_attempts => {
                log(format_args!(
                    "job `{}` #{} failed for good after {} attempts: {error}",
                    job.name, job.id, job.attempts
                ));
                DB::fail(&self.pool, job.id, &error).await?;
            }
            Err(error) => {
                let delay = backoff(job.attempts);
                log(format_args!(
                    "job `{}` #{} failed (attempt {} of {}), retrying in {delay:?}: {error}",
                    job.name, job.id, job.attempts, job.max_attempts
                ));
                DB::retry(
                    &self.pool,
                    job.id,
                    now_ms().saturating_add(ms(delay)),
                    &error,
                )
                .await?;
            }
        }
        Ok(true)
    }

    /// Enqueue the cron jobs whose tick has come, if this instance wins the
    /// tick, and return how long until the next one.
    async fn enqueue_due_crons(&self) -> Result<Duration> {
        let now = now_ms();
        let ticks: HashMap<String, i64> = DB::cron_rows(&self.pool).await?.into_iter().collect();
        let mut soonest = i64::MAX;
        for entry in &self.shared.crons {
            let due = match ticks.get(&entry.key) {
                Some(&due) => due,
                None => {
                    let due = entry.next_ms(now);
                    DB::cron_init(&self.pool, &entry.key, due).await?;
                    due
                }
            };
            if due > now {
                soonest = soonest.min(due);
                continue;
            }
            // Missed ticks (every instance was down) collapse into this one.
            let next = entry.next_ms(now.max(due));
            let won = DB::cron_fire(
                &self.pool,
                &entry.key,
                due,
                next,
                entry.name,
                &entry.payload,
                entry.max_attempts,
                now,
            )
            .await?;
            if won {
                self.shared.count(|m| &m.enqueued, entry.name);
                self.shared.wake.notify_one();
            }
            soonest = soonest.min(next);
        }
        let wait = soonest.saturating_sub(now_ms()).max(0);
        Ok(Duration::from_millis(u64::try_from(wait).unwrap_or(u64::MAX)))
    }

    /// The cron scheduler: enqueue cron jobs at their ticks until
    /// `shutdown`.
    async fn schedule_crons(self, shutdown: Shutdown) {
        // Re-read the ticks at least this often: other instances move them,
        // and the wall clock may jump.
        const RECHECK: Duration = Duration::from_secs(30);
        while !shutdown.is_shutdown() {
            let wait = match self.enqueue_due_crons().await {
                Ok(wait) => wait.min(RECHECK),
                Err(e) => {
                    log(format_args!("cron scheduler: {e}"));
                    self.shared.poll_interval.max(Duration::from_secs(1))
                }
            };
            tokio::select! {
                _ = shutdown.wait() => break,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// A worker: run jobs until `shutdown`, sleeping when idle.
    async fn work(self, shutdown: Shutdown) {
        let poll = self.shared.poll_interval;
        while !shutdown.is_shutdown() {
            let idle = match self.run_one().await {
                Ok(ran) => !ran,
                Err(e) => {
                    log(format_args!("job worker: {e}"));
                    true
                }
            };
            if idle {
                tokio::select! {
                    _ = shutdown.wait() => break,
                    _ = self.shared.wake.notified() => {}
                    _ = tokio::time::sleep(poll) => {}
                }
            }
        }
    }
}

fn error_text(e: &Error) -> String {
    let mut text = e.message().to_string();
    let mut source: Option<&dyn std::error::Error> = match e.source_error() {
        Some(s) => Some(s),
        None => None,
    };
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> &str {
    p.downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>")
}

#[cfg(feature = "tracing")]
fn log(msg: fmt::Arguments<'_>) {
    tracing::warn!("{msg}");
}

#[cfg(not(feature = "tracing"))]
fn log(msg: fmt::Arguments<'_>) {
    eprintln!("[zapline] warning: {msg}");
}

impl<DB: JobBackend> FromRequestParts for Jobs<DB> {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Jobs<DB>>().cloned().ok_or_else(|| {
            Error::internal(
                "no job queue registered: add `.jobs(|queue| queue.register::<MyJob>())` \
                 after `.database(..)`",
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Backends

mod sealed {
    pub trait Sealed {}
}

/// A database that can hold jobs: SQLite and Postgres.
#[allow(async_fn_in_trait)]
pub trait JobBackend: sqlx::Database + sealed::Sealed {
    #[doc(hidden)]
    fn create_table(pool: &Pool<Self>) -> impl Future<Output = Result<()>> + Send;
    #[doc(hidden)]
    fn insert(
        pool: &Pool<Self>,
        name: &str,
        payload: &str,
        max_attempts: i64,
        run_at: i64,
        now: i64,
    ) -> impl Future<Output = Result<()>> + Send;
    #[doc(hidden)]
    fn claim(
        pool: &Pool<Self>,
        now: i64,
        locked_until: i64,
    ) -> impl Future<Output = Result<Option<Claimed>>> + Send;
    #[doc(hidden)]
    fn complete(pool: &Pool<Self>, id: i64) -> impl Future<Output = Result<()>> + Send;
    #[doc(hidden)]
    fn retry(
        pool: &Pool<Self>,
        id: i64,
        run_at: i64,
        error: &str,
    ) -> impl Future<Output = Result<()>> + Send;
    #[doc(hidden)]
    fn fail(pool: &Pool<Self>, id: i64, error: &str) -> impl Future<Output = Result<()>> + Send;
    #[doc(hidden)]
    fn expire(pool: &Pool<Self>, now: i64) -> impl Future<Output = Result<Vec<String>>> + Send;
    #[doc(hidden)]
    fn counts(pool: &Pool<Self>) -> impl Future<Output = Result<Vec<(String, i64)>>> + Send;
    #[doc(hidden)]
    fn retry_failed(pool: &Pool<Self>, now: i64) -> impl Future<Output = Result<u64>> + Send;
    #[doc(hidden)]
    fn failed(pool: &Pool<Self>, limit: i64)
        -> impl Future<Output = Result<Vec<FailedRow>>> + Send;
    #[doc(hidden)]
    fn create_cron_table(pool: &Pool<Self>) -> impl Future<Output = Result<()>> + Send;
    #[doc(hidden)]
    fn cron_rows(pool: &Pool<Self>) -> impl Future<Output = Result<Vec<(String, i64)>>> + Send;
    #[doc(hidden)]
    fn cron_init(pool: &Pool<Self>, key: &str, next_at: i64)
        -> impl Future<Output = Result<()>> + Send;
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    fn cron_fire(
        pool: &Pool<Self>,
        key: &str,
        due: i64,
        next_at: i64,
        name: &str,
        payload: &str,
        max_attempts: i64,
        now: i64,
    ) -> impl Future<Output = Result<bool>> + Send;
}

/// The columns every backend shares (the id column differs).
macro_rules! table {
    ($id:literal) => {
        concat!(
            "CREATE TABLE IF NOT EXISTS zapline_jobs (",
            $id,
            ", name TEXT NOT NULL, payload TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued', \
             attempts BIGINT NOT NULL DEFAULT 0, max_attempts BIGINT NOT NULL, run_at BIGINT NOT NULL, \
             locked_until BIGINT NOT NULL DEFAULT 0, last_error TEXT, created_at BIGINT NOT NULL)"
        )
    };
}

const INDEX: &str = "CREATE INDEX IF NOT EXISTS zapline_jobs_due ON zapline_jobs (status, run_at)";

/// One row per cron job: the next tick to enqueue, in ms since the epoch.
const CRON_TABLE: &str = "CREATE TABLE IF NOT EXISTS zapline_cron \
     (key TEXT PRIMARY KEY, next_at BIGINT NOT NULL)";

const INSERT: &str = "INSERT INTO zapline_jobs (name, payload, max_attempts, run_at, created_at) \
     VALUES ($1, $2, $3, $4, $5)";

macro_rules! backend_for {
    ($db:ty, $table:expr, $lock:literal) => {
        impl sealed::Sealed for $db {}

        impl JobBackend for $db {
            async fn create_table(pool: &Pool<Self>) -> Result<()> {
                sqlx::query($table).execute(pool).await?;
                sqlx::query(INDEX).execute(pool).await?;
                Ok(())
            }

            async fn insert(
                pool: &Pool<Self>,
                name: &str,
                payload: &str,
                max_attempts: i64,
                run_at: i64,
                now: i64,
            ) -> Result<()> {
                sqlx::query(INSERT)
                .bind(name)
                .bind(payload)
                .bind(max_attempts)
                .bind(run_at)
                .bind(now)
                .execute(pool)
                .await?;
                Ok(())
            }

            async fn claim(pool: &Pool<Self>, now: i64, locked_until: i64) -> Result<Option<Claimed>> {
                let row: Option<(i64, String, String, i64, i64)> = sqlx::query_as(concat!(
                    "UPDATE zapline_jobs SET status = 'running', attempts = attempts + 1, locked_until = $2 \
                     WHERE id = (SELECT id FROM zapline_jobs \
                     WHERE (status = 'queued' AND run_at <= $1) \
                     OR (status = 'running' AND locked_until <= $1 AND attempts < max_attempts) \
                     ORDER BY run_at, id LIMIT 1",
                    $lock,
                    ") RETURNING id, name, payload, attempts, max_attempts"
                ))
                .bind(now)
                .bind(locked_until)
                .fetch_optional(pool)
                .await?;
                Ok(row.map(|(id, name, payload, attempts, max_attempts)| Claimed {
                    id,
                    name,
                    payload,
                    attempts,
                    max_attempts,
                }))
            }

            async fn complete(pool: &Pool<Self>, id: i64) -> Result<()> {
                sqlx::query("DELETE FROM zapline_jobs WHERE id = $1")
                    .bind(id)
                    .execute(pool)
                    .await?;
                Ok(())
            }

            async fn retry(pool: &Pool<Self>, id: i64, run_at: i64, error: &str) -> Result<()> {
                sqlx::query(
                    "UPDATE zapline_jobs SET status = 'queued', run_at = $2, locked_until = 0, \
                     last_error = $3 WHERE id = $1",
                )
                .bind(id)
                .bind(run_at)
                .bind(error)
                .execute(pool)
                .await?;
                Ok(())
            }

            async fn fail(pool: &Pool<Self>, id: i64, error: &str) -> Result<()> {
                sqlx::query(
                    "UPDATE zapline_jobs SET status = 'failed', locked_until = 0, last_error = $2 \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(error)
                .execute(pool)
                .await?;
                Ok(())
            }

            async fn expire(pool: &Pool<Self>, now: i64) -> Result<Vec<String>> {
                Ok(sqlx::query_scalar(
                    "UPDATE zapline_jobs SET status = 'failed', \
                     last_error = 'the worker stopped before the job finished' \
                     WHERE status = 'running' AND locked_until <= $1 AND attempts >= max_attempts \
                     RETURNING name",
                )
                .bind(now)
                .fetch_all(pool)
                .await?)
            }

            async fn counts(pool: &Pool<Self>) -> Result<Vec<(String, i64)>> {
                Ok(sqlx::query_as("SELECT status, COUNT(*) FROM zapline_jobs GROUP BY status")
                    .fetch_all(pool)
                    .await?)
            }

            async fn retry_failed(pool: &Pool<Self>, now: i64) -> Result<u64> {
                let done = sqlx::query(
                    "UPDATE zapline_jobs SET status = 'queued', attempts = 0, run_at = $1, \
                     locked_until = 0 WHERE status = 'failed'",
                )
                .bind(now)
                .execute(pool)
                .await?;
                Ok(done.rows_affected())
            }

            async fn failed(
                pool: &Pool<Self>,
                limit: i64,
            ) -> Result<Vec<FailedRow>> {
                Ok(sqlx::query_as(
                    "SELECT id, name, attempts, last_error FROM zapline_jobs \
                     WHERE status = 'failed' ORDER BY id DESC LIMIT $1",
                )
                .bind(limit)
                .fetch_all(pool)
                .await?)
            }

            async fn create_cron_table(pool: &Pool<Self>) -> Result<()> {
                sqlx::query(CRON_TABLE).execute(pool).await?;
                Ok(())
            }

            async fn cron_rows(pool: &Pool<Self>) -> Result<Vec<(String, i64)>> {
                Ok(sqlx::query_as("SELECT key, next_at FROM zapline_cron")
                    .fetch_all(pool)
                    .await?)
            }

            async fn cron_init(pool: &Pool<Self>, key: &str, next_at: i64) -> Result<()> {
                sqlx::query(
                    "INSERT INTO zapline_cron (key, next_at) VALUES ($1, $2) \
                     ON CONFLICT (key) DO NOTHING",
                )
                .bind(key)
                .bind(next_at)
                .execute(pool)
                .await?;
                Ok(())
            }

            async fn cron_fire(
                pool: &Pool<Self>,
                key: &str,
                due: i64,
                next_at: i64,
                name: &str,
                payload: &str,
                max_attempts: i64,
                now: i64,
            ) -> Result<bool> {
                let mut tx = pool.begin().await?;
                // Only one instance moves the tick from `due`: it enqueues.
                let moved = sqlx::query(
                    "UPDATE zapline_cron SET next_at = $1 WHERE key = $2 AND next_at = $3",
                )
                .bind(next_at)
                .bind(key)
                .bind(due)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if moved != 1 {
                    tx.rollback().await?;
                    return Ok(false);
                }
                sqlx::query(INSERT)
                    .bind(name)
                    .bind(payload)
                    .bind(max_attempts)
                    .bind(now)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(true)
            }
        }
    };
}

#[cfg(feature = "sqlite")]
backend_for!(
    sqlx::Sqlite,
    table!("id INTEGER PRIMARY KEY AUTOINCREMENT"),
    ""
);
#[cfg(feature = "postgres")]
backend_for!(
    sqlx::Postgres,
    table!("id BIGSERIAL PRIMARY KEY"),
    " FOR UPDATE SKIP LOCKED"
);

// ---------------------------------------------------------------------------
// App integration

impl App {
    /// Run durable [`Job`]s from the database registered with
    /// [`App::database`] (call that first). `configure` registers the job
    /// types and tunes the workers; handlers enqueue with [`Jobs`].
    ///
    /// At startup the `zapline_jobs` table is created if missing and the
    /// workers start; at shutdown they finish their current job (within the
    /// [grace period](App::tasks_grace_period)).
    ///
    /// ```no_run
    /// # #[cfg(feature = "sqlite")] {
    /// # use zapline::db::Database;
    /// # use zapline::jobs::Job;
    /// # use zapline::prelude::*;
    /// # #[model] struct SendWelcome {}
    /// # impl Job for SendWelcome {
    /// #     const NAME: &'static str = "send_welcome";
    /// #     async fn run(self, _res: Resources) -> Result<()> { Ok(()) }
    /// # }
    /// let app = App::new()
    ///     .database(Database::from_env())
    ///     .jobs(|queue| queue.register::<SendWelcome>().workers(8));
    /// # }
    /// ```
    pub fn jobs(self, configure: impl FnOnce(JobQueue) -> JobQueue) -> Self {
        self.jobs_in::<DefaultDb>(configure)
    }

    /// [`jobs`](Self::jobs) on a database other than [`DefaultDb`] (for
    /// apps with both backends enabled).
    pub fn jobs_in<DB: JobBackend>(self, configure: impl FnOnce(JobQueue) -> JobQueue) -> Self {
        let queue = configure(JobQueue::new());
        let workers = queue.workers;
        let has_crons = !queue.crons.is_empty();
        let shared = Arc::new(Shared {
            runners: queue.runners,
            crons: queue.crons,
            timeout: queue.timeout,
            poll_interval: queue.poll_interval,
            wake: Notify::new(),
            metrics: OnceLock::new(),
        });
        let mut app = self.on_startup(move |res: Resources| async move {
            let pool: Pool<DB> = res.get().ok_or_else(|| {
                Error::internal("jobs need a database: call `.database(..)` before `.jobs(..)`")
            })?;
            DB::create_table(&pool).await?;
            if !shared.crons.is_empty() {
                DB::create_cron_table(&pool).await?;
            }
            if let Some(metrics) = res.get::<MetricsHandle>() {
                let _ = shared.metrics.set(JobMetrics::new(metrics.registry()));
            }
            res.insert(Jobs::<DB> {
                pool,
                resources: res.downgrade(),
                shared,
            });
            Ok(())
        });
        let panel = DevPanel::new(
            format!("jobs-{}", DB::NAME.to_ascii_lowercase()),
            format!("Jobs ({})", DB::NAME),
            |req: &Request| {
                let jobs = req.state::<Jobs<DB>>().cloned();
                Box::pin(async move {
                    match jobs {
                        Some(jobs) => jobs.panel().await,
                        None => "<p class=dim>Not started.</p>".to_owned(),
                    }
                }) as BoxFuture<String>
            },
        )
        .action("Retry failed jobs", |req: &Request| {
            let jobs = req.state::<Jobs<DB>>().cloned();
            Box::pin(async move {
                if let Some(jobs) = jobs {
                    jobs.retry_failed().await?;
                }
                Ok(())
            })
        });
        app = app.dev_panel(panel);
        for _ in 0..workers {
            app = app.background(|res: Resources, shutdown: Shutdown| async move {
                match res.get::<Jobs<DB>>() {
                    Some(jobs) => jobs.work(shutdown).await,
                    None => log(format_args!("job worker started without a job queue")),
                }
            });
        }
        if has_crons && workers > 0 {
            app = app.background(|res: Resources, shutdown: Shutdown| async move {
                if let Some(jobs) = res.get::<Jobs<DB>>() {
                    jobs.schedule_crons(shutdown).await;
                }
            });
        }
        app
    }
}
