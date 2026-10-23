//! Regression tests for issues found in the pre-release review.
#![cfg(feature = "query")]

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use zapline::prelude::*;

#[tokio::test]
async fn redirect_to_invalid_location_is_a_500_not_a_panic() {
    #[derive(serde::Deserialize)]
    struct Next {
        next: String,
    }
    let c = App::new()
        .get("/login", |Query(q): Query<Next>| async move {
            Redirect::to(&q.next)
        })
        .test();
    let r = c.get("/login?next=/x%0d%0aSet-Cookie:%20a=b").send().await;
    assert_eq!(r.status(), 500);
    assert_eq!(r.header("set-cookie"), None);
    assert_eq!(r.header("location"), None);
    let r = c.get("/login?next=/home").send().await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.header("location"), Some("/home"));
}

/// Queued background tasks must be drained at shutdown under `App::run`,
/// even though the per-core worker runtimes stop first.
#[test]
fn app_run_drains_queued_background_tasks_on_shutdown() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let server = std::thread::spawn(move || {
        App::new()
            .tasks_grace_period(Duration::from_secs(5))
            .get("/", move |tasks: BackgroundTasks| {
                let flag = flag.clone();
                async move {
                    tasks.spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        flag.store(true, Ordering::SeqCst);
                    });
                    "queued"
                }
            })
            .workers(1)
            .server_config_mut_ext(|c| c.banner = false)
            .run_with_shutdown(("127.0.0.1", port), async move {
                let _ = stop_rx.await;
            })
            .unwrap();
        tx.send(()).unwrap();
    });

    // Wait for the server, make one request, then shut down immediately.
    let mut stream = loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    assert!(out.ends_with("queued"), "{out}");
    stop_tx.send(()).unwrap();

    rx.recv_timeout(Duration::from_secs(10))
        .expect("server did not stop");
    server.join().unwrap();
    assert!(
        done.load(Ordering::SeqCst),
        "background task was dropped at shutdown"
    );
}

trait ConfigExt {
    fn server_config_mut_ext(self, f: impl FnOnce(&mut zapline::ServerConfig)) -> Self;
}

impl ConfigExt for App {
    fn server_config_mut_ext(mut self, f: impl FnOnce(&mut zapline::ServerConfig)) -> Self {
        f(self.server_config_mut());
        self
    }
}
