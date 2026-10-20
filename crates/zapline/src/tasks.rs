//! Background tasks, long-running workers and scheduled jobs.
//!
//! - [`BackgroundTasks`]: an extractor that queues work to run after the
//!   handler returns (send an email, write an audit log...).
//! - [`App::schedule`]: run a job periodically ([`Schedule::every`]) or daily
//!   at a fixed UTC time ([`Schedule::daily_at`]).
//! - [`App::background`]: a long-running worker (queue consumer, cache
//!   refresher) that stops when the app shuts down.
//!
//! Jobs and workers start from a startup hook (so they can use providers
//! registered before them) and are stopped by a shutdown hook, which signals
//! [`Shutdown`] and waits up to a grace period
//! ([`App::tasks_grace_period`], default 10 s) before aborting them. Errors and
//! panics are logged and never take down the server.
//!
//! ```no_run
//! use std::time::Duration;
//! use zapline::prelude::*;
//! use zapline::tasks::{BackgroundTasks, Schedule, Shutdown};
//!
//! async fn signup(tasks: BackgroundTasks) -> &'static str {
//!     tasks.spawn(async { /* send the welcome email */ });
//!     "welcome!"
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .post("/signup", signup)
//!         .schedule(Schedule::every(Duration::from_secs(60)).name("purge-sessions"), |_res: Resources| async move {
//!             // delete expired sessions; returning Err(..) is logged
//!             Ok::<_, Error>(())
//!         })
//!         .background(|_res: Resources, shutdown: Shutdown| async move {
//!             while !shutdown.is_shutdown() {
//!                 tokio::select! {
//!                     _ = shutdown.wait() => break,
//!                     _ = tokio::time::sleep(Duration::from_secs(1)) => { /* poll a queue */ }
//!                 }
//!             }
//!         })
//!         .run("0.0.0.0:8080")
//! }
//! ```

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::extract::FromRequestParts;
use crate::handler::BoxFuture;
use crate::{App, Request, Resources, Result};

/// Default time [`App`] waits for jobs, workers and queued background tasks
/// to finish on shutdown before aborting them.
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Job outputs

/// What a job, worker or background task may return: `()` or
/// `Result<(), E>` where `E: Display` (errors are logged).
pub trait JobOutput: Send + 'static {
    /// `Err(message)` if the run failed.
    fn into_job_result(self) -> std::result::Result<(), String>;
}

impl JobOutput for () {
    fn into_job_result(self) -> std::result::Result<(), String> {
        Ok(())
    }
}

impl<E: fmt::Display + Send + 'static> JobOutput for std::result::Result<(), E> {
    fn into_job_result(self) -> std::result::Result<(), String> {
        self.map_err(|e| e.to_string())
    }
}

type JobResult = std::result::Result<(), String>;

/// Await `fut`, turning errors and panics into log lines.
async fn run_logged(what: &str, fut: impl Future<Output = JobResult>) {
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log_error(format_args!("{what} failed: {e}")),
        Err(panic) => log_error(format_args!("{what} panicked: {}", panic_message(&*panic))),
    }
}

fn panic_message(p: &(dyn Any + Send)) -> &str {
    p.downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>")
}

#[cfg(feature = "tracing")]
fn log_error(msg: fmt::Arguments<'_>) {
    tracing::error!("{msg}");
}

#[cfg(not(feature = "tracing"))]
fn log_error(msg: fmt::Arguments<'_>) {
    eprintln!("[zapline] error: {msg}");
}

#[cfg(feature = "tracing")]
fn log_warn(msg: fmt::Arguments<'_>) {
    tracing::warn!("{msg}");
}

#[cfg(not(feature = "tracing"))]
fn log_warn(msg: fmt::Arguments<'_>) {
    eprintln!("[zapline] warning: {msg}");
}

// ---------------------------------------------------------------------------
// Shutdown

/// A cloneable shutdown signal handed to workers (and available as
/// `State<Shutdown>` / `res.get::<Shutdown>()` once the app has jobs or
/// workers).
///
/// ```
/// use zapline::tasks::Shutdown;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let (trigger, shutdown) = Shutdown::channel();
/// assert!(!shutdown.is_shutdown());
/// trigger.trigger();
/// shutdown.wait().await; // returns immediately once triggered
/// assert!(shutdown.is_shutdown());
/// # }
/// ```
#[derive(Clone)]
pub struct Shutdown {
    rx: watch::Receiver<bool>,
}

