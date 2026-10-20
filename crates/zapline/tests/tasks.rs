//! Background tasks, scheduled jobs and long-running workers.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{oneshot, Notify};
use zapline::prelude::*;
use zapline::tasks::{BackgroundTasks, Schedule, Shutdown};

/// Poll `cond` every few ms for up to 5 s.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..1000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Panics, typed as `()` so async blocks need no never-type fallback.
fn boom(msg: &str) {
    panic!("{msg}");
}

// ---- BackgroundTasks ----------------------------------------------------------

#[tokio::test]
async fn background_tasks_run_after_the_handler_returns() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(Notify::new());
    let (done_tx, done_rx) = oneshot::channel::<()>();
    let done_tx = Arc::new(Mutex::new(Some(done_tx)));

    let (l, g) = (log.clone(), gate.clone());
    let c = App::new()
        .get("/", move |tasks: BackgroundTasks| {
            let (log, gate, done) = (l.clone(), g.clone(), done_tx.clone());
            async move {
                let task_log = log.clone();
                tasks.spawn(async move {
                    gate.notified().await; // held until the test has the response
                    task_log.lock().unwrap().push("task");
                    if let Some(tx) = done.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                });
                assert_eq!(tasks.len(), 1);
                log.lock().unwrap().push("handler");
                "response"
            }
        })
        .test();

    let r = c.get("/").send().await;
    assert_eq!(r.text(), "response");
    assert_eq!(*log.lock().unwrap(), vec!["handler"], "task not run yet");
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(5), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*log.lock().unwrap(), vec!["handler", "task"]);
}

#[tokio::test]
async fn tasks_run_in_order_and_survive_errors_and_panics() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = log.clone();
    let c = App::new()
        .get("/", move |tasks: BackgroundTasks| {
            let log = l.clone();
            async move {
                let (a, b, c) = (log.clone(), log.clone(), log.clone());
                tasks.spawn(async move { a.lock().unwrap().push(1) });
                tasks.spawn(async { Err::<(), _>(Error::internal("smtp down")) });
                tasks.spawn(async move {
                    b.lock().unwrap().push(2);
                    boom("task bug");
                });
                tasks.spawn(async move { c.lock().unwrap().push(3) });
                StatusCode::ACCEPTED
            }
        })
        .test();

    assert_eq!(c.get("/").send().await.status(), 202);
    eventually("all tasks", || log.lock().unwrap().len() == 3).await;
    assert_eq!(*log.lock().unwrap(), vec![1, 2, 3]);
    // The app keeps serving after a task panicked.
    assert_eq!(c.get("/").send().await.status(), 202);
    eventually("second batch", || log.lock().unwrap().len() == 6).await;
}

#[tokio::test]
async fn cleared_tasks_do_not_run() {
    let ran = Arc::new(AtomicBool::new(false));
    let r = ran.clone();
    let c = App::new()
        .get("/", move |tasks: BackgroundTasks| {
            let ran = r.clone();
            async move {
                tasks.spawn(async move { ran.store(true, Ordering::SeqCst) });
                tasks.clear();
                assert!(tasks.is_empty());
                Err::<(), _>(Error::bad_request("rolled back"))
            }
        })
        .test();
    assert_eq!(c.get("/").send().await.status(), 400);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!ran.load(Ordering::SeqCst));
}

