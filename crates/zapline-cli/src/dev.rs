//! `zapline dev`: rebuild and restart the app whenever source files change.
//!
//! Flow: build once, start the binary, then watch `src/`, `config/`,
//! `migrations/`, `Cargo.toml` and `build.rs`. After a burst of changes settles
//! (debounce), rebuild; if the build succeeds, stop the old process gracefully
//! (SIGTERM, then SIGKILL after a grace period) and start the new one. A failed
//! build leaves the previous version running. The app's output is streamed
//! straight through.
//!
//! `templates/` is deliberately not watched: debug builds re-read templates
//! on every render, so editing one needs a browser refresh, not a restart.
//!
//! A project with `styles/app.css` (Tailwind) also gets `tailwindcss --watch`
//! running alongside, rebuilding `static/app.css` as templates change.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{EventKind, RecursiveMode, Watcher};

use crate::build::{self, BuildOutcome};
use crate::project::Project;
use crate::ui;

/// Directories watched recursively (when they exist).
pub const WATCHED_DIRS: &[&str] = &["src", "config", "migrations"];
/// Files in the project root that trigger a rebuild.
pub const WATCHED_ROOT_FILES: &[&str] = &["Cargo.toml", "build.rs"];
/// Directory names that are never interesting, wherever they appear.
const IGNORED_DIRS: &[&str] = &["target", ".git", "node_modules", ".idea", ".vscode"];

/// How often the main loop wakes up to check for exits and signals.
const TICK: Duration = Duration::from_millis(50);

/// Settings for [`run`].
#[derive(Debug, Clone)]
pub struct DevOptions {
    /// Binary to run (default: the project's only/default binary).
    pub bin: Option<String>,
    /// Build with `--release`.
    pub release: bool,
    /// Arguments passed to the app.
    pub app_args: Vec<String>,
    /// How long the file system must be quiet before rebuilding.
    pub debounce: Duration,
    /// How long the old process gets to exit after SIGTERM before it is killed.
    pub grace: Duration,
    /// Regenerate this TypeScript client after every successful build.
    pub client: Option<PathBuf>,
}

impl Default for DevOptions {
    fn default() -> Self {
        DevOptions {
            bin: None,
            release: false,
            app_args: Vec::new(),
            debounce: Duration::from_millis(300),
            grace: Duration::from_secs(5),
            client: None,
        }
    }
}

// ---- change filtering ----------------------------------------------------------------

/// Editor swap/backup/temporary files that must not trigger rebuilds.
pub fn is_temp_file(name: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "~",
        ".swp",
        ".swo",
        ".swx",
        ".tmp",
        ".bak",
        ".orig",
        ".rej",
        ".zapline-tmp",
    ];
    name == "4913" // vim's write-permission probe
        || name == ".DS_Store"
        || name.starts_with(".#") // emacs lock files
        || (name.len() > 1 && name.starts_with('#') && name.ends_with('#'))
        || name.contains("___jb_") // JetBrains safe-write
        || SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Whether a change to `path` should trigger a rebuild of the project at `root`.
pub fn is_relevant(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let Some((file, dirs)) = parts.split_last() else {
        return false;
    };
    if parts.iter().any(|p| IGNORED_DIRS.contains(p)) || is_temp_file(file) {
        return false;
    }
    match dirs.first() {
        None => WATCHED_ROOT_FILES.contains(file),
        Some(top) => WATCHED_DIRS.contains(top),
    }
}

/// Whether a file system event kind is a content change (not mere access).
pub fn is_change(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

/// Collects change events and decides when a burst has settled.
#[derive(Debug)]
pub struct Debouncer {
    quiet: Duration,
    last_event: Option<Instant>,
    pending: BTreeSet<PathBuf>,
}

impl Debouncer {
    /// A debouncer that fires after `quiet` without new events.
    pub fn new(quiet: Duration) -> Self {
        Debouncer {
            quiet,
            last_event: None,
            pending: BTreeSet::new(),
        }
    }

    /// Records a change at time `now`.
    pub fn push(&mut self, now: Instant, path: PathBuf) {
        self.last_event = Some(now);
        self.pending.insert(path);
    }

    /// Whether any change is waiting.
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether changes are waiting and the quiet period has elapsed.
    pub fn is_ready(&self, now: Instant) -> bool {
        self.is_pending()
            && self
                .last_event
                .is_some_and(|t| now.saturating_duration_since(t) >= self.quiet)
    }

    /// Takes the changed paths, resetting the debouncer.
    pub fn take(&mut self) -> Vec<PathBuf> {
        self.last_event = None;
        std::mem::take(&mut self.pending).into_iter().collect()
    }
}

/// `src/a.rs`, `src/b.rs` and 3 more: a short description of changed files.
pub fn describe_changes(root: &Path, files: &[PathBuf]) -> String {
    let shown: Vec<String> = files
        .iter()
        .take(3)
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    match files.len().saturating_sub(shown.len()) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

// ---- process control ---------------------------------------------------------------

/// How a child process ended up stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Stopped {
    /// It had already exited.
    AlreadyExited,
    /// It exited on its own after SIGTERM.
    Graceful,
    /// It ignored SIGTERM for the whole grace period and was killed.
    Killed,
}

#[cfg(unix)]
fn send_terminate(child: &mut Child) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
}

