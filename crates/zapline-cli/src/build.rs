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
    /// Cargo reported errors (already printed to stderr). These are the
    /// compiler errors, empty when cargo failed before compiling (a broken
    /// `Cargo.toml`, a missing dependency).
    Failed(Vec<Diagnostic>),
}

/// One compiler error, from cargo's `--message-format=json` output.
///
/// ```
/// let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"cannot find value `x`","spans":[{"file_name":"src/main.rs","line_start":3,"column_start":5,"is_primary":true}],"rendered":"error: cannot find value `x`"}}"#;
/// let d = zapline_cli::build::diagnostic_from_message(line).unwrap();
/// assert_eq!((d.file.as_deref(), d.line, d.column), (Some("src/main.rs"), 3, 5));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// `error` (or `error: internal compiler error`).
    pub level: String,
    /// The one-line message, e.g. ``cannot find value `x` in this scope``.
    pub message: String,
    /// The file of the primary span, relative to the workspace root for
    /// workspace members.
    pub file: Option<String>,
    /// 1-based line of the primary span (0 when there is none).
    pub line: u32,
    /// 1-based column of the primary span (0 when there is none).
    pub column: u32,
    /// The full rustc rendering (source snippet, notes, help), as printed to
    /// the terminal; may contain ANSI colour codes.
    pub rendered: String,
}