#[tokio::test]
async fn clones_share_one_queue_that_runs_when_the_last_drops() {
    let ran = Arc::new(AtomicUsize::new(0));
    let tasks = BackgroundTasks::new();
    let clone = tasks.clone();
    let r = ran.clone();
    clone.spawn(async move {
        r.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(tasks.len(), 1);
    drop(tasks);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(ran.load(Ordering::SeqCst), 0, "a clone is still alive");
    drop(clone);
    eventually("task after last drop", || ran.load(Ordering::SeqCst) == 1).await;
}

#[test]
fn dropping_tasks_outside_a_runtime_does_not_panic() {
    let tasks = BackgroundTasks::new();
    tasks.spawn(async {});
    drop(tasks); // logged and discarded
}

#[tokio::test]
async fn background_tasks_on_a_real_server() {
    let (tx, rx) = oneshot::channel::<String>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let app = App::new().post("/signup", move |tasks: BackgroundTasks, body: String| {
        let tx = tx.clone();
        async move {
            tasks.spawn(async move {
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(format!("welcome {body}"));
                }
            });
            StatusCode::CREATED
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(app.serve(listener));

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(
            b"POST /signup HTTP/1.1\r\nhost: x\r\ncontent-length: 3\r\nconnection: close\r\n\r\nada",
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");

    let msg = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg, "welcome ada");
    server.abort();
}

// ---- scheduled jobs -------------------------------------------------------------

#[tokio::test]
async fn scheduled_job_runs_repeatedly_with_resources() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let c = App::new()
        .state(7u32)
        .schedule(
            Schedule::every(Duration::from_millis(10)),
            move |res: Resources| {
                let runs = r.clone();
                async move {
                    assert_eq!(res.require::<u32>().unwrap(), 7);
                    runs.fetch_add(1, Ordering::SeqCst);
                }
            },
        )
        .test();
    c.start().await;
    eventually("3 runs", || runs.load(Ordering::SeqCst) >= 3).await;
    c.shutdown().await;
    let after = runs.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(runs.load(Ordering::SeqCst), after, "stopped on shutdown");
}

#[tokio::test]
async fn job_starts_after_providers_registered_before_it() {
    struct Db(&'static str);
    let seen = Arc::new(Mutex::new(None));
    let s = seen.clone();
    let c = App::new()
        .provide_with(|_res: Resources| async { Ok(Db("ready")) })
        .schedule(
            Schedule::every(Duration::from_secs(60)).limit(1),
            move |res: Resources| {
                let seen = s.clone();
                async move {
                    *seen.lock().unwrap() = Some(res.dep::<Db>()?.0);
                    Ok::<_, Error>(())
                }
            },
        )
        .test();
    c.start().await;
    eventually("first run", || seen.lock().unwrap().is_some()).await;
    assert_eq!(*seen.lock().unwrap(), Some("ready"));
    c.shutdown().await;
}

#[tokio::test]
async fn delay_start_postpones_the_first_run() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let c = App::new()
        .schedule(
            Schedule::every(Duration::from_millis(5)).delay_start(Duration::from_millis(150)),
            move |_res: Resources| {
                let runs = r.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                }
            },
        )
        .test();
    c.start().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    eventually("delayed run", || runs.load(Ordering::SeqCst) >= 1).await;
    c.shutdown().await;
}

#[tokio::test]
async fn failing_and_panicking_jobs_keep_their_schedule() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let c = App::new()
        .schedule(
            Schedule::every(Duration::from_millis(5)).name("flaky"),
            move |_res: Resources| {
                let runs = r.clone();
                async move {
                    match runs.fetch_add(1, Ordering::SeqCst) {
                        0 => panic!("first run panics"),
                        1 => Err(Error::internal("second run fails")),
                        _ => Ok(()),
                    }
                }
            },
        )
        .get("/", || async { "alive" })
        .test();
    c.start().await;
    eventually("runs after failures", || runs.load(Ordering::SeqCst) >= 4).await;
    assert_eq!(c.get("/").send().await.text(), "alive");
    c.shutdown().await;
}

/// Records the maximum number of concurrent runs.
fn concurrency_probe(
    period: Duration,
    work: Duration,
    overlap: bool,
) -> (App, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (active, peak, runs) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    let (a, p, r) = (active.clone(), peak.clone(), runs.clone());
    let mut schedule = Schedule::every(period);
    if overlap {
        schedule = schedule.allow_overlap();
    }
    let app = App::new().schedule(schedule, move |_res: Resources| {
        let (active, peak, runs) = (a.clone(), p.clone(), r.clone());
        async move {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(work).await;
            active.fetch_sub(1, Ordering::SeqCst);
            runs.fetch_add(1, Ordering::SeqCst);
        }
    });
    (app, peak, runs)
}

#[tokio::test]
async fn runs_do_not_overlap_by_default() {
    let (app, peak, runs) =
        concurrency_probe(Duration::from_millis(5), Duration::from_millis(25), false);
    let c = app.test();
    c.start().await;
    eventually("several runs", || runs.load(Ordering::SeqCst) >= 3).await;
    c.shutdown().await;
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn allow_overlap_runs_concurrently_and_shutdown_drains_them() {
    let (app, peak, runs) =
        concurrency_probe(Duration::from_millis(5), Duration::from_millis(40), true);
    let c = app.tasks_grace_period(Duration::from_secs(5)).test();
    c.start().await;
    eventually("overlapping runs", || peak.load(Ordering::SeqCst) >= 2).await;
    c.shutdown().await;
    // Shutdown waited for every in-flight run to finish.
    let finished = runs.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(runs.load(Ordering::SeqCst), finished);
}

#[tokio::test]
async fn limit_stops_after_n_runs() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let c = App::new()
        .schedule(
            Schedule::every(Duration::from_millis(2)).limit(3),
            move |_res: Resources| {
                let runs = r.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                }
            },
        )
        .test();
    c.start().await;
    eventually("3 runs", || runs.load(Ordering::SeqCst) == 3).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 3);
    c.shutdown().await;
}

#[test]
fn daily_schedule_computes_the_next_utc_occurrence() {
    let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
    let day = 86_400;
    let s = Schedule::daily_at(3, 30);
    // 01:00 UTC -> 2h30m later.
    assert_eq!(
        s.delay_until_next(at(day * 10 + 3600)),
        Duration::from_secs(9000)
    );
    // 04:00 UTC -> tomorrow 03:30.
    assert_eq!(
        s.delay_until_next(at(day * 10 + 4 * 3600)),
        Duration::from_secs(day - 1800)
    );
    // Exactly 03:30 -> a full day (never runs twice for the same minute).
    assert_eq!(
        s.delay_until_next(at(day * 10 + 3 * 3600 + 1800)),
        Duration::from_secs(day)
    );
    assert_eq!(
        Schedule::every(Duration::from_secs(5)).delay_until_next(SystemTime::now()),
        Duration::from_secs(5)
    );
}

