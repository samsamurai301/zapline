//! Cron schedules for in-process scheduled tasks: firing, metrics and the
//! dev console.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zapline::cron::{Cron, Tz};
use zapline::metrics::{Metrics, Registry};
use zapline::prelude::*;
use zapline::tasks::Schedule;

/// Poll `cond` every 10 ms for up to `secs` seconds.
async fn eventually(what: &str, secs: u64, cond: impl Fn() -> bool) {
    for _ in 0..secs * 100 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test]
async fn a_cron_task_fires_on_whole_seconds() {
    let fired = Arc::new(Mutex::new(Vec::<SystemTime>::new()));
    let f = fired.clone();
    let client = App::new()
        .schedule(
            Schedule::cron("* * * * * *").name("tick").limit(2),
            move |_res: Resources| {
                let fired = f.clone();
                async move {
                    fired.lock().unwrap().push(SystemTime::now());
                }
            },
        )
        .test();
    client.start().await;
    eventually("two runs", 5, || fired.lock().unwrap().len() == 2).await;
    let fired = fired.lock().unwrap().clone();
    // Each run starts just after a second boundary, one second apart.
    let secs: Vec<u64> = fired
        .iter()
        .map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs())
        .collect();
    assert_eq!(secs[1], secs[0] + 1, "{fired:?}");
    for t in &fired {
        let ms = t.duration_since(UNIX_EPOCH).unwrap().subsec_millis();
        assert!(ms < 500, "fired {ms} ms after the second");
    }
    client.shutdown().await;
}

#[tokio::test]
async fn cron_runs_are_counted_and_timed_when_metrics_are_on() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let client = App::new()
        .plugin(Metrics::new().registry(Registry::new()))
        .schedule(
            Schedule::cron("* * * * * *").name("rollup").limit(2),
            move |_res: Resources| {
                let n = r.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        Err(Error::internal("first run fails"))
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .test();
    client.start().await;
    eventually("two runs", 5, || runs.load(Ordering::SeqCst) == 2).await;
    // The second run's metrics are recorded right after it returns.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let body = client.get("/metrics").send().await.text();
    for line in [
        "# TYPE scheduled_task_runs_total counter",
        r#"scheduled_task_runs_total{task="rollup",outcome="ok"} 1"#,
        r#"scheduled_task_runs_total{task="rollup",outcome="error"} 1"#,
        r#"scheduled_task_runs_total{task="rollup",outcome="panic"} 0"#,
        "# TYPE scheduled_task_duration_seconds histogram",
        r#"scheduled_task_duration_seconds_count{task="rollup"} 2"#,
    ] {
        assert!(body.contains(line), "missing `{line}` in:\n{body}");
    }
    client.shutdown().await;
}

#[tokio::test]
async fn no_task_metrics_without_the_plugin() {
    // Every other test here renders into its own registry.
    let registry = zapline::metrics::registry();
    let client = App::new()
        .schedule(Schedule::cron("@yearly"), |_res: Resources| async {})
        .test();
    client.start().await;
    assert!(!registry.render().contains("scheduled_task"));
    client.shutdown().await;
}

#[tokio::test]
async fn the_dev_console_lists_schedules() {
    let client = App::new()
        .dev_console()
        .schedule(
            Schedule::cron("0 3 * * MON-FRI").name("report"),
            |_res: Resources| async {},
        )
        .schedule(
            Schedule::every(Duration::from_secs(90)),
            |_res: Resources| async {},
        )
        .test();
    client.start().await;
    // The schedules start on the runtime right after startup.
    let mut page = client.get("/_dev").send().await;
    for _ in 0..100 {
        if page.text().contains(" GMT (in ") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        page = client.get("/_dev").send().await;
    }
    page.assert_status(200)
        .assert_contains("<h2>Scheduled tasks</h2>")
        .assert_contains("<code>report</code>")
        .assert_contains("<code>cron `0 3 * * MON-FRI`</code>")
        .assert_contains("<code>every 90s</code>")
        .assert_contains(" GMT (in ");
    client.shutdown().await;
}

#[test]
fn cron_schedules_compute_their_next_run() {
    let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
    // 1970-01-01 was a Thursday; the next Monday 09:30 is Jan 5th.
    let s = Schedule::cron("30 9 * * MON");
    assert_eq!(s.next_after(at(0)), Some(at(4 * 86_400 + 9 * 3600 + 1800)));
    assert_eq!(
        s.delay_until_next(at(0)),
        Duration::from_secs(4 * 86_400 + 9 * 3600 + 1800)
    );
    // `daily_at` is a cron underneath.
    assert_eq!(
        Schedule::daily_at(3, 30).next_after(at(0)),
        Schedule::cron("30 3 * * *").next_after(at(0))
    );
    // A parsed `Cron`, with a time zone.
    let cron = Cron::parse("0 9 * * *")
        .unwrap()
        .with_timezone(Tz::parse("+02:00").unwrap());
    assert_eq!(Schedule::cron(&cron).next_after(at(0)), Some(at(7 * 3600)));
}

#[test]
#[should_panic(expected = "Schedule::cron: invalid cron expression `61 * * * *`: minute `61`")]
fn invalid_cron_expressions_panic_at_build_time() {
    let _ = Schedule::cron("61 * * * *");
}