/// The sending half of a [`Shutdown`] signal. Dropping every trigger also
/// counts as shutdown.
#[derive(Clone)]
pub struct ShutdownTrigger {
    tx: Arc<watch::Sender<bool>>,
}

impl Shutdown {
    /// A new, untriggered signal. Useful to drive workers in unit tests.
    pub fn channel() -> (ShutdownTrigger, Shutdown) {
        let (tx, rx) = watch::channel(false);
        (ShutdownTrigger { tx: Arc::new(tx) }, Shutdown { rx })
    }

    /// Whether shutdown has started.
    pub fn is_shutdown(&self) -> bool {
        *self.rx.borrow() || self.rx.has_changed().is_err()
    }

    /// Resolve once shutdown has started (immediately if it already has).
    /// Cancel-safe: use it in `tokio::select!`.
    pub async fn wait(&self) {
        let mut rx = self.rx.clone();
        // An error means every trigger is gone, which also means shutdown.
        let _ = rx.wait_for(|stop| *stop).await;
    }
}

impl ShutdownTrigger {
    /// Signal shutdown to every [`Shutdown`] clone. Idempotent.
    pub fn trigger(&self) {
        self.tx.send_replace(true);
    }

    /// Another receiver for this signal.
    pub fn subscribe(&self) -> Shutdown {
        Shutdown {
            rx: self.tx.subscribe(),
        }
    }
}

impl fmt::Debug for Shutdown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shutdown")
            .field("is_shutdown", &self.is_shutdown())
            .finish()
    }
}

impl fmt::Debug for ShutdownTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShutdownTrigger")
            .field("triggered", &*self.tx.borrow())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Supervisor: shared by every job/worker of one app

pub(crate) struct Supervisor {
    trigger: ShutdownTrigger,
    shutdown: Shutdown,
    grace_ms: AtomicU64,
    deadline: Mutex<Option<Instant>>,
    inflight: AtomicUsize,
    idle: Notify,
    next_id: AtomicUsize,
}

/// App-state handle so `BackgroundTasks` can find the supervisor.
#[derive(Clone)]
struct SupervisorHandle(Arc<Supervisor>);

struct InflightGuard(Arc<Supervisor>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.0.inflight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl Supervisor {
    fn new() -> Self {
        let (trigger, shutdown) = Shutdown::channel();
        Supervisor {
            trigger,
            shutdown,
            grace_ms: AtomicU64::new(DEFAULT_GRACE_PERIOD.as_millis() as u64),
            deadline: Mutex::new(None),
            inflight: AtomicUsize::new(0),
            idle: Notify::new(),
            next_id: AtomicUsize::new(1),
        }
    }

    /// The supervisor of `app`, created (and registered as state) on first use.
    fn of(app: App) -> (App, Arc<Supervisor>) {
        if let Some(SupervisorHandle(sup)) = app.extensions().get::<SupervisorHandle>() {
            let sup = sup.clone();
            return (app, sup);
        }
        let sup = Arc::new(Supervisor::new());
        let drain_sup = sup.clone();
        let mut app = app
            .state(SupervisorHandle(sup.clone()))
            .state(sup.shutdown.clone())
            .on_shutdown(move |_res: Resources| async move {
                drain_sup.drain_logged(drain_sup.begin_shutdown()).await;
            });
        app.extensions_mut().insert(SupervisorHandle(sup.clone()));
        (app, sup)
    }

    fn track(self: &Arc<Self>) -> InflightGuard {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        InflightGuard(self.clone())
    }

    /// Signal shutdown (first call only) and return the shared deadline.
    fn begin_shutdown(&self) -> Instant {
        let mut deadline = self.deadline.lock().unwrap_or_else(|e| e.into_inner());
        *deadline.get_or_insert_with(|| {
            self.trigger.trigger();
            add(
                Instant::now(),
                Duration::from_millis(self.grace_ms.load(Ordering::Relaxed)),
            )
        })
    }

    /// Wait for tracked tasks (background tasks, overlapping job runs) to
    /// finish. Returns `false` if the deadline passed first.
    async fn drain(&self, deadline: Instant) -> bool {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inflight.load(Ordering::Acquire) == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.inflight.load(Ordering::Acquire) == 0;
            }
        }
    }

    async fn drain_logged(&self, deadline: Instant) {
        if !self.drain(deadline).await {
            log_warn(format_args!(
                "{} background task(s) still running after the grace period",
                self.inflight.load(Ordering::Acquire)
            ));
        }
    }
}

