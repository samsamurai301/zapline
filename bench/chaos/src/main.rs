//! Hostile-traffic, chaos and soak tests against a real zapline server.
//!
//! Each scenario starts a fresh `target` server (see `src/bin/target.rs`),
//! attacks it over real sockets, then checks that the process is still
//! alive, still answers a normal request quickly, and gave back the file
//! descriptors and memory the attack used.
//!
//!     ./bench/chaos/run.sh                  # every scenario, 60s soak
//!     ./bench/chaos/run.sh soak=600         # 10 minute soak
//!     ./bench/chaos/run.sh slowloris h2     # only scenarios whose name contains these
//!
//! Exit status is non-zero when a scenario fails.

use std::io;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

// ---------------------------------------------------------------------------
// The server under test
// ---------------------------------------------------------------------------

struct Target {
    child: Child,
    addr: SocketAddr,
}

impl Target {
    async fn start(env: &[(&str, &str)]) -> Target {
        Self::start_with(env, None).await
    }

    /// Start the server, optionally under `ulimit -n <nofile>`.
    async fn start_with(env: &[(&str, &str)], nofile: Option<u32>) -> Target {
        let port = free_port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let bin = std::env::current_exe().unwrap().with_file_name("target");
        let mut cmd = match nofile {
            Some(n) => {
                let mut c = Command::new("sh");
                c.arg("-c")
                    .arg(format!("ulimit -n {n} && exec \"$0\" \"$1\""))
                    .arg(&bin)
                    .arg(addr.to_string());
                c
            }
            None => {
                let mut c = Command::new(&bin);
                c.arg(addr.to_string());
                c
            }
        };
        cmd.envs(env.iter().copied())
            .env("RUST_LOG", "off")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("start target (build it with --bins)");
        let t = Target { child, addr };
        for _ in 0..200 {
            if TcpStream::connect(addr).await.is_ok() {
                return t;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("target did not start on {addr}");
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn rss_kib(&self) -> u64 {
        proc_field(self.pid(), "VmRSS:")
    }

    fn fds(&self) -> usize {
        std::fs::read_dir(format!("/proc/{}/fd", self.pid()))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    /// User + system CPU time in clock ticks (1/100 s on Linux).
    fn cpu_ticks(&self) -> u64 {
        let stat =
            std::fs::read_to_string(format!("/proc/{}/stat", self.pid())).unwrap_or_default();
        let after = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
        let f: Vec<&str> = after.split_whitespace().collect();
        let utime: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
        let stime: u64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
        utime + stime
    }

    /// One fresh connection, `GET /`, answered with 200 within a second.
    async fn healthy(&self) -> Result<Duration, String> {
        let start = Instant::now();
        let resp = exchange(
            self.addr,
            b"GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
            Duration::from_secs(1),
        )
        .await
        .map_err(|e| format!("health check failed: {e}"))?;
        if !resp.starts_with(b"HTTP/1.1 200") {
            return Err(format!("health check got {:?}", head_line(&resp)));
        }
        Ok(start.elapsed())
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn proc_field(pid: u32, field: &str) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Raw socket helpers
// ---------------------------------------------------------------------------

/// Write `req`, then read until the server closes or `limit` passes.
async fn exchange(addr: SocketAddr, req: &[u8], limit: Duration) -> io::Result<Vec<u8>> {
    let mut s = timeout(limit, TcpStream::connect(addr))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect"))??;
    // The server may answer (413, 431) and close before the request is
    // fully written: keep going and read what it sent.
    let _ = s.write_all(req).await;
    let mut out = Vec::new();
    let read = timeout(limit, async {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match s.read(&mut buf).await {
                Ok(0) => return Ok(()),
                Ok(n) => out.extend_from_slice(&buf[..n]),
                // A reset after the response arrived still counts as an answer.
                Err(e) if !out.is_empty() => return Err::<(), _>(e).or(Ok(())),
                Err(e) => return Err(e),
            }
        }
    })
    .await;
    match read {
        Ok(r) => r.map(|_| out),
        // Still open after `limit`: return what arrived (keep-alive).
        Err(_) if !out.is_empty() => Ok(out),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "no response")),
    }
}

fn head_line(resp: &[u8]) -> String {
    let end = resp
        .iter()
        .position(|&b| b == b'\r')
        .unwrap_or(resp.len().min(60));
    String::from_utf8_lossy(&resp[..end]).into_owned()
}

/// The status code of the first response in `resp`, if any.
fn status(resp: &[u8]) -> Option<u16> {
    let line = head_line(resp);
    line.split_whitespace().nth(1)?.parse().ok()
}

/// True when the peer has closed (or reset) the connection.
async fn closed_by_server(s: &mut TcpStream, wait: Duration) -> bool {
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, s.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => continue,
            Err(_) => return false,
        }
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

struct Outcome {
    name: &'static str,
    pass: bool,
    detail: String,
}

fn ok(name: &'static str, detail: impl Into<String>) -> Outcome {
    Outcome {
        name,
        pass: true,
        detail: detail.into(),
    }
}

fn fail(name: &'static str, detail: impl Into<String>) -> Outcome {
    Outcome {
        name,
        pass: false,
        detail: detail.into(),
    }
}

/// Common post-attack checks: alive, healthy, and fds back near baseline.
async fn settle(name: &'static str, t: &mut Target, base_fds: usize, detail: String) -> Outcome {
    if !t.alive() {
        return fail(name, format!("server process died. {detail}"));
    }
    if let Err(e) = t.healthy().await {
        return fail(name, format!("{e}. {detail}"));
    }
    // Connections the attack left behind should be gone within a few seconds.
    let mut fds = t.fds();
    for _ in 0..50 {
        if fds <= base_fds + 4 {
            break;
        }
        sleep(Duration::from_millis(100)).await;
        fds = t.fds();
    }
    if fds > base_fds + 4 {
        return fail(
            name,
            format!("fd leak: {base_fds} before, {fds} after. {detail}"),
        );
    }
    ok(name, detail)
}

/// Thousands of connections sending random bytes.
async fn garbage() -> Outcome {
    let name = "garbage bytes";
    let mut t = Target::start(&[]).await;
    let base = t.fds();
    let addr = t.addr;
    let mut tasks = Vec::new();
    for i in 0..5_000u32 {
        tasks.push(tokio::spawn(async move {
            let len = fastrand::usize(0..4096);
            let mut junk: Vec<u8> = (0..len).map(|_| fastrand::u8(..)).collect();
            // Half start like a request line, so the parser goes deeper.
            if i % 2 == 0 {
                let mut req = b"GET /".to_vec();
                req.append(&mut junk);
                junk = req;
            }
            exchange(addr, &junk, Duration::from_secs(3))
                .await
                .ok()
                .and_then(|r| status(&r))
        }));
        if tasks.len() >= 500 {
            for h in tasks.drain(..) {
                let _ = h.await;
            }
        }
    }
    let mut server_errors = 0;
    for h in tasks {
        if let Ok(Some(s)) = h.await {
            if s >= 500 {
                server_errors += 1;
            }
        }
    }
    if server_errors > 0 {
        return fail(name, format!("{server_errors} responses were 5xx"));
    }
    settle(
        name,
        &mut t,
        base,
        "5,000 connections of random bytes".into(),
    )
    .await
}

/// Hand-written malformed and smuggling-style requests. Each must get a 4xx
/// or a closed connection, never a 2xx/5xx, and the server must stay up.
async fn malformed(pipeline_flush: bool) -> Outcome {
    let name = if pipeline_flush {
        "malformed requests (pipeline_flush on)"
    } else {
        "malformed requests (pipeline_flush off)"
    };
    let env: &[(&str, &str)] = if pipeline_flush {
        &[("ZL_PIPELINE_FLUSH", "1")]
    } else {
        &[]
    };
    let mut t = Target::start(env).await;
    let base = t.fds();
    let long_uri = format!("GET /{} HTTP/1.1\r\nHost: t\r\n\r\n", "a".repeat(100_000));
    let huge_header = format!(
        "GET / HTTP/1.1\r\nHost: t\r\nX-Big: {}\r\n\r\n",
        "b".repeat(1_000_000)
    );
    let many_headers = format!(
        "GET / HTTP/1.1\r\nHost: t\r\n{}\r\n",
        (0..1000)
            .map(|i| format!("X-H{i}: v\r\n"))
            .collect::<String>()
    );
    let cases: Vec<(&str, Vec<u8>, &[u16])> = vec![
        ("bad method", b"G\x01T / HTTP/1.1\r\nHost: t\r\n\r\n".to_vec(), &[400]),
        ("bad version", b"GET / HTTP/9.9\r\nHost: t\r\n\r\n".to_vec(), &[400, 505]),
        ("header without colon", b"GET / HTTP/1.1\r\nHost: t\r\nNoColon\r\n\r\n".to_vec(), &[400]),
        ("NUL in header value", b"GET / HTTP/1.1\r\nHost: t\r\nX: a\x00b\r\n\r\n".to_vec(), &[400]),
        ("bare CR", b"GET / HTTP/1.1\rHost: t\r\n\r\n".to_vec(), &[400]),
        ("obs-fold header", b"GET / HTTP/1.1\r\nHost: t\r\nX: a\r\n b\r\n\r\n".to_vec(), &[400]),
        (
            "conflicting Content-Length",
            b"POST /json HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n{}x".to_vec(),
            &[400],
        ),
        ("negative Content-Length", b"POST /json HTTP/1.1\r\nHost: t\r\nContent-Length: -1\r\n\r\n".to_vec(), &[400]),
        (
            "overflowing Content-Length",
            b"POST /json HTTP/1.1\r\nHost: t\r\nContent-Length: 99999999999999999999999\r\n\r\n".to_vec(),
            &[400],
        ),
        (
            "bad chunk size",
            b"POST /json HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n{}\r\n0\r\n\r\n".to_vec(),
            &[400],
        ),
        (
            "TE not ending in chunked",
            b"POST /json HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: gzip\r\n\r\n{}".to_vec(),
            &[400, 411, 415],
        ),
        ("100 KB URI", long_uri.into_bytes(), &[414, 400, 431]),
        ("1 MB header", huge_header.into_bytes(), &[431, 400]),
        ("1,000 headers", many_headers.into_bytes(), &[431, 400]),
        ("invalid UTF-8 path", b"GET /users/\xff\xfe HTTP/1.1\r\nHost: t\r\n\r\n".to_vec(), &[400, 404]),
        ("percent NUL in path", b"GET /files/a%00b HTTP/1.1\r\nHost: t\r\n\r\n".to_vec(), &[200, 400, 404]),
        ("overflowing u64 param", b"GET /users/99999999999999999999999 HTTP/1.1\r\nHost: t\r\n\r\n".to_vec(), &[400, 404]),
        ("bad percent escape", b"GET /query?a=%zz&%=1 HTTP/1.1\r\nHost: t\r\n\r\n".to_vec(), &[200, 400]),
        (
            "JSON nested 100,000 deep",
            {
                let body = "[".repeat(100_000);
                format!("POST /json HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                    .into_bytes()
            },
            &[400, 422],
        ),
        (
            "JSON with wrong content type",
            b"POST /json HTTP/1.1\r\nHost: t\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            &[415, 400],
        ),
        (
            "multipart without boundary",
            b"POST /upload HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data\r\nContent-Length: 4\r\n\r\nabcd".to_vec(),
            &[400, 415],
        ),
        (
            "truncated multipart",
            b"POST /upload HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=X\r\nContent-Length: 30\r\n\r\n--X\r\nContent-Disposition: form-da".to_vec(),
            &[400],
        ),
        (
            "WebSocket upgrade missing key",
            b"GET /ws HTTP/1.1\r\nHost: t\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\r\n".to_vec(),
            &[400, 426],
        ),
        (
            "h2c upgrade header on HTTP/1.1",
            b"GET / HTTP/1.1\r\nHost: t\r\nConnection: Upgrade, HTTP2-Settings\r\nUpgrade: h2c\r\nHTTP2-Settings: AAMAAABkAAQAAP__\r\n\r\n".to_vec(),
            &[200, 400],
        ),
    ];
    let mut bad = Vec::new();
    let mut silent = Vec::new();
    let mut seen = Vec::new();
    for (label, req, allowed) in &cases {
        let resp = exchange(t.addr, req, Duration::from_secs(2)).await;
        match resp.as_deref().ok().and_then(status) {
            Some(s) if allowed.contains(&s) => seen.push(format!("{label}: {s}")),
            Some(s) => bad.push(format!("{label}: got {s}")),
            None => {
                silent.push(*label);
                seen.push(format!("{label}: closed"));
            }
        }
        if !t.alive() {
            return fail(name, format!("server died on `{label}`"));
        }
    }
    let detail = format!(
        "{} cases; closed without a status: {}",
        cases.len(),
        if silent.is_empty() {
            "none".to_string()
        } else {
            silent.join(", ")
        }
    );
    if !bad.is_empty() {
        return fail(
            name,
            format!("unexpected statuses: {}. {detail}", bad.join("; ")),
        );
    }
    if std::env::var_os("CHAOS_VERBOSE").is_some() {
        for s in &seen {
            println!("      {s}");
        }
    }
    settle(name, &mut t, base, detail).await
}

/// Slowloris: many connections dribbling request headers. They must be cut
/// off by `header_read_timeout`, and the server must keep answering others.
async fn slowloris() -> Outcome {
    let name = "slowloris (slow headers)";
    let mut t = Target::start(&[("ZL_HEADER_TIMEOUT_MS", "2000")]).await;
    let base = t.fds();
    let addr = t.addr;
    let mut tasks = Vec::new();
    for _ in 0..1_000 {
        tasks.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.ok()?;
            s.write_all(b"GET / HTTP/1.1\r\nHost: t\r\n").await.ok()?;
            let start = Instant::now();
            // One header byte every 200ms, for up to 6s.
            while start.elapsed() < Duration::from_secs(6) {
                if s.write_all(b"X").await.is_err() {
                    return Some(start.elapsed());
                }
                let mut buf = [0u8; 64];
                match timeout(Duration::from_millis(200), s.read(&mut buf)).await {
                    Ok(Ok(0)) | Ok(Err(_)) => return Some(start.elapsed()),
                    _ => {}
                }
            }
            None
        }));
    }
    sleep(Duration::from_millis(500)).await;
    let during = t.healthy().await;
    let mut cut = Vec::new();
    let mut survived = 0;
    for h in tasks {
        match h.await.ok().flatten() {
            Some(d) => cut.push(d),
            None => survived += 1,
        }
    }
    if let Err(e) = during {
        return fail(
            name,
            format!("server stopped answering during the attack: {e}"),
        );
    }
    if survived > 0 {
        return fail(
            name,
            format!("{survived} of 1,000 slow connections were never cut off"),
        );
    }
    cut.sort();
    let detail = format!(
        "1,000 connections cut after {:.1}s (median), health check answered during the attack in {:?}",
        cut[cut.len() / 2].as_secs_f64(),
        during.unwrap()
    );
    settle(name, &mut t, base, detail).await
}

/// Keep-alive connections that go quiet after one request.
async fn idle_keepalive() -> Outcome {
    let name = "idle keep-alive connections";
    let mut t = Target::start(&[("ZL_HEADER_TIMEOUT_MS", "2000")]).await;
    let base = t.fds();
    let mut conns = Vec::new();
    for _ in 0..500 {
        let mut s = TcpStream::connect(t.addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let _ = s.read(&mut buf).await;
        conns.push(s);
    }
    let mut closed = 0;
    let waits = conns
        .iter_mut()
        .map(|s| closed_by_server(s, Duration::from_secs(5)));
    for c in futures_util::future::join_all(waits).await {
        if c {
            closed += 1;
        }
    }
    drop(conns);
    let detail = format!(
        "{closed} of 500 idle connections closed by the server within 5s (header_read_timeout 2s)"
    );
    if closed < 500 {
        return fail(name, detail);
    }
    settle(name, &mut t, base, detail).await
}

/// Request bodies dribbled one byte at a time.
async fn slow_body() -> Outcome {
    let name = "slow request bodies";
    let mut t = Target::start(&[
        ("ZL_HEADER_TIMEOUT_MS", "2000"),
        ("ZL_BODY_TIMEOUT_MS", "3000"),
    ])
    .await;
    let base = t.fds();
    let addr = t.addr;
    let secs = 10;
    let mut tasks = Vec::new();
    for _ in 0..200 {
        tasks.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.ok()?;
            s.write_all(b"POST /json HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: 100000\r\n\r\n")
                .await
                .ok()?;
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(secs) {
                if s.write_all(b" ").await.is_err() {
                    return Some(start.elapsed());
                }
                let mut buf = [0u8; 256];
                match timeout(Duration::from_millis(500), s.read(&mut buf)).await {
                    Ok(Ok(0)) | Ok(Err(_)) => return Some(start.elapsed()),
                    Ok(Ok(_)) => return Some(start.elapsed()),
                    Err(_) => {}
                }
            }
            None
        }));
    }
    let mut open = 0;
    for h in tasks {
        if h.await.ok().flatten().is_none() {
            open += 1;
        }
    }
    let detail = format!("{open} of 200 connections still open after {secs}s of 2 bytes/s bodies (body_read_timeout 3s)");
    if open > 0 {
        return fail(name, detail);
    }
    settle(name, &mut t, base, detail).await
}

