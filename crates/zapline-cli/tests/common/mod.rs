//! Shared helpers for the integration tests.
#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// Path of the compiled `zapline` binary.
pub fn zapline_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zapline")
}

/// A `zapline` command that never emits colour codes.
pub fn zapline() -> Command {
    let mut cmd = Command::new(zapline_bin());
    cmd.env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    // Each test project lives in its own temp dir; build it there. An
    // inherited shared target dir would let parallel tests that build
    // same-named crates overwrite each other's binaries.
    cmd.env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR");
    cmd
}

/// Runs `zapline args...` in `dir` and returns the output.
pub fn run_in(dir: &Path, args: &[&str]) -> Output {
    zapline()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run zapline")
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Asserts success and returns stderr (where status lines go).
pub fn expect_success(out: &Output) -> String {
    assert!(
        out.status.success(),
        "command failed with {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        stdout(out),
        stderr(out)
    );
    stderr(out)
}

/// Asserts failure and returns stderr.
pub fn expect_failure(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        stdout(out),
        stderr(out)
    );
    stderr(out)
}

/// Creates a tiny cargo project with no dependencies whose binary speaks the
/// zapline env-var protocols, so the CLI can be tested without compiling zapline.
pub fn fake_app(dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    write_fake_main(&root, "v1");
    root
}

