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