impl App {
    /// How long shutdown waits for jobs, workers and queued
    /// [`BackgroundTasks`] before aborting them (default 10 s).
    pub fn tasks_grace_period(self, grace: Duration) -> Self {
        let (app, sup) = Supervisor::of(self);
        sup.grace_ms.store(
            u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        app
    }

    /// Run a job on a [`Schedule`] for the lifetime of the app.
    ///
    /// The job starts from a startup hook registered at this point, so it can
    /// use everything provided before it. Runs never overlap unless
    /// [`Schedule::allow_overlap`] is set; a run that overruns skips the
    /// missed ticks. Errors and panics are logged and the schedule continues.
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use zapline::prelude::*;
    /// use zapline::tasks::Schedule;
    ///
    /// let app = App::new()
    ///     .schedule(Schedule::every(Duration::from_secs(300)), |res: Resources| async move {
    ///         // refresh a cache, purge expired rows...
    ///     })
    ///     .schedule(Schedule::daily_at(3, 30).name("nightly-report"), |res: Resources| async move {
    ///         Ok::<_, Error>(())
    ///     });
    /// ```
    pub fn schedule<F, Fut>(self, schedule: Schedule, job: F) -> Self
    where
        F: Fn(Resources) -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: JobOutput,
    {
        let job = Arc::new(job);
        let job: JobFn = Arc::new(move |res| {
            let job = job.clone();
            Box::pin(async move { job(res).await.into_job_result() })
        });
        self.supervise(move |res, sup, id| {
            let name = match &schedule.name {
                Some(n) => format!("job `{n}`"),
                None => format!("job #{id}"),
            };
            run_schedule(job, schedule, res, sup, name)
        })
    }

    /// Run a long-running worker for the lifetime of the app. It should
    /// return once `shutdown` fires; after the grace period it is aborted.
    /// A returned error or a panic is logged (the worker is not restarted).
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use zapline::prelude::*;
    /// use zapline::tasks::Shutdown;
    ///
    /// let app = App::new().background(|res: Resources, shutdown: Shutdown| async move {
    ///     loop {
    ///         tokio::select! {
    ///             _ = shutdown.wait() => break,
    ///             _ = tokio::time::sleep(Duration::from_millis(500)) => { /* consume a queue */ }
    ///         }
    ///     }
    /// });
    /// ```
    pub fn background<F, Fut>(self, worker: F) -> Self
    where
        F: FnOnce(Resources, Shutdown) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: JobOutput,
    {
        self.supervise(move |res, sup, id| async move {
            let shutdown = sup.shutdown.clone();
            let what = format!("background worker #{id}");
            run_logged(&what, async move {
                worker(res, shutdown).await.into_job_result()
            })
            .await;
        })
    }

    /// Spawn `start` from a startup hook and stop it from a shutdown hook.
    fn supervise<S, Fut>(self, start: S) -> Self
    where
        S: FnOnce(Resources, Arc<Supervisor>, usize) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (app, sup) = Supervisor::of(self);
        let id = sup.next_id.fetch_add(1, Ordering::Relaxed);
        let slot: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::default();
        let (start_slot, stop_slot, stop_sup) = (slot.clone(), slot, sup.clone());
        app.on_startup(move |res: Resources| async move {
            let handle = tokio::spawn(start(res, sup, id));
            *start_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
            Ok(())
        })
        .on_shutdown(move |_res: Resources| async move {
            let deadline = stop_sup.begin_shutdown();
            let handle = stop_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(mut handle) = handle {
                if tokio::time::timeout_at(deadline, &mut handle)
                    .await
                    .is_err()
                {
                    handle.abort();
                    log_warn(format_args!(
                        "task #{id} did not stop within the grace period and was aborted"
                    ));
                }
            }
            stop_sup.drain_logged(deadline).await;
        })
    }
}