/// Writes `src/main.rs` of a fake app; `version` shows up in its output.
pub fn write_fake_main(root: &Path, version: &str) {
    const MAIN: &str = r##"fn main() {
    if std::env::var_os("ZAPLINE_PRINT_ROUTES").is_some() {
        println!("starting up");
        println!("{}", r#"[{"method":"GET","path":"/health"},{"method":"POST","path":"/users/{id}"}]"#);
        return;
    }
    if let Some(path) = std::env::var_os("ZAPLINE_EXPORT_OPENAPI") {
        let spec = r#"{"openapi":"3.1.0","info":{"title":"fake","version":"1.2.3"},"paths":{"/x":{"get":{},"post":{}}}}"#;
        std::fs::write(path, spec).unwrap();
        return;
    }
    // Like a debug build of a zapline app run by `zapline dev`.
    if let Some(file) = std::env::var_os("ZAPLINE_DEV_ADDR_FILE") {
        std::fs::write(file, "http://127.0.0.1:1\n").unwrap();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("fake-app __VERSION__ running args={args:?}");
    std::thread::sleep(std::time::Duration::from_secs(120));
}
"##;
    fs::write(
        root.join("src/main.rs"),
        MAIN.replace("__VERSION__", version),
    )
    .unwrap();
}

/// A child process whose stdout and stderr lines can be awaited.
pub struct Watched {
    pub child: Child,
    lines: Receiver<String>,
    pub seen: Vec<String>,
}

impl Watched {
    pub fn spawn(mut cmd: Command) -> Watched {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("failed to spawn");
        let (tx, rx) = mpsc::channel();
        for stream in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        Watched {
            child,
            lines: rx,
            seen: Vec::new(),
        }
    }

    /// Waits until a line containing `needle` shows up (also matching lines
    /// already seen after `from`). Returns the index of the matching line.
    pub fn wait_for(&mut self, needle: &str, timeout: Duration) -> usize {
        self.wait_for_after(0, needle, timeout)
    }

    pub fn wait_for_after(&mut self, from: usize, needle: &str, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(i) = self.seen[from.min(self.seen.len())..]
                .iter()
                .position(|l| l.contains(needle))
            {
                return from + i;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                panic!(
                    "timed out waiting for {needle:?}; output so far:\n{}",
                    self.seen.join("\n")
                );
            }
            if let Ok(line) = self
                .lines
                .recv_timeout(left.min(Duration::from_millis(200)))
            {
                self.seen.push(line);
            }
        }
    }

    /// Asserts that no line containing `needle` appears within `window`.
    pub fn assert_absent_for(&mut self, needle: &str, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            if let Ok(line) = self.lines.recv_timeout(Duration::from_millis(100)) {
                self.seen.push(line);
            }
        }
        assert!(
            !self.seen.iter().any(|l| l.contains(needle)),
            "unexpected {needle:?} in output:\n{}",
            self.seen.join("\n")
        );
    }

    /// Waits for the process to exit.
    pub fn wait_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Drain remaining output.
                while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(100)) {
                    self.seen.push(line);
                }
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("process did not exit; output:\n{}", self.seen.join("\n"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Sends `signal` (e.g. "INT", "TERM") to `pid` with the `kill` utility.
#[cfg(unix)]
pub fn signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("kill is available");
    assert!(status.success(), "kill -{signal} {pid} failed");
}

/// Whether a process with this pid exists.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Extracts `pid N` from a status line like `Running `target/debug/x` (pid 123)`.
pub fn pid_from_line(line: &str) -> Option<u32> {
    let rest = line.split("(pid ").nth(1)?;
    rest.split(')').next()?.trim().parse().ok()
}

/// Writes `src/main.rs` of a fake app that speaks the `zapline dev` proxy
/// handshake like `App::run`: with `ZAPLINE_DEV_PROXY` set it listens on a
/// free loopback port and reports it; otherwise it binds `$FAKE_ADDR`. Every
/// request gets a page naming `version`.
pub fn write_live_main(root: &Path, version: &str) {
    const MAIN: &str = r##"use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

fn main() {
    let addr = std::env::var("FAKE_ADDR").expect("FAKE_ADDR");
    let listener = match std::env::var_os("ZAPLINE_DEV_PROXY") {
        Some(path) => {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let json = format!(
                r#"{{"addr":"{addr}","internal":"{}","pid":{}}}"#,
                l.local_addr().unwrap(),
                std::process::id()
            );
            let tmp = format!("{}.tmp", path.to_str().unwrap());
            std::fs::write(&tmp, json).unwrap();
            std::fs::rename(&tmp, &path).unwrap();
            l
        }
        None => TcpListener::bind(&addr).unwrap(),
    };
    println!("fake-live __VERSION__ listening");
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
        }
        let body = "<html><head><title>t</title></head><body>__VERSION__</body></html>";
        let _ = write!(
            conn,
            "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    }
}
"##;
    fs::write(
        root.join("src/main.rs"),
        MAIN.replace("__VERSION__", version),
    )
    .unwrap();
}

/// A free TCP port on 127.0.0.1 (free when this returns).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `GET path` with `Connection: close`: the status code and body.
pub fn http_get(port: u16, path: &str) -> std::io::Result<(u16, String)> {
    use std::io::Write;
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port))?;
    conn.set_read_timeout(Some(Duration::from_secs(20)))?;
    write!(
        conn,
        "GET {path} HTTP/1.1\r\nhost: localhost:{port}\r\naccept: text/html\r\nconnection: close\r\n\r\n"
    )?;
    let mut raw = String::new();
    conn.read_to_string(&mut raw)?;
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    Ok((status, body))
}

/// Server-sent events from `zapline dev`'s `/__zapline/events`, as
/// `(event, data)` pairs read on a background thread.
pub fn dev_events(port: u16) -> Receiver<(String, String)> {
    use std::io::Write;
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        conn,
        "GET /__zapline/events HTTP/1.1\r\nhost: localhost\r\naccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut event = String::new();
        for line in BufReader::new(conn).lines().map_while(Result::ok) {
            if let Some(name) = line.strip_prefix("event: ") {
                event = name.to_owned();
            } else if let Some(data) = line.strip_prefix("data: ") {
                if tx.send((event.clone(), data.to_owned())).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

/// Waits for event `name` and returns its data.
pub fn wait_event(events: &Receiver<(String, String)>, name: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok((event, data)) if event == name => return data,
            Ok(other) => seen.push(other),
            Err(_) => panic!("no {name:?} event; saw {seen:?}"),
        }
    }
}
