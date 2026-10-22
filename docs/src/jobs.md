# Durable jobs

Work that must not be lost (emails, webhooks, image processing, billing) goes in a **job**: a struct stored in your database until a worker has run it. Jobs survive restarts and deploys, are retried with backoff when they fail, and are shared by every instance of the app.

```rust
use zapline::db::Database;
use zapline::jobs::{Job, Jobs};
use zapline::prelude::*;

#[model]
struct SendWelcome {
    user_id: i64,
}

impl Job for SendWelcome {
    const NAME: &'static str = "send_welcome";

    async fn run(self, res: Resources) -> Result<()> {
        // load the user with res.require::<..>()?, send the email...
        Ok(())
    }
}

#[post("/signup")]
async fn signup(jobs: Jobs) -> Result<StatusCode> {
    // create the user...
    jobs.enqueue(SendWelcome { user_id: 1 }).await?;
    Ok(StatusCode::CREATED)
}

fn main() -> std::io::Result<()> {
    App::new()
        .database(Database::from_env())
        .jobs(|queue| queue.register::<SendWelcome>())
        .mount(signup)
        .run("0.0.0.0:8080")
}
```

Requirements: the `postgres` or `sqlite` feature, and `json` (on by default). `zapline g job <name>` writes the struct, its registration and a test for you.

## Writing a job

- The struct is the payload, stored as JSON: keep it small (ids, not whole records). `#[model]` derives what it needs.
- `NAME` identifies the job type in the database. Keep it stable when you rename or move the struct, or queued jobs will find no handler.
- `run` gets the app's `Resources`, so it can use everything a handler can: `res.require::<Pool<Postgres>>()?`, `res.require::<Mailer>()?`, your own services.
- Return `Err` (or panic) to have the job retried. `MAX_ATTEMPTS` (default 5) caps the runs.
- Jobs run **at least once**: if a worker dies mid-run, the job runs again after its lease expires. Make jobs safe to repeat (check before you charge, upsert instead of insert).

## Enqueueing

| Call | Runs |
|---|---|
| `jobs.enqueue(job).await?` | as soon as a worker is free |
| `jobs.enqueue_in(job, Duration::from_secs(3600)).await?` | after the delay |

`Jobs` is an extractor, and also available from `Resources` (`res.require::<Jobs>()?`), so a job can enqueue follow-up jobs. Enqueueing a type that isn't registered in `.jobs(..)` is an error that names the missing `.register::<T>()`.

## Workers, retries and failures

`App::jobs` starts the workers at startup and stops them at shutdown, after their current job (within [`App::tasks_grace_period`](tasks.md#shutdown-and-the-grace-period)).

| Setting | Default | |
|---|---|---|
| `.workers(n)` | 4 | Jobs run at once in this instance. `0` runs none: useful in tests, or for web instances when a separate instance runs the jobs. |
| `.poll_interval(d)` | 1 s | How often idle workers look for due jobs. Jobs enqueued by the same instance wake a worker at once. |
| `.timeout(d)` | 5 min | A run that takes longer fails (and is retried). |

A failed run is retried after 5 s, 20 s, 80 s, 5 min, 21 min (capped at 1 h) until `MAX_ATTEMPTS`. Then the job stays in the table with status `failed` and its last error. Finished jobs are deleted.

```rust
let stats = jobs.stats().await?;      // JobStats { queued, running, failed }
let n = jobs.retry_failed().await?;   // queue every failed job again
```

The table is `zapline_jobs` (created at startup), so plain SQL works for anything else: `SELECT name, last_error FROM zapline_jobs WHERE status = 'failed'`.

On Postgres, workers claim jobs with `FOR UPDATE SKIP LOCKED`, so any number of instances share one queue without running a job twice. SQLite serializes writes, which gives the same guarantee for a single server.

## Testing jobs

With `.workers(0)` nothing runs in the background, and `run_pending` runs the due jobs on the spot, so tests are deterministic:

```rust
let client = App::new()
    .database(Database::sqlite_memory())
    .jobs(|queue| queue.register::<SendWelcome>().workers(0))
    .test();
let jobs: Jobs = client.resources().await.require()?;

client.post("/signup").send().await.assert_status(201);
assert_eq!(jobs.run_pending().await?, 1);
```

## Jobs or background tasks?

| | [`BackgroundTasks`](tasks.md) | Jobs |
|---|---|---|
| Stored | in memory | in the database |
| Survives a crash or deploy | no | yes |
| Retries | no | with backoff |
| Shared across instances | no | yes |
| Needs | nothing | a database |

Use background tasks for best-effort work (an audit log line, a cache warm-up) and jobs for everything a user would notice missing.
