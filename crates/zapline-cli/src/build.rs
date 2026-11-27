//! Running cargo: building the app, locating its binary, and running it with a timeout.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::project::Project;
use crate::ui;

/// The `cargo` to invoke: `$CARGO` (set when running as `cargo zapline`) or `cargo`.
pub fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
}

/// Picks the binary target to operate on: the requested one, or the project's
/// only/default one.
pub fn resolve_bin(project: &Project, requested: Option<&str>) -> Result<String> {
    match requested {
        Some(bin) => {
            project.check_bin(bin)?;
            Ok(bin.to_string())
        }
        None => project.default_bin(),
    }
}

/// Result of `cargo build`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildOutcome {
    /// Built; the executable is at this path.
    Built(PathBuf),
    /// Cargo reported errors (already printed to stderr).
    Failed,
}

/// Builds binary `bin` of `project`, streaming cargo's own output (compiler
/// diagnostics included) to stderr, and returns the path of the executable.
pub fn build(project: &Project, bin: &str, release: bool) -> Result<BuildOutcome> {
    let mut cmd = cargo();
    cmd.args([
        "build",
        "--message-format=json-render-diagnostics",
        "--bin",
        bin,
    ]);
    if release {
        cmd.arg("--release");
    }
    if ui::stderr_color() {
        cmd.args(["--color", "always"]);
    }
    cmd.current_dir(&project.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd
        .spawn()
        .context("failed to run `cargo build` (is cargo installed?)")?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no cargo stdout"))?;
    let mut executable = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("cannot read cargo output")?;
        if let Some(path) = executable_from_message(&line, bin) {
            executable = Some(path);
        }
    }
    let status = child.wait().context("failed to wait for cargo")?;
    if !status.success() {
        return Ok(BuildOutcome::Failed);
    }
    match executable {
        Some(path) => Ok(BuildOutcome::Built(path)),
        None => bail!("cargo succeeded but did not report an executable for `{bin}`"),
    }
}

/// Extracts the executable path from one line of `--message-format=json` output
/// if it is the compiler artifact of binary `bin`.
pub fn executable_from_message(line: &str, bin: &str) -> Option<PathBuf> {
    let msg: serde_json::Value = serde_json::from_str(line).ok()?;
    if msg.get("reason")?.as_str()? != "compiler-artifact" {
        return None;
    }
    let target = msg.get("target")?;
    if target.get("name")?.as_str()? != bin {
        return None;
    }
    let is_bin = target
        .get("kind")?
        .as_array()?
        .iter()
        .any(|k| k.as_str() == Some("bin"));
    if !is_bin {
        return None;
    }
    msg.get("executable")?.as_str().map(PathBuf::from)
}

/// Whether the app's stderr is passed through to ours (the default) or only
/// kept for error messages (`zapline mcp`, whose clients don't show stderr).
static ECHO_STDERR: AtomicBool = AtomicBool::new(true);

/// Pass the app's stderr through (`true`, the default) or only keep its tail
/// for error messages.
pub fn set_echo_stderr(echo: bool) {
    ECHO_STDERR.store(echo, Ordering::Relaxed);
}

/// Lines of the app's stderr kept for error messages.
const STDERR_TAIL_LINES: usize = 20;

/// Captured output of a finished process.
#[derive(Debug)]
pub struct Captured {
    /// Exit status.
    pub status: ExitStatus,
    /// Everything written to stdout.
    pub stdout: String,
    /// The last lines written to stderr.
    pub stderr_tail: String,
}

impl Captured {
    /// An error saying the app failed to `what`, with the end of its stderr
    /// when that was not already shown.
    pub fn failure(&self, what: &str) -> anyhow::Error {
        let mut msg = format!("the app exited with {} {what}", self.status);
        if !ECHO_STDERR.load(Ordering::Relaxed) && !self.stderr_tail.trim().is_empty() {
            msg.push_str("\napp stderr:\n");
            msg.push_str(self.stderr_tail.trim_end());
        }
        anyhow!(msg)
    }
}

