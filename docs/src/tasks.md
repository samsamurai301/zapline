# Background tasks & scheduled jobs

Queue work from a handler with `BackgroundTasks`, run periodic or [cron](cron.md) jobs with `App::schedule`, and run long-lived workers with `App::background`. All three are stopped cleanly on shutdown.

```rust
use zapline::prelude::*;
use zapline::tasks::BackgroundTasks;

async fn send_welcome_email(address: String) -> Result<()> {
    // talk to the mail server...
    Ok(())
}

#[post("/signup")]
async fn signup(tasks: BackgroundTasks, Json(address): Json<String>) -> StatusCode {
    tasks.spawn(send_welcome_email(address));   // runs after this handler returns
    StatusCode::CREATED
}
```

`zapline::tasks` is always compiled in; there is no Cargo feature to enable.

## `BackgroundTasks`

`BackgroundTasks` is an extractor holding a queue. `tasks.spawn(future)` adds to it, and the queued futures run **after the handler returns**:

- They run **in order**, one after another, on the current runtime. The response doesn't wait for them.
- They start when the extractor (and every clone of it) is dropped, which normally happens when the handler function finishes. This is **whatever the handler returns**, an error included. Queue tasks after the fallible work, or call `tasks.clear()` on failure.
- A task may return `()` or `Result<(), E>` where `E: Display`. Errors and panics are logged (through `tracing` when enabled, otherwise on stderr) and never affect the client or the server.
- A task must be `Send + 'static`. `Dep<T>` is an `Arc` underneath, so clone it into the task.
- If the app has a job, a worker, or sets `App::tasks_grace_period`, shutdown waits for queued tasks (see [Shutdown and the grace period](#shutdown-and-the-grace-period)). Without any of these, tasks still queued when the process exits can be cut off.
- Tasks live in memory. If the process crashes or is killed before they run, they are lost. For work that must not be lost, use [durable jobs](jobs.md), which are stored in the database and retried.

```rust
use zapline::di::Dep;
use zapline::prelude::*;
use zapline::tasks::BackgroundTasks;

struct Mailer;

impl Mailer {
    async fn send_receipt(&self, order_id: u64) -> Result<()> {
        println!("receipt for order {order_id}");
        Ok(())
    }
}

async fn save_order(items: Vec<String>) -> Result<u64> {
    Ok(items.len() as u64)
}

#[post("/orders")]
async fn create_order(
    tasks: BackgroundTasks,
    mailer: Dep<Mailer>,
    Json(items): Json<Vec<String>>,
) -> Result<(StatusCode, Json<u64>)> {
    let id = save_order(items).await?;              // fallible work first
    let mailer = mailer.clone();
    tasks.spawn(async move { mailer.send_receipt(id).await });
    Ok((StatusCode::CREATED, Json(id)))
}
```

| Method | Effect |
|---|---|
| `spawn(future)` | Queues a task |
| `clear()` | Drops every queued task |
| `len()` / `is_empty()` | Inspect the queue |
| `BackgroundTasks::new()` | A queue that isn't tied to a request. Its tasks run when it is dropped inside a Tokio runtime. |

## Scheduled jobs

`App::schedule(schedule, job)` runs a closure periodically for the lifetime of the app. The closure receives `Resources` (use `res.dep::<T>()` to reach your [providers](di.md)) and returns a future producing `()` or `Result<(), E: Display>`:

```rust
use std::time::Duration;
use zapline::prelude::*;
use zapline::tasks::Schedule;

fn app() -> App {
    App::new()
        .schedule(
            Schedule::every(Duration::from_secs(300)).name("refresh-cache"),
            |_res: Resources| async move {
                // refresh a cache; returning Err(..) is logged and the schedule goes on
                Ok::<_, Error>(())
            },
        )
        .schedule(
            Schedule::cron("0 9 * * MON-FRI").name("standup-digest"),   // 09:00 UTC on weekdays
            |_res: Resources| async move {
                println!("sending the digest");
            },
        )
        .schedule(
            Schedule::daily_at(3, 30).name("nightly-report"),   // 03:30 UTC
            |_res: Resources| async move {
                println!("building the nightly report");
            },
        )
}
```

| Schedule | Meaning |
|---|---|
| `Schedule::every(period)` | Every `period`, starting immediately at startup. It is fixed-rate: a run that overruns skips the missed ticks instead of bunching them up. Panics if `period` is zero. |
| `Schedule::cron(expr)` | Whenever the [cron expression](cron.md) matches: 5 or 6 fields, ranges, steps, names, `L`/`W`/`#`, `@hourly`... in UTC, or in the zone given by a `CRON_TZ=` prefix. It follows the wall clock; a run that overruns skips the matches it missed. Panics on an invalid expression (pass a `Cron` you parsed to handle the error). |
| `Schedule::daily_at(hour, minute)` | Once a day at that time in **UTC**: the same as `Schedule::cron("{minute} {hour} * * *")`. Panics on an invalid time. |

Refine a schedule with modifiers:

| Modifier | Effect |
|---|---|
| `.delay_start(duration)` | Waits this long after startup before the first run (for `cron` and `daily_at`, the first run is the next occurrence after the delay) |
| `.allow_overlap()` | Starts a new run even if the previous one is still running. By default runs never overlap. |
| `.limit(runs)` | Stops after `runs` runs |
| `.name(text)` | A name for log messages, metrics and the dev console. Without one, jobs are called `job #1`, `job #2`, ... |

Things to know:

- A job starts from a startup hook registered at that point in the builder chain, so it can use every provider registered before it.
- Errors and panics are logged, and the schedule continues.
- Jobs and workers run on the app's small "system" runtime (two threads, the one that runs startup hooks), not on the per-core request runtimes. A busy job can't stall a request's core, but it shares those two threads with every other job, so move blocking or CPU-heavy work to `tokio::task::spawn_blocking`.
- Each running instance of your app runs its own schedule. With several replicas, a job runs once per replica. For work that must run once per tick across the cluster, use a [cron job](jobs.md#cron-jobs) instead: `.jobs(|q| q.cron("0 3 * * *", NightlyReport {}))`.
- `schedule.next_after(time)` and `schedule.delay_until_next(time)` tell you when a schedule fires next, without running it.

With the [`Metrics`](observability.md#metrics) plugin installed, every run is counted and timed, labelled by the schedule's name (`task`). Without the plugin nothing is recorded.

| Series | Type | Labels |
|---|---|---|
| `scheduled_task_runs_total` | counter | `task`, `outcome` (`ok`, `error`, `panic`) |
| `scheduled_task_duration_seconds` | histogram (5 ms to 15 min) | `task` |

In debug builds the [dev console](dev-console.md) lists every schedule with its next run, run and failure counts, and how the last run went.

## Long-running workers

`App::background(worker)` runs an async closure once, for the lifetime of the app: a queue consumer, a cache refresher, a websocket client. It receives `Resources` and a `Shutdown` signal and **should return when the signal fires**:

```rust
use std::time::Duration;
use zapline::prelude::*;
use zapline::tasks::Shutdown;

fn app() -> App {
    App::new().background(|_res: Resources, shutdown: Shutdown| async move {
        loop {
            tokio::select! {
                _ = shutdown.wait() => break,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    // poll a queue, flush a buffer...
                }
            }
        }
    })
}
```

A worker that returns an error or panics is logged and **not restarted**. Wrap the loop yourself if you want retries.

### `Shutdown`

`Shutdown` is a cloneable signal:

| Method | Effect |
|---|---|
| `wait().await` | Resolves once shutdown has started (immediately if it already has). Cancel-safe, so it works inside `tokio::select!`. |
| `is_shutdown()` | Non-blocking check |
| `Shutdown::channel()` | Creates a `(ShutdownTrigger, Shutdown)` pair, to drive workers in unit tests. `trigger.trigger()` fires it. |

As soon as the app has a job, a worker, or a custom grace period, `Shutdown` is also registered as state, so handlers can react to it. For example, to end a long poll early instead of holding up the drain:

```rust
use std::time::Duration;
use zapline::prelude::*;
use zapline::tasks::Shutdown;

async fn poll(State(shutdown): State<Shutdown>) -> &'static str {
    tokio::select! {
        _ = shutdown.wait() => "shutting down, retry elsewhere",
        _ = tokio::time::sleep(Duration::from_secs(30)) => "nothing new",
    }
}

fn app() -> App {
    App::new().tasks_grace_period(Duration::from_secs(10)).get("/poll", poll)
}
```

Testing a worker needs no app at all:

```rust
use std::time::Duration;
use zapline::tasks::Shutdown;

async fn worker(shutdown: Shutdown) -> u32 {
    let mut ticks = 0;
    loop {
        tokio::select! {
            _ = shutdown.wait() => return ticks,
            _ = tokio::time::sleep(Duration::from_millis(5)) => ticks += 1,
        }
    }
}

#[tokio::test]
async fn worker_stops_when_signalled() {
    let (trigger, shutdown) = Shutdown::channel();
    let handle = tokio::spawn(worker(shutdown));
    tokio::time::sleep(Duration::from_millis(30)).await;
    trigger.trigger();
    assert!(handle.await.unwrap() > 0);
}
```

## Shutdown and the grace period

When the server stops, shutdown hooks run after the connections have drained (see [State, lifecycle & plugins](lifecycle.md)). The task supervisor then:

1. Fires `Shutdown`, which stops schedules and tells workers to return.
2. Waits for workers, queued `BackgroundTasks` and overlapping job runs to finish.
3. After the grace period, **aborts** whatever is still running and logs a warning.

The grace period defaults to 10 seconds (`zapline::tasks::DEFAULT_GRACE_PERIOD`). Change it with `App::tasks_grace_period(Duration)`.

The grace period comes on top of the time the server spends draining connections (see [Server: TLS, HTTP/2, graceful shutdown](server.md)). Size your orchestrator's termination timeout for both; see [Deployment](deployment.md).

## Tests

`App::test()` starts jobs and workers together with the other startup hooks, on the first request. Call `client.start().await` to start them eagerly and `client.shutdown().await` to stop them and wait for the drain. Scheduled jobs with long periods never fire inside a short test, so test the job's logic as a plain function and keep the schedule thin.
