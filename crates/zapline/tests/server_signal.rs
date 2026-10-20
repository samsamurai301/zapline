//! OS signal handling. This lives in its own test binary because a signal
//! reaches every server in the process.
#![cfg(target_os = "linux")]

mod server_support;

use std::time::Duration;

use server_support::*;
use zapline::prelude::*;

fn send_signal(name: &str) {
    let status = std::process::Command::new("kill")
        .arg(format!("-{name}"))
        .arg(std::process::id().to_string())
        .status()
        .expect("failed to run `kill`");
    assert!(status.success());
}

#[tokio::test]
async fn sigterm_drains_and_a_second_signal_forces_shutdown() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = App::new()
        .get("/", || async { "ok" })
        .get("/slow", || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "done"
        })
        .get("/hang", || async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            "never"
        });
    // Plain `serve`: stops on SIGINT/SIGTERM, default 30s shutdown timeout.
    let server = tokio::spawn(app.serve(listener));

    // Once a request was served, the signal handlers are installed.
    let out = raw(
        addr,
        "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(out.ends_with("ok"), "{out}");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let slow = tokio::spawn(raw(addr, "GET /slow HTTP/1.1\r\nHost: x\r\n\r\n"));
    let hang = tokio::spawn(raw(addr, "GET /hang HTTP/1.1\r\nHost: x\r\n\r\n"));
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_signal("TERM");
    wait_refused(addr).await;
    let out = slow.await.unwrap();
    assert!(
        out.starts_with("HTTP/1.1 200 OK") && out.ends_with("done"),
        "{out}"
    );

    // `/hang` keeps the server draining...
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !server.is_finished(),
        "server stopped before the in-flight request ended"
    );

    // ...until a second signal skips the grace period.
    send_signal("INT");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("second signal did not force shutdown")
        .unwrap()
        .unwrap();
    assert_eq!(hang.await.unwrap(), "");
}