/// Runs `cmd` with stdout captured and stderr passed through, killing it after
/// `timeout`. Used for the env-var protocols (`routes`, `openapi`), where an app
/// that ignores the variable would otherwise start serving and hang forever.
pub fn run_captured(mut cmd: Command, timeout: Duration) -> Result<Captured> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("failed to start the app")?;
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

    // Echo stderr as it comes (or not) and keep its last lines.
    let tail = Arc::new(Mutex::new(std::collections::VecDeque::<String>::new()));
    let tail_writer = Arc::clone(&tail);
    let echo = ECHO_STDERR.load(Ordering::Relaxed);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if echo {
                eprintln!("{line}");
            }
            if let Ok(mut t) = tail_writer.lock() {
                if t.len() == STDERR_TAIL_LINES {
                    t.pop_front();
                }
                t.push_back(line);
            }
        }
    });

    // Stream chunks to the main thread so that a grandchild holding the pipe
    // open (or a killed process tree) can never block us.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("failed to wait for the app")? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_and_reap(&mut child);
            bail!(
                "the app did not exit within {}s\n\
                 hint: `App::run` exits right after printing routes or exporting the spec; \
                 make sure `main` reaches it before starting anything long-running",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // The process is gone; collect what it wrote (stop early if something
    // else keeps the pipe open).
    let mut bytes = Vec::new();
    let drain_until = Instant::now() + Duration::from_secs(2);
    loop {
        let left = drain_until.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(chunk) => bytes.extend(chunk),
            Err(_) => break,
        }
    }
    let stderr_tail = tail
        .lock()
        .map(|t| t.iter().map(|l| format!("{l}\n")).collect())
        .unwrap_or_default();
    Ok(Captured {
        status,
        stdout: String::from_utf8_lossy(&bytes).into_owned(),
        stderr_tail,
    })
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs the app through `cargo run --quiet [--bin X]` with `envs` set, after
/// making sure it is built (so the timeout only covers running, not compiling).
pub fn run_app(
    project: &Project,
    bin: Option<&str>,
    envs: &[(&str, &OsStr)],
    timeout: Duration,
) -> Result<Captured> {
    let bin = resolve_bin(project, bin)?;
    match build(project, &bin, false)? {
        BuildOutcome::Built(_) => {}
        BuildOutcome::Failed => bail!("build failed"),
    }
    let mut cmd = cargo();
    cmd.args(["run", "--quiet", "--bin", &bin])
        .current_dir(&project.root);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    run_captured(cmd, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_executable_of_the_requested_binary() {
        let line = r#"{"reason":"compiler-artifact","package_id":"x","target":{"kind":["bin"],"crate_types":["bin"],"name":"my-app"},"profile":{},"features":[],"filenames":["/t/debug/my-app"],"executable":"/t/debug/my-app","fresh":false}"#;
        assert_eq!(
            executable_from_message(line, "my-app"),
            Some(PathBuf::from("/t/debug/my-app"))
        );
        assert_eq!(executable_from_message(line, "other"), None);
    }

    #[test]
    fn ignores_libraries_and_other_messages() {
        let lib = r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"my-app"},"executable":null}"#;
        assert_eq!(executable_from_message(lib, "my-app"), None);
        let other = r#"{"reason":"build-finished","success":true}"#;
        assert_eq!(executable_from_message(other, "my-app"), None);
        assert_eq!(executable_from_message("not json", "my-app"), None);
        assert_eq!(executable_from_message("", "my-app"), None);
    }

    #[cfg(unix)]
    #[test]
    fn run_captured_returns_stdout_and_status() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo one; echo two; exit 3"]);
        let out = run_captured(cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, "one\ntwo\n");
    }

    #[cfg(unix)]
    #[test]
    fn run_captured_kills_runaway_processes() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let err = run_captured(cmd, Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not exit"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn run_captured_handles_large_output_without_deadlock() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 1000000 /dev/zero | tr '\\0' 'x'"]);
        let out = run_captured(cmd, Duration::from_secs(20)).unwrap();
        assert_eq!(out.stdout.len(), 1_000_000);
    }
}