/// Clients that request a large response and never read it.
async fn slow_reader() -> Outcome {
    let name = "clients that never read the response";
    let mut t = Target::start(&[]).await;
    let base = t.fds();
    let rss0 = t.rss_kib();
    let mut conns = Vec::new();
    for _ in 0..200 {
        let mut s = TcpStream::connect(t.addr).await.unwrap();
        s.write_all(b"GET /big HTTP/1.1\r\nHost: t\r\n\r\n")
            .await
            .unwrap();
        conns.push(s);
    }
    sleep(Duration::from_secs(2)).await;
    let rss = t.rss_kib();
    let during = t.healthy().await;
    let mut closed = 0;
    let waits = conns.iter_mut().map(|s| async {
        // Never read the body: only check whether the server gave up.
        let mut probe = [0u8; 0];
        let _ = &mut probe;
        sleep(Duration::from_secs(8)).await;
        s.try_write(b"").is_err()
    });
    for c in futures_util::future::join_all(waits).await {
        if c {
            closed += 1;
        }
    }
    drop(conns);
    let detail = format!(
        "200 stalled 4 MiB responses: RSS {} -> {} MiB, {} still held after 10s",
        rss0 / 1024,
        rss / 1024,
        200 - closed
    );
    if let Err(e) = during {
        return fail(name, format!("{e}. {detail}"));
    }
    settle(name, &mut t, base, detail).await
}