/// No SIGTERM outside unix: the process is killed right away.
#[cfg(not(unix))]
fn send_terminate(child: &mut Child) {
    let _ = child.kill();
}

/// Asks `child` to exit (SIGTERM on unix) and kills it if it is still running
/// after `grace`.
pub fn stop(child: &mut Child, grace: Duration) -> Stopped {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return Stopped::AlreadyExited;
    }
    send_terminate(child);
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Stopped::Graceful,
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    Stopped::Killed
}

/// Sets up SIGINT/SIGTERM handling: the first signal requests a graceful
/// shutdown, a second one terminates immediately.
fn install_signal_handlers() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        use signal_hook::flag;
        for sig in [SIGINT, SIGTERM, SIGHUP] {
            // Order matters: the conditional default runs first and only fires
            // once the flag is already set (second signal).
            flag::register_conditional_default(sig, flag.clone())
                .context("cannot install signal handler")?;
            flag::register(sig, flag.clone()).context("cannot install signal handler")?;
        }
    }
    Ok(flag)
}

// ---- the dev loop ----------------------------------------------------------------------

struct Dev<'a> {
    project: &'a Project,
    opts: &'a DevOptions,
    bin: String,
    app: Option<Child>,
    debounce: Debouncer,
    shutdown: Arc<AtomicBool>,
    /// `tailwindcss --watch`, which exits when its stdin closes.
    css: Option<(Child, ChildStdin)>,
}

impl Dev<'_> {
    fn stopping(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Builds; on success replaces the running app with the new binary.
    fn rebuild_and_restart(&mut self, rx: &mpsc::Receiver<notify::Result<notify::Event>>) {
        ui::status("Building", &self.bin);
        let outcome = match build::build(self.project, &self.bin, self.opts.release) {
            Ok(o) => o,
            Err(e) => {
                ui::status_error("Failed", format!("{e:#}"));
                return;
            }
        };
        if self.stopping() {
            return;
        }
        match outcome {
            BuildOutcome::Failed => {
                let keeps = if self.app.is_some() {
                    "; the previous version keeps running"
                } else {
                    ""
                };
                ui::status_error(
                    "Failed",
                    format!("build failed, fix the errors and save to retry{keeps}"),
                );
            }
            BuildOutcome::Built(exe) => {
                // Files may have changed while compiling: rebuild instead of
                // restarting into an already stale binary.
                self.collect_events(rx, Duration::ZERO);
                if self.debounce.is_pending() {
                    ui::status_info("Changed", "files changed during the build, rebuilding");
                    return;
                }
                if let Some(target) = &self.opts.client {
                    let shown = target.strip_prefix(&self.project.root).unwrap_or(target);
                    match crate::client::regenerate(self.project, &exe, target) {
                        Ok(true) => ui::status("Client", format!("updated {}", shown.display())),
                        Ok(false) => {}
                        Err(e) => ui::status_warn("Client", format!("not updated: {e:#}")),
                    }
                }
                self.restart(&exe);
            }
        }
    }

    fn restart(&mut self, exe: &Path) {
        if let Some(mut old) = self.app.take() {
            ui::status_info("Stopping", format!("previous run (pid {})", old.id()));
            if stop(&mut old, self.opts.grace) == Stopped::Killed {
                ui::status_warn("Killed", "the app did not exit after SIGTERM");
            }
        }
        let shown = exe.strip_prefix(&self.project.root).unwrap_or(exe);
        let mut cmd = Command::new(exe);
        cmd.args(&self.opts.app_args)
            .current_dir(&self.project.root);
        // Dev mode: 500 responses carry the real error instead of a generic
        // message, so you (or your AI assistant) see what broke.
        if std::env::var_os("ZAPLINE_DEV").is_none() {
            cmd.env("ZAPLINE_DEV", "1");
        }
        // Where the app records its URL, so `zapline mcp` can talk to it.
        let addr_file = self.project.root.join(crate::live::ADDR_FILE);
        if let Some(dir) = addr_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::remove_file(&addr_file);
        cmd.env(crate::live::ENV_ADDR_FILE, &addr_file);
        match cmd.spawn() {
            Ok(child) => {
                ui::status(
                    "Running",
                    format!("`{}` (pid {})", shown.display(), child.id()),
                );
                self.app = Some(child);
            }
            Err(e) => ui::status_error("Failed", format!("cannot start {}: {e}", shown.display())),
        }
    }

    /// Notices when the app exits on its own.
    fn reap_exited(&mut self) {
        let Some(child) = self.app.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.app = None;
                report_exit(status);
            }
            Ok(None) => {}
            Err(e) => {
                self.app = None;
                ui::status_error("Failed", format!("cannot wait for the app: {e}"));
            }
        }
    }

    /// Reads file system events for up to `wait`, feeding the debouncer.
    fn collect_events(
        &mut self,
        rx: &mpsc::Receiver<notify::Result<notify::Event>>,
        wait: Duration,
    ) {
        let mut next = rx.recv_timeout(wait);
        loop {
            match next {
                Ok(Ok(event)) => {
                    if is_change(&event.kind) {
                        let now = Instant::now();
                        for path in event.paths {
                            if is_relevant(&self.project.root, &path) {
                                self.debounce.push(now, path);
                            }
                        }
                    }
                }
                Ok(Err(e)) => ui::warn(format!("file watcher: {e}")),
                Err(_) => return,
            }
            next = rx.try_recv().map_err(|_| RecvTimeoutError::Timeout);
        }
    }

    fn finish(&mut self) {
        if let Some(mut child) = self.app.take() {
            ui::status_info("Stopping", format!("the app (pid {})", child.id()));
            stop(&mut child, self.opts.grace);
        }
        if let Some((mut child, stdin)) = self.css.take() {
            drop(stdin);
            stop(&mut child, Duration::from_secs(1));
        }
        ui::status("Stopped", "dev server");
    }
}