// ---------------------------------------------------------------------------
// Schedules

/// When a scheduled job runs. Build with [`every`](Schedule::every) or
/// [`daily_at`](Schedule::daily_at), then refine.
///
/// ```
/// # use std::time::Duration;
/// use zapline::tasks::Schedule;
///
/// let s = Schedule::every(Duration::from_secs(30))
///     .delay_start(Duration::from_secs(5))
///     .name("refresh-cache");
/// ```
#[derive(Clone, Debug)]
pub struct Schedule {
    kind: Kind,
    delay: Duration,
    overlap: bool,
    limit: Option<u64>,
    name: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Every(Duration),
    /// Seconds after midnight UTC.
    Daily(u64),
}

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

impl Schedule {
    /// Run every `period`, starting immediately (fixed rate: a run that
    /// overruns skips the missed ticks instead of bunching up).
    ///
    /// # Panics
    /// If `period` is zero.
    pub fn every(period: Duration) -> Self {
        assert!(!period.is_zero(), "Schedule::every: period must be > 0");
        Schedule::new(Kind::Every(period))
    }

    /// Run once a day at `hour:minute` UTC.
    ///
    /// # Panics
    /// If `hour >= 24` or `minute >= 60`.
    pub fn daily_at(hour: u32, minute: u32) -> Self {
        assert!(
            hour < 24 && minute < 60,
            "Schedule::daily_at: invalid time {hour:02}:{minute:02}"
        );
        Schedule::new(Kind::Daily(u64::from(hour) * 3600 + u64::from(minute) * 60))
    }

    fn new(kind: Kind) -> Self {
        Schedule {
            kind,
            delay: Duration::ZERO,
            overlap: false,
            limit: None,
            name: None,
        }
    }

    /// Wait this long after startup before the first run (for `daily_at`,
    /// the first run is the next occurrence after the delay).
    pub fn delay_start(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Let a new run start while the previous one is still running.
    pub fn allow_overlap(mut self) -> Self {
        self.overlap = true;
        self
    }

    /// Stop after `runs` runs.
    pub fn limit(mut self, runs: u64) -> Self {
        self.limit = Some(runs);
        self
    }

    /// A name for log messages.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into().into());
        self
    }

    /// Time from `now` until the next run would be due, ignoring
    /// `delay_start` (for diagnostics and tests). For `every`, the period.
    pub fn delay_until_next(&self, now: SystemTime) -> Duration {
        match self.kind {
            Kind::Every(period) => period,
            Kind::Daily(at) => {
                let since_epoch = now.duration_since(UNIX_EPOCH).unwrap_or_default();
                let day_ms = DAY.as_millis();
                let now_ms = since_epoch.as_millis() % day_ms;
                let target_ms = u128::from(at) * 1000;
                let wait = (target_ms + day_ms - now_ms) % day_ms;
                let wait = if wait == 0 { day_ms } else { wait };
                Duration::from_millis(wait as u64)
            }
        }
    }

    fn first_run(&self, start: Instant) -> Instant {
        let start = add(start, self.delay);
        match self.kind {
            Kind::Every(_) => start,
            Kind::Daily(_) => {
                let now = SystemTime::now();
                let then = now.checked_add(self.delay).unwrap_or(now);
                add(start, self.delay_until_next(then))
            }
        }
    }

    /// The next tick after a run that was due at `due`.
    fn next_run(&self, due: Instant, now: Instant) -> Instant {
        match self.kind {
            Kind::Every(period) => {
                let next = add(due, period);
                if now < next {
                    return next;
                }
                // Overran: stay on the grid, skipping missed ticks.
                let behind = (now - due).as_nanos() % period.as_nanos();
                add(now, period - Duration::from_nanos(behind as u64))
            }
            Kind::Daily(_) => add(now, self.delay_until_next(SystemTime::now())),
        }
    }
}