/// Connections dropped at every stage of a request, in two rounds: memory
/// may rise to a high-water mark in the first, but must not keep rising.
async fn disconnects() -> Outcome {
    let name = "abrupt disconnects";
    let mut t = Target::start(&[]).await;
    let base = t.fds();
    let addr = t.addr;
    let stages: [&[u8]; 6] = [
        b"",
        b"GET / HT",
        b"GET /slow HTTP/1.1\r\nHost: t\r\n\r\n",
        b"POST /json HTTP/1.1\r\nHost: t\r\nContent-Length: 1000\r\n\r\n{\"a\":",
        b"GET /big HTTP/1.1\r\nHost: t\r\n\r\n",
        b"GET /ws HTTP/1.1\r\nHost: t\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
    ];
    let n = 30_000;
    let mut rss = Vec::new();
    for _round in 0..2 {
        let mut tasks = Vec::new();
        for i in 0..n {
            let stage = stages[i % stages.len()];
            tasks.push(tokio::spawn(async move {
                if let Ok(mut s) = TcpStream::connect(addr).await {
                    let _ = s.write_all(stage).await;
                    if i % 3 == 0 {
                        // RST instead of FIN.
                        #[allow(deprecated)]
                        let _ = s.set_linger(Some(Duration::ZERO));
                    }
                    if i % 2 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            }));
            // Keep at most 200 in flight, like a busy but bounded client fleet.
            if tasks.len() >= 200 {
                for h in tasks.drain(..) {
                    let _ = h.await;
                }
            }
        }
        for h in tasks {
            let _ = h.await;
        }
        sleep(Duration::from_secs(2)).await;
        rss.push(t.rss_kib());
    }
    let detail = format!(
        "2 x {n} connections dropped mid-request/response; RSS after round 1 {} KiB, after round 2 {} KiB",
        rss[0], rss[1]
    );
    if rss[1] > rss[0] + 16 * 1024 {
        return fail(name, format!("memory kept growing: {detail}"));
    }
    settle(name, &mut t, base, detail).await
}

/// Bodies and inputs past the limits.
async fn oversized() -> Outcome {
    let name = "oversized bodies and inputs";
    let mut t = Target::start(&[]).await;
    let base = t.fds();
    let three_mb = "x".repeat(3 * 1024 * 1024);
    let json_cl = format!(
        "POST /json HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{three_mb}",
        three_mb.len()
    );
    let mut chunked = String::from("POST /json HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
    for _ in 0..48 {
        chunked.push_str(&format!("10000\r\n{}\r\n", "y".repeat(0x10000)));
    }
    chunked.push_str("0\r\n\r\n");
    let mp_body = format!("--X\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a\"\r\n\r\n{three_mb}\r\n--X--\r\n");
    let multipart = format!(
        "POST /upload HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{mp_body}",
        mp_body.len()
    );
    let many_fields: String = (0..150_000).map(|i| format!("k{i}=v&")).collect();
    let form = format!(
        "POST /form HTTP/1.1\r\nHost: t\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{many_fields}",
        many_fields.len()
    );
    let q: String = (0..5_000).map(|i| format!("k{i}=v&")).collect();
    let query = format!("GET /query?{q} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    let cases: Vec<(&str, String, &[u16])> = vec![
        ("3 MiB JSON with Content-Length", json_cl, &[413]),
        ("3 MiB chunked JSON", chunked, &[413]),
        ("3 MiB multipart file", multipart, &[413]),
        ("150,000 form fields (1.4 MiB)", form, &[200, 400, 413]),
        ("5,000 query params", query, &[200, 400, 414, 431]),
    ];
    let mut results = Vec::new();
    for (label, req, allowed) in &cases {
        let resp = exchange(t.addr, req.as_bytes(), Duration::from_secs(5)).await;
        let s = resp.as_deref().ok().and_then(status);
        match s {
            Some(s) if allowed.contains(&s) => results.push(format!("{label}: {s}")),
            other => {
                return fail(
                    name,
                    format!("{label}: expected {allowed:?}, got {other:?}"),
                )
            }
        }
    }
    settle(name, &mut t, base, results.join("; ")).await
}

/// Handlers that panic.
async fn panics(catch: bool) -> Outcome {
    let name = if catch {
        "panicking handlers (CatchPanic)"
    } else {
        "panicking handlers (no middleware)"
    };
    let env: &[(&str, &str)] = if catch {
        &[("ZL_CATCH_PANIC", "1")]
    } else {
        &[]
    };
    let mut t = Target::start(env).await;
    let base = t.fds();
    let mut statuses = std::collections::BTreeMap::new();
    for _ in 0..2_000 {
        let r = exchange(
            t.addr,
            b"GET /panic HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
            Duration::from_secs(2),
        )
        .await;
        let key = r
            .ok()
            .and_then(|r| status(&r))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "closed".into());
        *statuses.entry(key).or_insert(0) += 1;
    }
    let detail = format!("2,000 panics -> {statuses:?}");
    if catch && statuses.get("500") != Some(&2000) {
        return fail(name, detail);
    }
    settle(name, &mut t, base, detail).await
}

/// More connections than the process has file descriptors.
async fn fd_exhaustion() -> Outcome {
    let name = "file descriptor exhaustion";
    let mut t = Target::start_with(&[("ZL_WORKERS", "2")], Some(256)).await;
    let mut conns = Vec::new();
    for _ in 0..600 {
        if let Ok(Ok(s)) = timeout(Duration::from_millis(200), TcpStream::connect(t.addr)).await {
            conns.push(s);
        }
    }
    // While at the limit: the accept loop must back off, not spin.
    let c0 = t.cpu_ticks();
    sleep(Duration::from_secs(3)).await;
    let cpu = (t.cpu_ticks() - c0) as f64 / 3.0; // ticks per second = % of one core
    drop(conns);
    sleep(Duration::from_secs(2)).await;
    let detail =
        format!("600 connections against a 256 fd limit: {cpu:.0}% of one core while saturated");
    if !t.alive() {
        return fail(name, format!("server died. {detail}"));
    }
    // Recovery: it must serve again once descriptors free up.
    let mut healthy = Err(String::new());
    for _ in 0..20 {
        healthy = t.healthy().await;
        if healthy.is_ok() {
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }
    if let Err(e) = healthy {
        return fail(name, format!("did not recover: {e}. {detail}"));
    }
    if cpu > 50.0 {
        return fail(name, format!("accept loop spins: {detail}"));
    }
    ok(name, detail)
}

/// HTTP/2 abuse: rapid reset (CVE-2023-44487), PING and SETTINGS floods.
async fn h2_abuse() -> Outcome {
    let name = "HTTP/2 rapid reset and floods";
    let mut t = Target::start(&[]).await;
    let base = t.fds();
    fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(9 + payload.len());
        let len = payload.len() as u32;
        f.extend_from_slice(&len.to_be_bytes()[1..]);
        f.push(kind);
        f.push(flags);
        f.extend_from_slice(&stream.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }
    // :method GET, :scheme http, :path /, :authority t
    let headers = [0x82u8, 0x86, 0x84, 0x41, 0x01, b't'];
    let rst = 0x8u32.to_be_bytes(); // CANCEL
    let mut results = Vec::new();

    // Rapid reset: open and cancel 100k streams as fast as possible.
    let c0 = t.cpu_ticks();
    let start = Instant::now();
    let mut conn = TcpStream::connect(t.addr).await.unwrap();
    let mut out = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    out.extend(frame(0x4, 0, 0, &[]));
    conn.write_all(&out).await.unwrap();
    let reader = {
        let (mut r, mut w) = conn.into_split();
        let send = tokio::spawn(async move {
            let mut id = 1u32;
            let mut sent = 0u32;
            while sent < 100_000 {
                let mut batch = Vec::with_capacity(64 * 1024);
                for _ in 0..1000 {
                    batch.extend(frame(0x1, 0x5, id, &headers)); // END_STREAM | END_HEADERS
                    batch.extend(frame(0x3, 0, id, &rst));
                    id += 2;
                    sent += 1;
                }
                if w.write_all(&batch).await.is_err() {
                    break;
                }
            }
            sent
        });
        let recv = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut goaway = false;
            while let Ok(Ok(n)) = timeout(Duration::from_secs(3), r.read(&mut buf)).await {
                if n == 0 {
                    break;
                }
                // A GOAWAY frame header has type 0x7 at offset 3.
                if buf[..n].windows(4).any(|w| w[3] == 0x7 && w[0] == 0) {
                    goaway = true;
                }
            }
            goaway
        });
        (send, recv)
    };
    let sent = reader.0.await.unwrap_or(0);
    let goaway = reader.1.await.unwrap_or(false);
    let cpu = (t.cpu_ticks() - c0) as f64 / 100.0;
    results.push(format!(
        "rapid reset: {sent} streams sent in {:.1}s, server {}, {cpu:.1}s of CPU",
        start.elapsed().as_secs_f64(),
        if goaway {
            "sent GOAWAY"
        } else {
            "kept the connection"
        }
    ));
    if let Err(e) = t.healthy().await {
        return fail(
            name,
            format!("{e} after rapid reset. {}", results.join("; ")),
        );
    }

    // PING flood without reading the replies.
    let rss0 = t.rss_kib();
    let mut conn = TcpStream::connect(t.addr).await.unwrap();
    let mut out = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    out.extend(frame(0x4, 0, 0, &[]));
    let _ = conn.write_all(&out).await;
    let ping = frame(0x6, 0, 0, &[0u8; 8]);
    let mut batch = Vec::new();
    for _ in 0..10_000 {
        batch.extend_from_slice(&ping);
    }
    let mut pings = 0;
    for _ in 0..100 {
        match timeout(Duration::from_millis(500), conn.write_all(&batch)).await {
            Ok(Ok(())) => pings += 10_000,
            _ => break,
        }
    }
    sleep(Duration::from_millis(500)).await;
    let rss1 = t.rss_kib();
    drop(conn);
    results.push(format!(
        "PING flood: {pings} unread pings, RSS {} -> {} MiB",
        rss0 / 1024,
        rss1 / 1024
    ));
    if rss1 > rss0 + 100 * 1024 {
        return fail(
            name,
            format!("PING flood grew memory unbounded. {}", results.join("; ")),
        );
    }

    // SETTINGS flood.
    let mut conn = TcpStream::connect(t.addr).await.unwrap();
    let mut out = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    out.extend(frame(0x4, 0, 0, &[]));
    let settings = frame(0x4, 0, 0, &[0x0, 0x3, 0, 0, 0, 100]);
    for _ in 0..10_000 {
        out.extend_from_slice(&settings);
    }
    let r = timeout(Duration::from_secs(3), conn.write_all(&out)).await;
    let closed = closed_by_server(&mut conn, Duration::from_secs(3)).await;
    results.push(format!(
        "SETTINGS flood: {}",
        if closed || r.is_err() {
            "connection closed by server"
        } else {
            "connection kept"
        }
    ));
    drop(conn);
    settle(name, &mut t, base, results.join("; ")).await
}