fn report_exit(status: ExitStatus) {
    if status.success() {
        ui::status_warn("Exited", "the app exited; waiting for changes");
    } else {
        ui::status_error(
            "Crashed",
            format!("the app exited with {status}; waiting for changes"),
        );
    }
}

/// Runs the dev loop until interrupted.
pub fn run(project: &Project, opts: &DevOptions) -> Result<()> {
    let bin = build::resolve_bin(project, opts.bin.as_deref())?;
    let shutdown = install_signal_handlers()?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("cannot start the file watcher")?;

    let mut watched = Vec::new();
    for dir in WATCHED_DIRS {
        let path = project.root.join(dir);
        if path.is_dir() {
            watcher
                .watch(&path, RecursiveMode::Recursive)
                .with_context(|| format!("cannot watch {}", path.display()))?;
            watched.push(format!("{dir}/"));
        }
    }
    // Root-level files (Cargo.toml, build.rs) and directories created later.
    watcher
        .watch(&project.root, RecursiveMode::NonRecursive)
        .with_context(|| format!("cannot watch {}", project.root.display()))?;
    watched.extend(WATCHED_ROOT_FILES.iter().map(|f| f.to_string()));
    ui::status_info("Watching", watched.join(", "));

    let mut dev = Dev {
        project,
        opts,
        bin,
        app: None,
        debounce: Debouncer::new(opts.debounce),
        shutdown,
        css: None,
    };
    if crate::tailwind::is_used(&project.root) {
        match crate::tailwind::watch(&project.root) {
            Ok(css) => {
                ui::status_info(
                    "Watching",
                    format!("{} with Tailwind", crate::tailwind::INPUT),
                );
                dev.css = Some(css);
            }
            Err(e) => ui::status_warn("Tailwind", format!("not running: {e:#}")),
        }
    }
    dev.rebuild_and_restart(&rx);

    while !dev.stopping() {
        dev.collect_events(&rx, TICK);
        // Ctrl-C reaches the app too; do not report that as a crash.
        if dev.stopping() {
            break;
        }
        dev.reap_exited();
        if dev.debounce.is_ready(Instant::now()) {
            let files = dev.debounce.take();
            ui::status_info("Changed", describe_changes(&project.root, &files));
            dev.rebuild_and_restart(&rx);
        }
    }
    dev.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

    fn root() -> PathBuf {
        PathBuf::from("/work/app")
    }

    fn relevant(rel: &str) -> bool {
        is_relevant(&root(), &root().join(rel))
    }

    #[test]
    fn source_config_and_manifest_changes_are_relevant() {
        for p in [
            "src/main.rs",
            "src/routes/users.rs",
            "config/default.toml",
            "migrations/20260101000000_create_todos.sql",
            "Cargo.toml",
            "build.rs",
        ] {
            assert!(relevant(p), "{p}");
        }
    }

    #[test]
    fn everything_else_is_ignored() {
        for p in [
            "target/debug/app",
            "src/target/x.rs",
            ".git/index",
            "src/.git/HEAD",
            "README.md",
            "templates/index.html",
            "Cargo.lock",
            "tests/api.rs",
            "node_modules/x/y.js",
            "docs/src/a.rs",
            "src/main.rs.swp",
            "src/.main.rs.swp",
            "src/main.rs~",
            "src/.#main.rs",
            "src/#main.rs#",
            "src/4913",
            "config/default.toml.tmp",
            "src/main.rs___jb_tmp___",
            "src/main.rs___jb_old___",
            "src/.DS_Store",
        ] {
            assert!(!relevant(p), "{p}");
        }
        // The project root itself and paths outside it.
        assert!(!is_relevant(&root(), &root()));
        assert!(!is_relevant(&root(), Path::new("/elsewhere/src/main.rs")));
    }

    #[test]
    fn only_content_changes_count() {
        assert!(is_change(&EventKind::Create(CreateKind::File)));
        assert!(is_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_change(&EventKind::Remove(RemoveKind::File)));
        assert!(!is_change(&EventKind::Access(AccessKind::Any)));
        assert!(!is_change(&EventKind::Other));
    }

    #[test]
    fn debouncer_waits_for_quiet() {
        let mut d = Debouncer::new(Duration::from_millis(300));
        let t0 = Instant::now();
        assert!(!d.is_ready(t0));
        d.push(t0, "/a".into());
        assert!(d.is_pending());
        assert!(!d.is_ready(t0 + Duration::from_millis(100)));
        // Another event restarts the quiet period.
        d.push(t0 + Duration::from_millis(250), "/b".into());
        assert!(!d.is_ready(t0 + Duration::from_millis(400)));
        assert!(d.is_ready(t0 + Duration::from_millis(550)));
        assert_eq!(d.take(), [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert!(!d.is_pending());
        assert!(!d.is_ready(t0 + Duration::from_secs(10)));
    }

    #[test]
    fn debouncer_deduplicates_paths() {
        let mut d = Debouncer::new(Duration::from_millis(10));
        let t = Instant::now();
        for _ in 0..5 {
            d.push(t, "/a".into());
        }
        assert_eq!(d.take().len(), 1);
    }

    #[test]
    fn describes_changed_files() {
        let r = root();
        let files: Vec<PathBuf> = ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs"]
            .iter()
            .map(|f| r.join(f))
            .collect();
        assert_eq!(describe_changes(&r, &files[..1]), "src/a.rs");
        assert_eq!(
            describe_changes(&r, &files[..3]),
            "src/a.rs, src/b.rs, src/c.rs"
        );
        assert_eq!(
            describe_changes(&r, &files),
            "src/a.rs, src/b.rs, src/c.rs and 2 more"
        );
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::process::Stdio;

        fn sh(script: &str) -> Child {
            Command::new("sh")
                .args(["-c", script])
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        }

        #[test]
        fn stop_terminates_politely() {
            let mut child = sh("exec sleep 30");
            let started = Instant::now();
            assert_eq!(stop(&mut child, Duration::from_secs(5)), Stopped::Graceful);
            assert!(started.elapsed() < Duration::from_secs(4));
        }

        #[test]
        fn stop_reports_already_exited() {
            let mut child = sh("exit 0");
            child.wait().unwrap();
            assert_eq!(
                stop(&mut child, Duration::from_secs(1)),
                Stopped::AlreadyExited
            );
        }

        #[test]
        fn stop_kills_processes_that_ignore_sigterm() {
            // The shell ignores SIGTERM and keeps looping; `exec`-less on purpose.
            let mut child = sh("trap '' TERM; while true; do sleep 0.05; done");
            // Give the shell a moment to install its trap.
            std::thread::sleep(Duration::from_millis(300));
            let started = Instant::now();
            assert_eq!(
                stop(&mut child, Duration::from_millis(300)),
                Stopped::Killed
            );
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(child.try_wait().unwrap().is_some());
        }
    }
}