/// The compiler *error* in one line of cargo's JSON output, if it is one
/// (warnings and other messages are `None`).
pub fn diagnostic_from_message(line: &str) -> Option<Diagnostic> {
    let msg: serde_json::Value = serde_json::from_str(line).ok()?;
    if msg.get("reason")?.as_str()? != "compiler-message" {
        return None;
    }
    let message = msg.get("message")?;
    let level = message.get("level")?.as_str()?;
    if !level.starts_with("error") {
        return None;
    }
    let spans = message.get("spans").and_then(|s| s.as_array());
    let primary = spans.and_then(|spans| {
        spans
            .iter()
            .find(|s| s.get("is_primary").and_then(|p| p.as_bool()) == Some(true))
            .or_else(|| spans.first())
    });
    let number = |key: &str| {
        primary
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    Some(Diagnostic {
        level: level.to_owned(),
        message: message.get("message")?.as_str()?.to_owned(),
        file: primary
            .and_then(|s| s.get("file_name"))
            .and_then(|f| f.as_str())
            .map(str::to_owned),
        line: number("line_start"),
        column: number("column_start"),
        rendered: message
            .get("rendered")
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .to_owned(),
    })
}

/// The rendered text of a compiler message (error or warning), to print.
fn rendered_message(line: &str) -> Option<String> {
    let msg: serde_json::Value = serde_json::from_str(line).ok()?;
    if msg.get("reason")?.as_str()? != "compiler-message" {
        return None;
    }
    msg.get("message")?
        .get("rendered")?
        .as_str()
        .map(str::to_owned)
}

/// Builds binary `bin` of `project`, streaming cargo's own output (compiler
/// diagnostics included) to stderr, and returns the path of the executable.
pub fn build(project: &Project, bin: &str, release: bool) -> Result<BuildOutcome> {
    build_with(project, bin, release, |_| {})
}

/// [`build`], calling `on_spawn` with cargo's process id once it runs (so
/// another thread can stop a build that is no longer wanted).
pub fn build_with(
    project: &Project,
    bin: &str,
    release: bool,
    on_spawn: impl FnOnce(u32),
) -> Result<BuildOutcome> {
    let color = ui::stderr_color();
    let mut cmd = cargo();
    cmd.args([
        "build",
        // Diagnostics as JSON, so `zapline dev` can show them in the
        // browser; we print their rendering ourselves.
        if color {
            "--message-format=json-diagnostic-rendered-ansi"
        } else {
            "--message-format=json"
        },
        "--bin",
        bin,
    ]);
    if release {
        cmd.arg("--release");
    }
    if color {
        cmd.args(["--color", "always"]);
    }
    cmd.current_dir(&project.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd
        .spawn()
        .context("failed to run `cargo build` (is cargo installed?)")?;
    on_spawn(child.id());

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no cargo stdout"))?;
    let mut executable = None;
    let mut errors = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.context("cannot read cargo output")?;
        if let Some(path) = executable_from_message(&line, bin) {
            executable = Some(path);
        } else if let Some(text) = rendered_message(&line) {
            eprint!("{text}");
            if let Some(d) = diagnostic_from_message(&line) {
                errors.push(d);
            }
        }
    }
    let status = child.wait().context("failed to wait for cargo")?;
    if !status.success() {
        return Ok(BuildOutcome::Failed(errors));
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
    run_app_in(project, bin, envs, timeout, false)
}

/// [`run_app`] with a choice of profile: `release` builds and runs
/// `cargo run --release`.
pub fn run_app_in(
    project: &Project,
    bin: Option<&str>,
    envs: &[(&str, &OsStr)],
    timeout: Duration,
    release: bool,
) -> Result<Captured> {
    let bin = resolve_bin(project, bin)?;
    match build(project, &bin, release)? {
        BuildOutcome::Built(_) => {}
        BuildOutcome::Failed(_) => bail!("build failed"),
    }
    let mut cmd = cargo();
    cmd.args(["run", "--quiet", "--bin", &bin])
        .current_dir(&project.root);
    if release {
        cmd.arg("--release");
    }
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

    #[test]
    fn parses_compiler_errors_with_their_primary_span() {
        let line = r#"{"reason":"compiler-message","package_id":"x","manifest_path":"/w/Cargo.toml","target":{"kind":["bin"],"name":"app"},"message":{"rendered":"error[E0425]: cannot find value `nope` in this scope\n --> src/main.rs:4:13\n","$message_type":"diagnostic","children":[],"code":{"code":"E0425","explanation":null},"level":"error","message":"cannot find value `nope` in this scope","spans":[{"byte_end":60,"byte_start":56,"column_end":17,"column_start":13,"expansion":null,"file_name":"src/lib.rs","is_primary":false,"label":null,"line_end":2,"line_start":2,"suggested_replacement":null,"suggestion_applicability":null,"text":[]},{"byte_end":60,"byte_start":56,"column_end":17,"column_start":13,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":"not found in this scope","line_end":4,"line_start":4,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}"#;
        let d = diagnostic_from_message(line).unwrap();
        assert_eq!(d.level, "error");
        assert_eq!(d.message, "cannot find value `nope` in this scope");
        assert_eq!(d.file.as_deref(), Some("src/main.rs"));
        assert_eq!((d.line, d.column), (4, 13));
        assert!(d.rendered.starts_with("error[E0425]"), "{}", d.rendered);
        assert!(rendered_message(line).is_some());
    }

    #[test]
    fn errors_without_spans_and_non_errors() {
        let summary = r#"{"reason":"compiler-message","message":{"rendered":"error: aborting due to 1 previous error\n","level":"error","message":"aborting due to 1 previous error","spans":[]}}"#;
        let d = diagnostic_from_message(summary).unwrap();
        assert_eq!((d.file, d.line, d.column), (None, 0, 0));
        let ice = r#"{"reason":"compiler-message","message":{"rendered":"","level":"error: internal compiler error","message":"boom","spans":[]}}"#;
        assert_eq!(diagnostic_from_message(ice).unwrap().message, "boom");

        let warning = r#"{"reason":"compiler-message","message":{"rendered":"warning: unused variable: `x`\n","level":"warning","message":"unused variable: `x`","spans":[]}}"#;
        assert_eq!(diagnostic_from_message(warning), None);
        // Warnings are still printed.
        assert_eq!(
            rendered_message(warning).as_deref(),
            Some("warning: unused variable: `x`\n")
        );
        let artifact = r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"x"},"executable":"/t/x"}"#;
        assert_eq!(diagnostic_from_message(artifact), None);
        assert_eq!(rendered_message(artifact), None);
        assert_eq!(diagnostic_from_message("garbage"), None);
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