/// WebSocket abuse: huge frame headers, many connections, ping floods.
async fn websocket() -> Outcome {
    let name = "WebSocket abuse";
    let mut t = Target::start(&[]).await;
    let base = t.fds();
    let url = format!("ws://{}/ws", t.addr);
    let mut results = Vec::new();

    // A frame that claims 2^62 bytes.
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let mut raw = ws.into_inner();
    let mut frame = vec![0x82u8, 0xFF]; // binary, masked, 64-bit length
    frame.extend_from_slice(&(1u64 << 62).to_be_bytes());
    frame.extend_from_slice(&[1, 2, 3, 4]);
    let _ = match &mut raw {
        tokio_tungstenite::MaybeTlsStream::Plain(s) => s.write_all(&frame).await,
        _ => unreachable!(),
    };
    let closed = match &mut raw {
        tokio_tungstenite::MaybeTlsStream::Plain(s) => {
            closed_by_server(s, Duration::from_secs(3)).await
        }
        _ => unreachable!(),
    };
    results.push(format!(
        "2^62-byte frame header: {}",
        if closed { "closed" } else { "kept open" }
    ));
    if !closed {
        return fail(name, results.join("; "));
    }

    // 2,000 open WebSockets, then all dropped.
    let rss0 = t.rss_kib();
    let mut socks = Vec::new();
    for _ in 0..2_000 {
        if let Ok((ws, _)) = tokio_tungstenite::connect_async(&url).await {
            socks.push(ws);
        }
    }
    let rss1 = t.rss_kib();
    let n = socks.len();
    drop(socks);
    results.push(format!(
        "{n} concurrent sockets: RSS {} -> {} MiB",
        rss0 / 1024,
        rss1 / 1024
    ));

    // Echo 50,000 messages over one socket.
    use futures_util::{SinkExt, StreamExt};
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let start = Instant::now();
    let mut echoed = 0;
    for i in 0..50_000u32 {
        ws.send(tokio_tungstenite::tungstenite::Message::text(i.to_string()))
            .await
            .unwrap();
        if let Some(Ok(_)) = ws.next().await {
            echoed += 1;
        }
    }
    results.push(format!(
        "{echoed} echoes in {:.1}s",
        start.elapsed().as_secs_f64()
    ));
    drop(ws);
    sleep(Duration::from_secs(1)).await;
    settle(name, &mut t, base, results.join("; ")).await
}

