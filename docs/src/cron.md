# Cron schedules

Cron expressions say *when* something runs. zapline uses them in two places:

| | Runs | Use it for |
|---|---|---|
| `App::schedule(Schedule::cron(expr), job)` ([tasks](tasks.md#scheduled-jobs)) | in every instance of the app, in memory | refreshing a local cache, per-instance housekeeping |
| `.jobs(\|q\| q.cron(expr, job))` ([durable jobs](jobs.md#cron-jobs)) | **once per tick for the whole cluster**, stored and retried | reports, billing runs, emails, anything that must happen exactly once per tick |

```rust
use zapline::db::Database;
use zapline::jobs::Job;
use zapline::prelude::*;
use zapline::tasks::Schedule;

#[model]
struct InvoiceRun {}

impl Job for InvoiceRun {
    const NAME: &'static str = "invoice_run";
    async fn run(self, _res: Resources) -> Result<()> { Ok(()) }
}

fn app() -> App {
    App::new()
        .database(Database::from_env())
        // Each instance: every 5 minutes during office hours, UTC.
        .schedule(Schedule::cron("*/5 8-18 * * MON-FRI").name("warm-cache"), |_res: Resources| async move {
            // ...
        })
        // The cluster: 02:00 Paris time on the 1st, enqueued once.
        .jobs(|q| q.cron("CRON_TZ=Europe/Paris 0 2 1 * *", InvoiceRun {}))
}
```

`zapline::cron` is always compiled in. Only IANA time zone names (`Europe/Paris`) need the `tz` feature.

## Syntax

Five fields, or six with **seconds first**:

```text
┌───────────── second (0-59)          only in the 6-field form
│ ┌─────────── minute (0-59)
│ │ ┌───────── hour (0-23)
│ │ │ ┌─────── day of month (1-31)
│ │ │ │ ┌───── month (1-12 or JAN-DEC)
│ │ │ │ │ ┌─── day of week (0-7 or SUN-SAT; 0 and 7 are Sunday)
│ │ │ │ │ │
0 30 9 * * MON-FRI      09:30:00 on weekdays
  30 9 * * MON-FRI      the same, 5 fields
```

Each field is a comma-separated list of:

| Item | Meaning | Example |
|---|---|---|
| `*` | every value | `* * * * *` every minute |
| `n` | one value | `0 12 * * *` at noon |
| `a-b` | a range, which may wrap around | `0 22-2 * * *` 22:00 to 02:00; `FRI-MON` |
| `*/n` | every n-th value | `*/15 * * * *` every 15 minutes |
| `a-b/n` | every n-th value in a range | `0 9-17/2 * * *` 09:00, 11:00, ... 17:00 |
| `a/n` | every n-th value from `a` | `5/20 * * * *` minutes 5, 25, 45 |
| names | months and weekdays, any case | `0 0 1 JAN,jul *` |

Extensions in the day fields (from Quartz):

| Item | Field | Meaning |
|---|---|---|
| `?` | either day field | same as `*` |
| `L` | day of month | the last day of the month (28th to 31st) |
| `L-n` | day of month | n days before the last day: `L-2` |
| `nW` | day of month | the weekday nearest the n-th, inside the month: `15W` |
| `LW` | day of month | the last weekday of the month |
| `d#n` | day of week | the n-th such weekday of the month: `FRI#2`, `1#1` |
| `dL` | day of week | the last such weekday of the month: `5L`, `SUNL` |

When **both** day fields are restricted (neither starts with `*` or `?`), a day matches if **either** matches, as in classic cron: `0 0 1 * MON` runs on the 1st *and* every Monday. If one of them starts with `*` or `?`, both must match: `0 0 */2 * MON` runs on Mondays that fall on an odd date.

Macros:

| Macro | Same as |
|---|---|
| `@yearly`, `@annually` | `0 0 1 1 *` |
| `@monthly` | `0 0 1 * *` |
| `@weekly` | `0 0 * * 0` |
| `@daily`, `@midnight` | `0 0 * * *` |
| `@hourly` | `0 * * * *` |

Expressions are checked when the app is built: a typo panics with the reason (``invalid cron expression `0 25 * * *`: hour `25` is out of range 0-23``), and so does an expression that can never match, such as `0 0 30 2 *`. To handle errors yourself, for example for a schedule read from configuration, parse it first:

```rust
use zapline::cron::Cron;
use zapline::tasks::Schedule;

fn schedule_from_config(expr: &str) -> Result<Schedule, zapline::cron::CronError> {
    let cron: Cron = expr.parse()?;
    Ok(Schedule::cron(cron))
}
```

## Time zones

An expression runs in **UTC** unless it starts with `CRON_TZ=<zone>` (or `TZ=<zone>`):

| Zone | Example | Needs |
|---|---|---|
| UTC | `CRON_TZ=UTC`, `Z`, `GMT` | nothing |
| A fixed offset | `CRON_TZ=+05:30`, `-0800`, `UTC+2` | nothing |
| An IANA zone | `CRON_TZ=America/New_York` | the `tz` feature |

```toml
zapline = { version = "0.1", features = ["tz"] }
```

With `tz`, zone rules come from the system's time zone database (`/usr/share/zoneinfo` on Linux and macOS; bundled on Windows), through the [`jiff`](https://docs.rs/jiff) crate. Without it, an IANA name is an error that says to enable the feature. You can also set the zone in code with `Cron::with_timezone(Tz::parse("Europe/Paris")?)`.

Fields are matched against the **local** time: `CRON_TZ=+05:30 0 1 * * MON` runs at 01:00 on Mondays in that zone, which is Sunday 19:30 UTC.

### Daylight-saving time

Wall clocks skip an hour in spring and repeat one in autumn. zapline handles both without surprises:

- **Clocks go forward** (02:00 jumps to 03:00): a time that doesn't exist that day, such as `30 2 * * *`, runs once, at the jump (03:00 local). A schedule with several times in the missing hour also runs just once there.
- **Clocks go back** (02:00 returns to 01:00): a time that happens twice, such as `30 1 * * *`, runs once, the first time. A schedule whose hour field covers every hour, such as `*/15 * * * *`, keeps its rhythm through both passes, so it never pauses for an hour.

UTC and fixed offsets have no daylight-saving changes, which is why they are the default.

## Inspecting a schedule

`Cron` computes fire times without running anything, which is handy in tests and admin pages:

```rust
use std::time::{Duration, UNIX_EPOCH};
use zapline::cron::Cron;

let cron = Cron::parse("0 0 L * *")?;                // midnight on the last day of each month
let jan_1_2024 = UNIX_EPOCH + Duration::from_secs(1_704_067_200);
let next: Vec<_> = cron.upcoming(jan_1_2024).take(2).collect();
assert_eq!(next[0], UNIX_EPOCH + Duration::from_secs(1_706_659_200)); // 2024-01-31
assert_eq!(next[1], UNIX_EPOCH + Duration::from_secs(1_709_164_800)); // 2024-02-29
# Ok::<(), zapline::cron::CronError>(())
```

| Method | Returns |
|---|---|
| `Cron::parse(expr)` / `expr.parse::<Cron>()` | the parsed expression or a `CronError` |
| `cron.next_after(t)` | the first fire time strictly after `t` (whole seconds) |
| `cron.upcoming(t)` | every fire time after `t`, as an iterator |
| `cron.with_timezone(tz)` / `cron.timezone()` | change or read the zone |
| `cron.to_string()` | the expression, with `CRON_TZ=` when the zone isn't UTC |
| `Schedule::next_after(t)` | the same for a task schedule |

In debug builds the [dev console](dev-console.md) lists every scheduled task with its next run, and every cron job with its next enqueue.