#[test]
#[should_panic(expected = "invalid time 24:00")]
fn daily_schedule_rejects_invalid_times() {
    let _ = Schedule::daily_at(24, 0);
}

#[test]
#[should_panic(expected = "period must be > 0")]
fn zero_period_is_rejected() {
    let _ = Schedule::every(Duration::ZERO);
}

// ---- workers & shutdown ---------------------------------------------------------

#[tokio::test]
async fn worker_runs_until_shutdown() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicBool::new(false));
    let (t, s) = (ticks.clone(), stopped.clone());
    let c = App::new()
        .background(move |_res: Resources, shutdown: Shutdown| async move {
            while !shutdown.is_shutdown() {
                tokio::select! {
                    _ = shutdown.wait() => {}
                    _ = tokio::time::sleep(Duration::from_millis(2)) => {
                        t.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            s.store(true, Ordering::SeqCst);
        })
        .test();
    c.start().await;
    eventually("worker ticks", || ticks.load(Ordering::SeqCst) >= 3).await;
    assert!(!stopped.load(Ordering::SeqCst));
    c.shutdown().await;
    assert!(
        stopped.load(Ordering::SeqCst),
        "shutdown waited for the worker"
    );
}

#[tokio::test]
async fn stuck_worker_is_aborted_after_the_grace_period() {
    struct Flag(Arc<AtomicBool>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let d = dropped.clone();
    let c = App::new()
        .tasks_grace_period(Duration::from_millis(50))
        .background(move |_res: Resources, _shutdown: Shutdown| async move {
            let _flag = Flag(d);
            std::future::pending::<()>().await; // ignores shutdown
        })
        .test();
    c.start().await;
    let started = std::time::Instant::now();
    c.shutdown().await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(40),
        "waited for the grace period: {took:?}"
    );
    assert!(
        took < Duration::from_secs(3),
        "bounded by the grace period: {took:?}"
    );
    eventually("worker aborted", || dropped.load(Ordering::SeqCst)).await;
}

#[tokio::test]
async fn failing_worker_does_not_affect_the_app() {
    let c = App::new()
        .background(|_res: Resources, _s: Shutdown| async {
            Err::<(), _>(Error::internal("queue unreachable"))
        })
        .background(|_res: Resources, _s: Shutdown| async { boom("worker bug") })
        .get("/", || async { "ok" })
        .test();
    c.start().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(c.get("/").send().await.text(), "ok");
    c.shutdown().await;
}

#[tokio::test]
async fn shutdown_token_is_available_to_handlers_and_jobs() {
    let seen = Arc::new(AtomicBool::new(false));
    let s = seen.clone();
    let c = App::new()
        .background(move |res: Resources, shutdown: Shutdown| async move {
            let from_state = res.get::<Shutdown>().expect("registered as state");
            shutdown.wait().await;
            s.store(from_state.is_shutdown(), Ordering::SeqCst);
        })
        .get("/", |State(s): State<Shutdown>| async move {
            if s.is_shutdown() {
                "stopping"
            } else {
                "running"
            }
        })
        .test();
    assert_eq!(c.get("/").send().await.text(), "running");
    let res = c.resources().await;
    c.shutdown().await;
    assert!(res.get::<Shutdown>().unwrap().is_shutdown());
    assert!(
        seen.load(Ordering::SeqCst),
        "worker saw the same signal via Resources"
    );
}

#[tokio::test]
async fn shutdown_waits_for_queued_background_tasks() {
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    let c = App::new()
        .tasks_grace_period(Duration::from_secs(5))
        .post("/", move |tasks: BackgroundTasks| {
            let done = d.clone();
            async move {
                tasks.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    done.store(true, Ordering::SeqCst);
                });
            }
        })
        .test();
    c.post("/").send().await;
    c.shutdown().await;
    assert!(done.load(Ordering::SeqCst), "shutdown drained the task");
}

#[tokio::test]
async fn shutdown_channel_for_unit_tests() {
    let (trigger, shutdown) = Shutdown::channel();
    let other = trigger.subscribe();
    let waiter = tokio::spawn({
        let s = shutdown.clone();
        async move { s.wait().await }
    });
    assert!(!shutdown.is_shutdown());
    trigger.trigger();
    waiter.await.unwrap();
    assert!(shutdown.is_shutdown() && other.is_shutdown());
    assert_eq!(format!("{shutdown:?}"), "Shutdown { is_shutdown: true }");

    // Dropping every trigger also counts as shutdown.
    let (trigger, shutdown) = Shutdown::channel();
    drop(trigger);
    assert!(shutdown.is_shutdown());
    shutdown.wait().await;
}

#[tokio::test]
async fn jobs_run_under_app_serve() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let app = App::new()
        .schedule(
            Schedule::every(Duration::from_millis(5)),
            move |_res: Resources| {
                let runs = r.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                }
            },
        )
        .get("/", || async { "ok" });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = tokio::spawn(app.serve(listener));
    eventually("job runs while serving", || {
        runs.load(Ordering::SeqCst) >= 2
    })
    .await;
    server.abort();
}