/// Sustained mixed load over keep-alive connections, watching memory.
async fn soak(secs: u64) -> Outcome {
    let name = "soak (mixed load, memory over time)";
    let mut t = Target::start(&[]).await;
    let base = t.fds();
    let addr = t.addr;
    let done = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let stop = Instant::now() + Duration::from_secs(secs);
    let requests: [&[u8]; 6] = [
        b"GET / HTTP/1.1\r\nHost: t\r\n\r\n",
        b"GET /users/42 HTTP/1.1\r\nHost: t\r\n\r\n",
        b"GET /query?a=1&b=2&c=3 HTTP/1.1\r\nHost: t\r\n\r\n",
        b"POST /json HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n{\"name\":\"zap\",\"tags\":[1,2]}",
        b"POST /form HTTP/1.1\r\nHost: t\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 7\r\n\r\na=1&b=2",
        b"GET /nope HTTP/1.1\r\nHost: t\r\n\r\n",
    ];
    let mut workers = Vec::new();
    for w in 0..128 {
        let (done, errors) = (done.clone(), errors.clone());
        workers.push(tokio::spawn(async move {
            let mut lat = Vec::new();
            let mut buf = vec![0u8; 8192];
            'outer: while Instant::now() < stop {
                let Ok(mut s) = TcpStream::connect(addr).await else {
                    errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                // Reconnect every 1,000 requests, so accept and teardown are soaked too.
                for i in 0..1000 {
                    if Instant::now() >= stop {
                        break 'outer;
                    }
                    let req = requests[(w + i) % requests.len()];
                    let t0 = Instant::now();
                    if s.write_all(req).await.is_err() {
                        errors.fetch_add(1, Ordering::Relaxed);
                        continue 'outer;
                    }
                    // Every response here is small and has Content-Length.
                    let mut got = 0;
                    let ok = loop {
                        match timeout(Duration::from_secs(5), s.read(&mut buf[got..])).await {
                            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break false,
                            Ok(Ok(n)) => got += n,
                        }
                        if let Some(end) = buf[..got].windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = std::str::from_utf8(&buf[..end]).unwrap_or("");
                            let cl: usize = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if got >= end + 4 + cl {
                                let st = status(&buf[..got]).unwrap_or(0);
                                break st == 200 || st == 404;
                            }
                        }
                    };
                    if !ok {
                        errors.fetch_add(1, Ordering::Relaxed);
                        continue 'outer;
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                    if i % 64 == 0 {
                        lat.push(t0.elapsed());
                    }
                }
            }
            lat
        }));
    }
    // Sample memory: the first 20% is warm-up; after that RSS must be flat.
    let mut samples = Vec::new();
    let sample_every = Duration::from_secs((secs / 20).max(1));
    while Instant::now() < stop {
        sleep(sample_every).await;
        samples.push(t.rss_kib());
    }
    let mut lat = Vec::new();
    for w in workers {
        lat.extend(w.await.unwrap_or_default());
    }
    lat.sort();
    let p99 = lat.get(lat.len() * 99 / 100).copied().unwrap_or_default();
    let total = done.load(Ordering::Relaxed);
    let errs = errors.load(Ordering::Relaxed);
    let warm = samples.len() / 5;
    let after_warm = samples[warm.min(samples.len() - 1)];
    let max_after = *samples[warm..].iter().max().unwrap_or(&after_warm);
    let last = *samples.last().unwrap();
    let detail = format!(
        "{secs}s, 128 connections: {total} requests ({:.0}/s), {errs} errors, p99 {:?}, RSS after warm-up {} KiB, peak {} KiB, end {} KiB",
        total as f64 / secs as f64,
        p99,
        after_warm,
        max_after,
        last
    );
    if !t.alive() {
        return fail(name, format!("server died. {detail}"));
    }
    if errs > 0 {
        return fail(name, detail);
    }
    // Growth beyond 10% (and 8 MiB) after warm-up looks like a leak.
    if last > after_warm + (after_warm / 10).max(8 * 1024) {
        return fail(name, format!("memory kept growing: {detail}"));
    }
    settle(name, &mut t, base, detail).await
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let soak_secs = args
        .iter()
        .find_map(|a| a.strip_prefix("soak=").and_then(|v| v.parse().ok()))
        .unwrap_or(60);
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with("soak=")).collect();
    let wanted = |key: &str| filters.is_empty() || filters.iter().any(|f| key.contains(f.as_str()));

    let mut outcomes = Vec::new();
    macro_rules! run {
        ($key:expr, $fut:expr) => {
            if wanted($key) {
                let start = Instant::now();
                let o = $fut.await;
                println!(
                    "{} {:<42} {:>5.1}s  {}",
                    if o.pass { "PASS" } else { "FAIL" },
                    o.name,
                    start.elapsed().as_secs_f64(),
                    o.detail
                );
                outcomes.push(o);
            }
        };
    }
    run!("garbage", garbage());
    run!("malformed", malformed(true));
    run!("malformed-noflush", malformed(false));
    run!("slowloris", slowloris());
    run!("idle", idle_keepalive());
    run!("slow-body", slow_body());
    run!("slow-reader", slow_reader());
    run!("disconnects", disconnects());
    run!("oversized", oversized());
    run!("panics", panics(false));
    run!("panics-catch", panics(true));
    run!("fd-exhaustion", fd_exhaustion());
    run!("h2", h2_abuse());
    run!("websocket", websocket());
    run!("soak", soak(soak_secs));

    let failed = outcomes.iter().filter(|o| !o.pass).count();
    println!(
        "\n{} scenarios, {} passed, {} failed",
        outcomes.len(),
        outcomes.len() - failed,
        failed
    );
    if failed > 0 {
        std::process::exit(1);
    }
}
