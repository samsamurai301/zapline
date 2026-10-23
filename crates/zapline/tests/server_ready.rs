//! Zero-downtime swaps under a supervisor: the systemd notify protocol, the
//! in-process readiness check, and two instances sharing one port. Its own
//! test binary because it sets process-wide environment variables.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use zapline::prelude::*;
use zapline::server::{ENV_READY_PATH, ENV_READY_TIMEOUT};

/// An instance that answers `/` with its name; `/ready` fails `warmup`
/// times before it passes.
fn instance(name: &'static str, warmup: usize, checks: Arc<AtomicUsize>) -> App {
    App::new()
        .get("/", move || async move { name })
        .get("/ready", move || {
            let checks = checks.clone();
            async move {
                if checks.fetch_add(1, Ordering::SeqCst) < warmup {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::OK
                }
            }
        })
}

type Stop = tokio::sync::oneshot::Sender<()>;

fn start(app: App, addr: SocketAddr) -> (Stop, JoinHandle<std::io::Result<()>>) {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let handle = std::thread::spawn(move || {
        app.run_with_shutdown(addr, async move {
            let _ = stopped.await;
        })
    });
    (stop, handle)
}

fn recv(sock: &UnixDatagram) -> String {
    let mut buf = [0u8; 256];
    let n = sock.recv(&mut buf).expect("no notification in time");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn get(addr: SocketAddr) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")?;
    let mut out = String::new();
    stream.read_to_string(&mut out)?;
    if !out.starts_with("HTTP/1.1 200") {
        return Err(std::io::Error::other(out));
    }
    Ok(out.rsplit("\r\n\r\n").next().unwrap_or_default().to_owned())
}

/// Whether the kernel hands connections queued on a closing listener to
/// another one in its `SO_REUSEPORT` group (Linux 5.14+, off by default;
/// `zapline ship` turns it on).
fn migrates_requests() -> bool {
    std::fs::read_to_string("/proc/sys/net/ipv4/tcp_migrate_req").is_ok_and(|v| v.trim() == "1")
}

#[test]
fn ready_after_self_check_and_swap_without_dropping_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let sock_path = tmp.path().join("notify.sock");
    let sock = UnixDatagram::bind(&sock_path).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    // The only test in this binary; set before any server starts.
    std::env::set_var("NOTIFY_SOCKET", &sock_path);
    std::env::set_var(ENV_READY_PATH, "/ready");
    std::env::set_var(ENV_READY_TIMEOUT, "10");
    let addr: SocketAddr = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap()
    };

    // The old version: READY=1 only after its own check passed.
    let checks_a = Arc::new(AtomicUsize::new(0));
    let (stop_a, a) = start(instance("A", 3, checks_a.clone()), addr);
    assert_eq!(recv(&sock), "READY=1");
    assert_eq!(checks_a.load(Ordering::SeqCst), 4);
    assert_eq!(get(addr).unwrap(), "A");

    // Traffic keeps flowing during the whole swap.
    let done = Arc::new(AtomicBool::new(false));
    let traffic = {
        let done = done.clone();
        std::thread::spawn(move || {
            let (mut ok, mut failed, mut seen_b) = (0, Vec::new(), false);
            while !done.load(Ordering::SeqCst) {
                match get(addr) {
                    Ok(body) => {
                        ok += 1;
                        seen_b |= body == "B";
                    }
                    Err(e) => failed.push(e.to_string()),
                }
            }
            (ok, failed, seen_b)
        })
    };

    // The new version binds the same port while the old one serves.
    let checks_b = Arc::new(AtomicUsize::new(0));
    let (stop_b, b) = start(instance("B", 2, checks_b.clone()), addr);
    assert_eq!(recv(&sock), "READY=1");
    assert_eq!(checks_b.load(Ordering::SeqCst), 3);

    // Stop the old one: it says so, drains and exits.
    std::thread::sleep(Duration::from_millis(200));
    stop_a.send(()).unwrap();
    assert_eq!(recv(&sock), "STOPPING=1");
    a.join().unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while get(addr).unwrap() != "B" {
        assert!(Instant::now() < deadline, "the new version never answered");
    }
    std::thread::sleep(Duration::from_millis(200));
    done.store(true, Ordering::SeqCst);
    let (ok, failed, seen_b) = traffic.join().unwrap();
    assert!(ok > 10, "only {ok} requests went through");
    assert!(seen_b);
    if migrates_requests() {
        assert!(failed.is_empty(), "{} failed: {:?}", failed.len(), failed);
    } else {
        // Without `net.ipv4.tcp_migrate_req`, connections still queued on
        // the old listener when it closes are reset by the kernel; only
        // those (at the close instant) may fail.
        assert!(failed.len() <= 3, "{} failed: {:?}", failed.len(), failed);
    }

    stop_b.send(()).unwrap();
    assert_eq!(recv(&sock), "STOPPING=1");
    b.join().unwrap().unwrap();

    // A version that never gets ready fails to start, without READY=1 and
    // without ever taking the port.
    std::env::set_var(ENV_READY_TIMEOUT, "0.5");
    let started = Instant::now();
    let (_stop, c) = start(
        instance("C", usize::MAX, Arc::new(AtomicUsize::new(0))),
        addr,
    );
    let err = c.join().unwrap().unwrap_err().to_string();
    assert!(err.contains("readiness check failed"), "{err}");
    assert!(err.contains("503"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    sock.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 64];
    assert!(
        sock.recv(&mut buf).is_err(),
        "a failed start sent a notification"
    );
    assert!(get(addr).is_err(), "the port was bound");
}