/// `t + d`, capped at roughly 30 years ahead instead of overflowing.
fn add(t: Instant, d: Duration) -> Instant {
    const FAR: Duration = Duration::from_secs(30 * 365 * 24 * 60 * 60);
    t.checked_add(d.min(FAR)).unwrap_or(t)
}

type JobFn = Arc<dyn Fn(Resources) -> BoxFuture<JobResult> + Send + Sync>;

async fn run_schedule(
    job: JobFn,
    schedule: Schedule,
    res: Resources,
    sup: Arc<Supervisor>,
    name: String,
) {
    let shutdown = sup.shutdown.clone();
    let name: Arc<str> = name.into();
    let mut due = schedule.first_run(Instant::now());
    let mut runs = 0u64;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait() => break,
            _ = tokio::time::sleep_until(due) => {}
        }
        if schedule.overlap {
            let guard = sup.track();
            let (job, res, name) = (job.clone(), res.clone(), name.clone());
            tokio::spawn(async move {
                run_logged(&name, job(res)).await;
                drop(guard);
            });
        } else {
            run_logged(&name, job(res.clone())).await;
        }
        runs += 1;
        if schedule.limit.is_some_and(|max| runs >= max) {
            break;
        }
        due = schedule.next_run(due, Instant::now());
    }
}

// ---------------------------------------------------------------------------
// BackgroundTasks

/// Queue work to run after the handler returns, FastAPI-style.
///
/// Queued tasks run in order on the current runtime once the extractor (and
/// every clone of it) is dropped, which normally happens when the handler
/// finishes, whatever it returns; queue tasks after the fallible work or call
/// [`clear`](Self::clear) on failure. Errors and panics are logged. When the
/// app has jobs or workers (or sets [`App::tasks_grace_period`]), shutdown
/// waits for queued tasks too.
///
/// ```
/// use zapline::prelude::*;
/// use zapline::tasks::BackgroundTasks;
///
/// async fn send_welcome(email: String) -> Result<()> {
///     // talk to the mail server...
///     Ok(())
/// }
///
/// async fn signup(tasks: BackgroundTasks, body: String) -> StatusCode {
///     tasks.spawn(send_welcome(body));
///     StatusCode::CREATED
/// }
///
/// let app = App::new().post("/signup", signup);
/// ```
#[derive(Clone, Default)]
pub struct BackgroundTasks {
    queue: Arc<Queue>,
}

#[derive(Default)]
struct Queue {
    tasks: Mutex<Vec<BoxFuture<JobResult>>>,
    supervisor: Option<Arc<Supervisor>>,
}

impl BackgroundTasks {
    /// An empty queue, not tied to a request. Its tasks run when it is
    /// dropped inside a Tokio runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue `task`. It may return `()` or `Result<(), E: Display>`.
    pub fn spawn<F>(&self, task: F)
    where
        F: Future + Send + 'static,
        F::Output: JobOutput,
    {
        let task: BoxFuture<JobResult> = Box::pin(async move { task.await.into_job_result() });
        self.queue.lock().push(task);
    }

    /// Drop every queued task.
    pub fn clear(&self) {
        self.queue.lock().clear();
    }

    /// Number of queued tasks.
    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    /// Whether no task is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Queue {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<BoxFuture<JobResult>>> {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        let tasks = std::mem::take(self.tasks.get_mut().unwrap_or_else(|e| e.into_inner()));
        if tasks.is_empty() {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            log_warn(format_args!(
                "{} background task(s) dropped: no Tokio runtime to run them on",
                tasks.len()
            ));
            return;
        };
        let guard = self.supervisor.as_ref().map(|s| s.track());
        rt.spawn(async move {
            for task in tasks {
                run_logged("background task", task).await;
            }
            drop(guard);
        });
    }
}

impl fmt::Debug for BackgroundTasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackgroundTasks")
            .field("queued", &self.len())
            .finish()
    }
}

impl FromRequestParts for BackgroundTasks {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        let supervisor = req
            .state::<SupervisorHandle>()
            .map(|SupervisorHandle(s)| s.clone());
        Ok(BackgroundTasks {
            queue: Arc::new(Queue {
                tasks: Mutex::new(Vec::new()),
                supervisor,
            }),
        })
    }
}
