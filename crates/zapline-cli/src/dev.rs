//! `zapline dev`: a Vite-style dev loop for a Rust web app.
//!
//! - **Rust changes** (`src/`, `config/`, `migrations/`, `Cargo.toml`,
//!   `build.rs`, `.env`) rebuild in the background while the previous
//!   version keeps serving. The new binary starts next to the old one, and
//!   requests switch over once it answers HTTP; only then is the old process
//!   stopped (SIGTERM, then SIGKILL after a grace period). A failed build
//!   leaves the previous version running.
//! - **Asset changes** (`templates/`, `static/`, `public/`, `assets/`,
//!   `locales/`, `i18n/`) reload open browsers without restarting anything:
//!   templates and translations are read again by the running app, and a
//!   CSS change swaps the stylesheets in place, without a page reload.
//! - **Compile errors** show as an overlay in the browser (file, line,
//!   rustc's message), cleared by the next good build.
//!
//! The browser side works through a small proxy ([`proxy`]) on the app's
//! own address: the app listens on a free loopback port instead (zapline's
//! `App::run` does this when `ZAPLINE_DEV_PROXY` is set, see
//! `zapline::dev::ENV_DEV_PROXY`), and the proxy forwards every request,
//! adds one `<script>` tag to HTML pages ([`inject`]) and pushes events over
//! server-sent events at `/__zapline/events`. Nothing of this exists in a
//! production build of the app.
//!
//! An app that cannot be proxied (a binary that does not mention
//! `ZAPLINE_DEV_PROXY`, TLS, a Unix socket, `--no-proxy`) gets the plain
//! loop: rebuild, stop the old process, start the new one.
//!
//! A project with `styles/app.css` (Tailwind) also gets `tailwindcss --watch`
//! running alongside, rebuilding `static/app.css` as templates change, which
//! the browsers then pick up as a CSS hot swap.
//!
//! Typed API clients (`--client`, `[client]` in `zapline.toml`, or
//! `frontend/src/api.ts` when there is a `frontend/`) are regenerated from
//! each new binary before it starts, and only rewritten when they change, so
//! a frontend's `tsc --watch` or vite flags a Rust signature change at once.
//!
//! A one-file app (`zapline dev app.rs`, see [`crate::script`]) works the same
//! way with the file itself as the only watched path: each save regenerates
//! its hidden project (so header changes take effect) and rebuilds.

pub mod inject;
pub mod proxy;

use std::collections::BTreeSet;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{EventKind, RecursiveMode, Watcher};

use crate::build::{self, BuildOutcome, Diagnostic};
use crate::project::Project;
use crate::script::Script;
use crate::ui;
use proxy::{Event, Hub, Problem, Proxy};

/// Directories whose changes rebuild the app (watched recursively when
/// they exist).
pub const WATCHED_DIRS: &[&str] = &["src", "config", "migrations"];
/// Files in the project root whose changes rebuild the app.
pub const WATCHED_ROOT_FILES: &[&str] = &["Cargo.toml", "build.rs", ".env"];
/// Directories whose changes reload browsers without a rebuild.
pub const ASSET_DIRS: &[&str] = &["templates", "static", "public", "assets", "locales", "i18n"];
/// Asset directories served as files: a `.css` file there is hot swapped.
const SERVED_DIRS: &[&str] = &["static", "public", "assets"];
/// Directory names that are never interesting, wherever they appear.
const IGNORED_DIRS: &[&str] = &["target", ".git", "node_modules", ".idea", ".vscode"];

/// The variable that tells the app to listen behind the dev proxy (the
/// same name as `zapline::dev::ENV_DEV_PROXY`).
pub const ENV_DEV_PROXY: &str = "ZAPLINE_DEV_PROXY";

/// How often the main loop wakes up to check for exits and signals.
const TICK: Duration = Duration::from_millis(50);
/// Quiet period for asset changes: short, there is nothing to compile.
const ASSET_QUIET: Duration = Duration::from_millis(80);
/// How long a new version may take to report its address.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a new version may take to answer HTTP once listening.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Typed clients (`.ts`, or `.js` modules) to regenerate after every
    /// successful build; see [`crate::client::configured`].
    pub clients: Vec<PathBuf>,
    /// Whether `clients` were asked for (`--client`, `zapline.toml`) rather
    /// than found (`frontend/`). Found ones turn off quietly when the app has
    /// no OpenAPI document.
    pub clients_explicit: bool,
    /// Run the live reload proxy (browser reload, CSS hot swap, error
    /// overlay, zero-downtime swaps). `false` runs the app directly.
    pub proxy: bool,
    /// Start the app with `ZAPLINE_RECORD=1`: its dev console records
    /// requests for `zapline snap`.
    pub record: bool,
}

impl Default for DevOptions {
    fn default() -> Self {
        DevOptions {
            bin: None,
            release: false,
            app_args: Vec::new(),
            debounce: Duration::from_millis(300),
            grace: Duration::from_secs(5),
            clients: Vec::new(),
            clients_explicit: false,
            proxy: true,
            record: false,
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
        || name.starts_with(".goutputstream-") // GNOME's safe-write
        // `sed -i`'s scratch file: `sed` and six random characters.
        || (name.len() == 9 && name.starts_with("sed") && name.bytes().all(|b| b.is_ascii_alphanumeric()))
        || SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// What a changed file means for the running app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Rust code or something read at startup: rebuild and swap the app.
    Rebuild,
    /// A template, translation or served file: reload open pages.
    Reload,
    /// A served stylesheet: swap it in open pages, no reload.
    Css,
    /// Nothing to do.
    Ignore,
}

/// Classifies a change to `path` in the project at `root`.
///
/// ```
/// use std::path::Path;
/// use zapline_cli::dev::{classify, Change};
/// let root = Path::new("/app");
/// assert_eq!(classify(root, Path::new("/app/src/main.rs")), Change::Rebuild);
/// assert_eq!(classify(root, Path::new("/app/templates/index.html")), Change::Reload);
/// assert_eq!(classify(root, Path::new("/app/static/app.css")), Change::Css);
/// assert_eq!(classify(root, Path::new("/app/README.md")), Change::Ignore);
/// ```
pub fn classify(root: &Path, path: &Path) -> Change {
    let Ok(rel) = path.strip_prefix(root) else {
        return Change::Ignore;
    };
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let Some((file, dirs)) = parts.split_last() else {
        return Change::Ignore;
    };
    if parts.iter().any(|p| IGNORED_DIRS.contains(p)) || is_temp_file(file) {
        return Change::Ignore;
    }
    match dirs.first() {
        None if WATCHED_ROOT_FILES.contains(file) => Change::Rebuild,
        None => Change::Ignore,
        Some(top) if WATCHED_DIRS.contains(top) => Change::Rebuild,
        // Source maps change with their stylesheet; the CSS event covers it.
        Some(_) if file.ends_with(".map") => Change::Ignore,
        Some(top) if SERVED_DIRS.contains(top) && file.ends_with(".css") => Change::Css,
        Some(top) if ASSET_DIRS.contains(top) => Change::Reload,
        Some(_) => Change::Ignore,
    }
}

/// Whether a change to `path` should trigger a rebuild of the project at `root`.
pub fn is_relevant(root: &Path, path: &Path) -> bool {
    classify(root, path) == Change::Rebuild
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
    let shown: Vec<String> = files.iter().take(3).map(|p| relative(root, p)).collect();
    match files.len().saturating_sub(shown.len()) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

// ---- talking to the app ----------------------------------------------------------------

/// What a starting app reported in its handshake file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// The address the app asked for, e.g. `0.0.0.0:8080`.
    pub addr: String,
    /// Where it actually listens; `None` when it serves `addr` itself.
    pub internal: Option<SocketAddr>,
}

/// Parses the handshake JSON an app writes for `zapline dev`.
///
/// ```
/// use zapline_cli::dev::parse_handshake;
/// let hs = parse_handshake(r#"{"addr":"0.0.0.0:8080","internal":"127.0.0.1:41234","pid":7}"#).unwrap();
/// assert_eq!(hs.addr, "0.0.0.0:8080");
/// assert_eq!(hs.internal, Some("127.0.0.1:41234".parse().unwrap()));
/// ```
pub fn parse_handshake(json: &str) -> Option<Handshake> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(Handshake {
        addr: v.get("addr")?.as_str()?.to_owned(),
        internal: match v.get("internal")? {
            serde_json::Value::Null => None,
            other => Some(other.as_str()?.parse().ok()?),
        },
    })
}

/// Whether the executable can listen behind the proxy: it mentions
/// [`ENV_DEV_PROXY`] (zapline's `App::run` reads it). Streams the file, so
/// a large debug binary costs one sequential read.
pub fn speaks_handshake(exe: &Path) -> bool {
    let Ok(file) = std::fs::File::open(exe) else {
        return false;
    };
    contains(file, ENV_DEV_PROXY.as_bytes()).unwrap_or(false)
}

/// Whether `needle` occurs in everything `reader` yields.
fn contains(mut reader: impl Read, needle: &[u8]) -> std::io::Result<bool> {
    let keep = needle.len().saturating_sub(1);
    let mut buf = vec![0u8; 1 << 20];
    let mut filled = 0;
    loop {
        let n = reader.read(&mut buf[filled..])?;
        if n == 0 {
            return Ok(false);
        }
        filled += n;
        let window = &buf[..filled];
        let first = needle[0];
        let mut from = 0;
        while let Some(i) = window[from..].iter().position(|&b| b == first) {
            let at = from + i;
            if window[at..].starts_with(needle) {
                return Ok(true);
            }
            from = at + 1;
        }
        // Keep the tail: the needle may straddle two reads.
        let tail = filled.min(keep);
        buf.copy_within(filled - tail..filled, 0);
        filled = tail;
    }
}

/// `http://localhost:8080` for `0.0.0.0:8080`: an address to click on.
pub fn browser_url(addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() || addr.ip().is_loopback() {
        format!("http://localhost:{}", addr.port())
    } else {
        format!("http://{addr}")
    }
}

/// Where the proxy address of the project at `root` is remembered between
/// runs, so the error overlay works even when the very first build fails.
fn remembered_addr_file(root: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    std::env::temp_dir().join(format!("zapline-dev-{hex}.addr"))
}

/// The compiler errors worth showing: rustc's summary lines ("aborting due
/// to 2 previous errors") only when there is nothing better.
fn shown_errors(errors: Vec<Diagnostic>) -> Vec<Diagnostic> {
    if errors.iter().any(|d| d.file.is_some()) {
        errors.into_iter().filter(|d| d.file.is_some()).collect()
    } else {
        errors
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
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
fn terminate_pid(pid: u32) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    if let Ok(pid) = i32::try_from(pid) {
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
}

/// No SIGTERM outside unix: nothing to send; callers kill instead.
#[cfg(not(unix))]
fn terminate_pid(_pid: u32) {}

#[cfg(unix)]
fn send_terminate(child: &mut Child) {
    terminate_pid(child.id());
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

/// A cargo build running on a background thread.
struct BuildJob {
    result: mpsc::Receiver<Result<BuildOutcome>>,
    /// cargo's pid once it runs, to stop it on shutdown.
    pid: Arc<Mutex<Option<u32>>>,
}

/// A new version that was started and has not taken over yet.
struct Candidate {
    child: Child,
    exe: PathBuf,
    handshake_file: PathBuf,
    started: Instant,
    /// Set once the handshake file was read.
    handshake: Option<(Handshake, Instant)>,
}

struct Dev<'a> {
    project: &'a Project,
    /// Set for a one-file app: the file whose hidden project `project` is.
    script: Option<&'a Script>,
    /// The app's working directory.
    run_dir: PathBuf,
    opts: &'a DevOptions,
    bin: String,
    /// The version serving requests.
    app: Option<Child>,
    candidate: Option<Candidate>,
    /// Previous versions being stopped in the background.
    retiring: Vec<JoinHandle<()>>,
    build: Option<BuildJob>,
    rust: Debouncer,
    assets: Debouncer,
    shutdown: Arc<AtomicBool>,
    /// `tailwindcss --watch`, which exits when its stdin closes.
    css: Option<(Child, ChildStdin)>,
    hub: Arc<Hub>,
    /// The proxy and the address the app asked for (which differs from the
    /// bound one for port 0).
    proxy: Option<(Proxy, SocketAddr)>,
    /// Set once an app turned out not to listen behind the proxy.
    direct: bool,
    spawned: u64,
    /// The executable known to speak the proxy handshake.
    speaks: Option<PathBuf>,
    /// Typed clients regenerated after each build.
    clients: crate::client::Live,
}

impl Dev<'_> {
    fn stopping(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn shown<'p>(&self, exe: &'p Path) -> std::path::Display<'p> {
        exe.strip_prefix(&self.project.root)
            .unwrap_or(exe)
            .display()
    }

    fn proxied(&self) -> bool {
        self.proxy.is_some()
    }

    /// Starts `cargo build` on a background thread.
    fn start_build(&mut self) {
        match self.script {
            Some(script) => ui::status("Building", &script.shown),
            None => ui::status("Building", &self.bin),
        }
        self.hub.send(Event::Building);
        let (tx, rx) = mpsc::channel();
        let pid = Arc::new(Mutex::new(None));
        let (project, script, bin, release, slot) = (
            self.project.clone(),
            self.script.cloned(),
            self.bin.clone(),
            self.opts.release,
            pid.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("zapline-dev-build".into())
            .spawn(move || {
                let on_spawn = |id| {
                    if let Ok(mut slot) = slot.lock() {
                        *slot = Some(id);
                    }
                };
                let outcome = match script {
                    // Re-read the header first: a new dependency needs a new manifest.
                    Some(script) => script
                        .materialize()
                        .and_then(|m| script.build_with(&m, release, on_spawn)),
                    None => build::build_with(&project, &bin, release, on_spawn),
                };
                let _ = tx.send(outcome);
            });
        match spawned {
            Ok(_) => self.build = Some(BuildJob { result: rx, pid }),
            Err(e) => ui::status_error("Failed", format!("cannot start the build: {e}")),
        }
    }

    /// Handles a finished build, if there is one.
    fn poll_build(&mut self) {
        let Some(job) = &self.build else { return };
        let outcome = match job.result.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("the build thread died")),
        };
        self.build = None;
        if self.stopping() {
            return;
        }
        let keeps = if self.app.is_some() {
            "; the previous version keeps running"
        } else {
            ""
        };
        match outcome {
            Err(e) => {
                ui::status_error("Failed", format!("{e:#}"));
                self.hub.send(Event::Problem(Problem {
                    title: "Build failed".into(),
                    note: format!("{e:#}"),
                    errors: Vec::new(),
                }));
            }
            Ok(BuildOutcome::Failed(errors)) => {
                ui::status_error(
                    "Failed",
                    format!("build failed, fix the errors and save to retry{keeps}"),
                );
                let errors = shown_errors(errors);
                let note = if errors.is_empty() {
                    format!("cargo failed before compiling (see the terminal){keeps}.")
                } else {
                    format!("{}{keeps}.", plural(errors.len(), "error", "errors"))
                };
                self.hub.send(Event::Problem(Problem {
                    title: "Build failed".into(),
                    note,
                    errors,
                }));
            }
            Ok(BuildOutcome::Built(exe)) => {
                // Files changed while compiling: rebuild instead of starting
                // an already stale binary.
                if self.rust.is_pending() {
                    ui::status_info("Changed", "files changed during the build, rebuilding");
                    return;
                }
                if !self.clients.is_empty() {
                    self.clients
                        .refresh_and_report(self.project, &exe, self.opts.clients_explicit);
                }
                self.launch(&exe);
            }
        }
    }

    fn command(&self, exe: &Path) -> Command {
        let mut cmd = Command::new(exe);
        cmd.args(&self.opts.app_args).current_dir(&self.run_dir);
        // Dev mode: 500 responses carry the real error instead of a generic
        // message, so you (or your AI assistant) see what broke; templates
        // and translations are read again when they change.
        if std::env::var_os("ZAPLINE_DEV").is_none() {
            cmd.env("ZAPLINE_DEV", "1");
        }
        if self.opts.record {
            cmd.env("ZAPLINE_RECORD", "1");
        }
        // Where the app records its URL, so `zapline mcp` can talk to it.
        let addr_file = self.project.root.join(crate::live::ADDR_FILE);
        if let Some(dir) = addr_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::remove_file(&addr_file);
        cmd.env(crate::live::ENV_ADDR_FILE, &addr_file);
        cmd
    }

    /// Whether the dev loop may put the proxy in front of `exe`.
    fn can_proxy(&mut self, exe: &Path) -> bool {
        let dev_on = std::env::var_os("ZAPLINE_DEV").is_none_or(|v| v == "1");
        if !self.opts.proxy || self.direct || !dev_on {
            return false;
        }
        // Once a build of this binary spoke the handshake, later ones do
        // too: skip reading (often 100+ MB of) debug binary every rebuild.
        if self.speaks.as_deref() == Some(exe) {
            return true;
        }
        let speaks = speaks_handshake(exe);
        if speaks {
            self.speaks = Some(exe.to_owned());
        }
        speaks
    }

    /// Starts a freshly built `exe`.
    fn launch(&mut self, exe: &Path) {
        if let Some(mut old) = self.candidate.take() {
            // A newer build beat the previous one to the finish.
            stop(&mut old.child, Duration::from_secs(1));
            let _ = std::fs::remove_file(&old.handshake_file);
        }
        if !self.can_proxy(exe) {
            self.hub.send(Event::Ready);
            self.restart_direct(exe);
            return;
        }
        self.spawned += 1;
        let handshake_file = std::env::temp_dir().join(format!(
            "zapline-dev-{}-{}.json",
            std::process::id(),
            self.spawned
        ));
        let _ = std::fs::remove_file(&handshake_file);
        let mut cmd = self.command(exe);
        cmd.env(ENV_DEV_PROXY, &handshake_file);
        match cmd.spawn() {
            Ok(child) => {
                ui::status_info(
                    "Starting",
                    format!("`{}` (pid {})", self.shown(exe), child.id()),
                );
                self.candidate = Some(Candidate {
                    child,
                    exe: exe.to_owned(),
                    handshake_file,
                    started: Instant::now(),
                    handshake: None,
                });
            }
            Err(e) => {
                let msg = format!("cannot start {}: {e}", self.shown(exe));
                ui::status_error("Failed", &msg);
                self.hub.send(Event::Problem(Problem {
                    title: "The app failed to start".into(),
                    note: msg,
                    errors: Vec::new(),
                }));
            }
        }
    }

    /// The plain loop: stop the old process, then start the new one.
    fn restart_direct(&mut self, exe: &Path) {
        if let Some(mut old) = self.app.take() {
            ui::status_info("Stopping", format!("previous run (pid {})", old.id()));
            if stop(&mut old, self.opts.grace) == Stopped::Killed {
                ui::status_warn("Killed", "the app did not exit after SIGTERM");
            }
        }
        // The app binds its address itself now.
        self.proxy = None;
        let shown = self.shown(exe).to_string();
        match self.command(exe).spawn() {
            Ok(child) => {
                ui::status("Running", format!("`{shown}` (pid {})", child.id()));
                self.app = Some(child);
            }
            Err(e) => ui::status_error("Failed", format!("cannot start {shown}: {e}")),
        }
    }

    /// Moves a starting version along: read its handshake, wait until it
    /// answers, then let it take over.
    fn poll_candidate(&mut self) {
        let Some(c) = self.candidate.as_mut() else {
            return;
        };
        match c.child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                let file = c.handshake_file.clone();
                self.candidate = None;
                let _ = std::fs::remove_file(file);
                let keeps = if self.app.is_some() {
                    "; the previous version keeps running"
                } else {
                    ""
                };
                ui::status_error(
                    "Crashed",
                    format!("the new version exited during startup with {status}{keeps}"),
                );
                self.hub.send(Event::Problem(Problem {
                    title: "The app failed to start".into(),
                    note: format!(
                        "It exited with {status} before listening (see the terminal for its output){keeps}."
                    ),
                    errors: Vec::new(),
                }));
                return;
            }
            Err(e) => {
                ui::status_error("Failed", format!("cannot wait for the app: {e}"));
                self.candidate = None;
                return;
            }
        }
        if c.handshake.is_none() {
            if let Some(hs) = std::fs::read_to_string(&c.handshake_file)
                .ok()
                .and_then(|text| parse_handshake(&text))
            {
                c.handshake = Some((hs, Instant::now()));
            }
        }
        match c.handshake.clone() {
            None if c.started.elapsed() >= HANDSHAKE_TIMEOUT => {
                ui::status_warn(
                    "Direct",
                    "the app did not report its address (does it use `App::run`?); \
                     running it without the live reload proxy",
                );
                self.direct = true;
                self.promote(None);
            }
            None => {}
            Some((hs, _)) if hs.internal.is_none() => {
                ui::status_warn(
                    "Direct",
                    format!(
                        "the app serves {} itself (TLS or a Unix socket); \
                         running it without the live reload proxy",
                        hs.addr
                    ),
                );
                self.direct = true;
                self.promote(None);
            }
            Some((hs, since)) => {
                let internal = hs.internal.unwrap_or_else(|| ([127, 0, 0, 1], 0).into());
                if proxy::probe(internal, Duration::from_millis(500)) {
                    self.promote(Some((hs, internal)));
                } else if since.elapsed() >= HEALTH_TIMEOUT {
                    if let Some(mut c) = self.candidate.take() {
                        stop(&mut c.child, self.opts.grace);
                        let _ = std::fs::remove_file(&c.handshake_file);
                    }
                    let msg = format!(
                        "the new version listens on {internal} but did not answer HTTP within {}s",
                        HEALTH_TIMEOUT.as_secs()
                    );
                    ui::status_error("Failed", &msg);
                    self.hub.send(Event::Problem(Problem {
                        title: "The app did not answer".into(),
                        note: msg,
                        errors: Vec::new(),
                    }));
                }
            }
        }
    }

    /// The candidate takes over: requests go to it, browsers reload, and
    /// the previous version is stopped in the background.
    fn promote(&mut self, target: Option<(Handshake, SocketAddr)>) {
        let Some(c) = self.candidate.take() else {
            return;
        };
        let _ = std::fs::remove_file(&c.handshake_file);
        let mut url = None;
        match &target {
            Some((hs, internal)) => {
                url = self.ensure_proxy(&hs.addr, *internal);
                self.hub.set_upstream(Some(*internal), None);
            }
            None => {
                self.hub.set_upstream(None, None);
                self.proxy = None;
            }
        }
        let pid = c.child.id();
        if let Some(old) = self.app.replace(c.child) {
            self.retire(old);
        }
        let at = url.map(|u| format!(" at {u}")).unwrap_or_default();
        ui::status(
            "Running",
            format!("`{}` (pid {pid}){at}", self.shown(&c.exe)),
        );
        self.hub.send(Event::Clear);
        self.hub.send(Event::Ready);
        self.hub.send(Event::Reload);
    }

    /// Makes sure the proxy listens on `asked` (the app's own address) and
    /// returns its URL.
    fn ensure_proxy(&mut self, asked: &str, internal: SocketAddr) -> Option<String> {
        let Ok(addr) = asked.parse::<SocketAddr>() else {
            ui::status_warn("Proxy", format!("cannot use the app's address {asked:?}"));
            return None;
        };
        if let Some((proxy, wanted)) = &self.proxy {
            if *wanted == addr {
                return Some(browser_url(proxy.addr()));
            }
        }
        // A new address: let go of the old one first (it may be the same port).
        self.proxy = None;
        match Proxy::bind(addr, self.hub.clone()) {
            Ok(proxy) => {
                let url = browser_url(proxy.addr());
                ui::status_info("Proxy", format!("{url} with live reload"));
                let _ = std::fs::write(remembered_addr_file(&self.project.root), asked);
                self.proxy = Some((proxy, addr));
                Some(url)
            }
            Err(e) => {
                ui::status_error(
                    "Failed",
                    format!(
                        "cannot listen on {addr}: {e}; the app is at {} without live reload",
                        browser_url(internal)
                    ),
                );
                None
            }
        }
    }

    /// Listens on the address the app used last time right away, so pages
    /// show the build's progress (and errors) before the app is up.
    fn listen_early(&mut self) {
        let Ok(text) = std::fs::read_to_string(remembered_addr_file(&self.project.root)) else {
            return;
        };
        let Ok(addr) = text.trim().parse::<SocketAddr>() else {
            return;
        };
        if let Ok(proxy) = Proxy::bind(addr, self.hub.clone()) {
            ui::status_info(
                "Proxy",
                format!("{} with live reload", browser_url(proxy.addr())),
            );
            self.proxy = Some((proxy, addr));
        }
    }

    /// Stops a replaced version on a background thread (it may take the
    /// grace period to finish its last requests).
    fn retire(&mut self, mut old: Child) {
        ui::status_info("Stopping", format!("previous run (pid {})", old.id()));
        let grace = self.opts.grace;
        let spawned = std::thread::Builder::new()
            .name("zapline-dev-stop".into())
            .spawn(move || {
                if stop(&mut old, grace) == Stopped::Killed {
                    ui::status_warn("Killed", "the previous run did not exit after SIGTERM");
                }
            });
        match spawned {
            Ok(handle) => self.retiring.push(handle),
            Err(e) => ui::status_error("Failed", format!("cannot stop the previous run: {e}")),
        }
        self.retiring.retain(|h| !h.is_finished());
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
                if self.proxied() {
                    self.hub
                        .set_upstream(None, Some(format!("The app exited with {status}.")));
                    if !status.success() {
                        self.hub.send(Event::Problem(Problem {
                            title: "The app crashed".into(),
                            note: format!(
                                "It exited with {status} (see the terminal for its output). \
                                 Save a file to rebuild."
                            ),
                            errors: Vec::new(),
                        }));
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                self.app = None;
                ui::status_error("Failed", format!("cannot wait for the app: {e}"));
            }
        }
    }

    /// Tells open pages about settled asset changes.
    fn push_assets(&mut self) {
        let files = self.assets.take();
        if !self.proxied() {
            return;
        }
        let root = &self.project.root;
        let css: Vec<String> = files
            .iter()
            .filter(|f| classify(root, f) == Change::Css)
            .map(|f| relative(root, f))
            .collect();
        let pages = plural(self.hub.clients(), "page", "pages");
        let changed = describe_changes(root, &files);
        if css.len() == files.len() {
            self.hub.send(Event::Css(css));
            ui::status_info(
                "Updated",
                format!("{changed}: stylesheet swapped in {pages}"),
            );
        } else {
            self.hub.send(Event::Reload);
            ui::status_info("Reloaded", format!("{changed}: {pages}"));
        }
    }

    /// Reads file system events for up to `wait`, feeding the debouncers.
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
                            let change = match self.script {
                                Some(script) if path == script.path => Change::Rebuild,
                                Some(_) => Change::Ignore,
                                None => classify(&self.project.root, &path),
                            };
                            match change {
                                Change::Rebuild => self.rust.push(now, path),
                                Change::Reload | Change::Css => self.assets.push(now, path),
                                Change::Ignore => {}
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
        if let Some(job) = self.build.take() {
            if let Some(pid) = job.pid.lock().ok().and_then(|p| *p) {
                terminate_pid(pid);
            }
        }
        if let Some(mut c) = self.candidate.take() {
            stop(&mut c.child, self.opts.grace);
            let _ = std::fs::remove_file(&c.handshake_file);
        }
        if let Some(mut child) = self.app.take() {
            ui::status_info("Stopping", format!("the app (pid {})", child.id()));
            stop(&mut child, self.opts.grace);
        }
        for handle in self.retiring.drain(..) {
            let _ = handle.join();
        }
        if let Some((mut child, stdin)) = self.css.take() {
            drop(stdin);
            stop(&mut child, Duration::from_secs(1));
        }
        self.proxy = None;
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
    let mut assets = Vec::new();
    for dir in WATCHED_DIRS.iter().chain(ASSET_DIRS) {
        let path = project.root.join(dir);
        if path.is_dir() {
            watcher
                .watch(&path, RecursiveMode::Recursive)
                .with_context(|| format!("cannot watch {}", path.display()))?;
            if ASSET_DIRS.contains(dir) {
                assets.push(format!("{dir}/"));
            } else {
                watched.push(format!("{dir}/"));
            }
        }
    }
    // Root-level files (Cargo.toml, build.rs) and directories created later.
    watcher
        .watch(&project.root, RecursiveMode::NonRecursive)
        .with_context(|| format!("cannot watch {}", project.root.display()))?;
    watched.extend(WATCHED_ROOT_FILES.iter().map(|f| f.to_string()));
    ui::status_info("Watching", watched.join(", "));
    if opts.proxy && !assets.is_empty() {
        ui::status_info(
            "Watching",
            format!("{} (browser reload, no rebuild)", assets.join(", ")),
        );
    }

    let mut dev = Dev::new(project, opts, bin, shutdown);
    if opts.proxy {
        dev.listen_early();
    }
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
    dev.main_loop(&rx, &project.root);
    Ok(())
}

/// Runs the dev loop for a one-file app until interrupted. The app runs in the
/// current directory, like `zapline run`.
pub fn run_script(script: &Script, opts: &DevOptions) -> Result<()> {
    // A broken header is reported before anything starts.
    let materialized = script.materialize()?;
    let run_dir = std::env::current_dir().context("cannot determine the current directory")?;
    let shutdown = install_signal_handlers()?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("cannot start the file watcher")?;
    // The directory, not the file: editors that save by renaming a temporary
    // file over the original would otherwise end the watch after one save.
    let dir = script
        .path
        .parent()
        .context("the script has no parent directory")?;
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("cannot watch {}", dir.display()))?;
    ui::status_info("Watching", &script.shown);

    let mut dev = Dev::new(&materialized.project, opts, script.bin.clone(), shutdown);
    dev.script = Some(script);
    dev.run_dir = run_dir;
    if opts.proxy {
        dev.listen_early();
    }
    dev.main_loop(&rx, dir);
    Ok(())
}

impl<'a> Dev<'a> {
    fn new(
        project: &'a Project,
        opts: &'a DevOptions,
        bin: String,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Dev {
            project,
            script: None,
            run_dir: project.root.clone(),
            opts,
            bin,
            app: None,
            candidate: None,
            retiring: Vec::new(),
            build: None,
            rust: Debouncer::new(opts.debounce),
            assets: Debouncer::new(opts.debounce.min(ASSET_QUIET)),
            shutdown,
            css: None,
            hub: Hub::new(),
            proxy: None,
            direct: false,
            spawned: 0,
            speaks: None,
            clients: crate::client::Live::new(opts.clients.clone()),
        }
    }

    /// Builds and starts the app, then rebuilds after every settled burst of
    /// changes (and reloads browsers after asset changes) until a shutdown
    /// signal arrives.
    fn main_loop(&mut self, rx: &mpsc::Receiver<notify::Result<notify::Event>>, root: &Path) {
        self.start_build();
        while !self.stopping() {
            self.collect_events(rx, TICK);
            // Ctrl-C reaches the app too; do not report that as a crash.
            if self.stopping() {
                break;
            }
            self.poll_build();
            self.poll_candidate();
            self.reap_exited();
            let now = Instant::now();
            if self.assets.is_ready(now) {
                self.push_assets();
            }
            if self.build.is_none() && self.rust.is_ready(now) {
                let files = self.rust.take();
                ui::status_info("Changed", describe_changes(root, &files));
                self.start_build();
            }
        }
        self.finish();
    }
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
            "src/sedTz9Jkj",
            "templates/.goutputstream-ABC123",
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

    fn kind(rel: &str) -> Change {
        classify(&root(), &root().join(rel))
    }

    #[test]
    fn classifies_rust_and_asset_changes() {
        for p in [
            "src/main.rs",
            "config/default.toml",
            "migrations/1_x.sql",
            ".env",
            "build.rs",
        ] {
            assert_eq!(kind(p), Change::Rebuild, "{p}");
        }
        for p in [
            "templates/index.html",
            "templates/partials/nav.html",
            // Templates may inline a stylesheet: not a served file to swap.
            "templates/email.css",
            "static/app.js",
            "static/img/logo.svg",
            "public/robots.txt",
            "locales/en.toml",
            "i18n/fr.toml",
        ] {
            assert_eq!(kind(p), Change::Reload, "{p}");
        }
        for p in [
            "static/app.css",
            "static/css/site.css",
            "public/main.css",
            "assets/x.css",
        ] {
            assert_eq!(kind(p), Change::Css, "{p}");
        }
        for p in [
            "README.md",
            "Cargo.lock",
            "tests/api.rs",
            // Tailwind's input: its output in static/ is what changes the page.
            "styles/app.css",
            "static/app.css.map",
            "static/.app.css.swp",
            "templates/index.html~",
            "static/node_modules/x/y.css",
            "target/debug/build/x.css",
        ] {
            assert_eq!(kind(p), Change::Ignore, "{p}");
        }
        assert_eq!(
            classify(&root(), Path::new("/elsewhere/static/a.css")),
            Change::Ignore
        );
        // Templates are reloaded by the browser, never rebuilt.
        assert!(!relevant("templates/index.html"));
        assert!(relevant(".env"));
    }

    #[test]
    fn parses_handshakes() {
        let hs =
            parse_handshake(r#"{"addr":"[::]:3000","internal":"[::1]:4000","pid":1}"#).unwrap();
        assert_eq!(hs.addr, "[::]:3000");
        assert_eq!(hs.internal, Some("[::1]:4000".parse().unwrap()));
        let direct = parse_handshake(r#"{"addr":"unix:/tmp/app.sock","internal":null}"#).unwrap();
        assert_eq!(direct.internal, None);
        assert_eq!(parse_handshake(r#"{"addr":"x","internal":"nope"}"#), None);
        assert_eq!(parse_handshake(r#"{"internal":null}"#), None);
        assert_eq!(parse_handshake("{\"addr\":"), None);
    }

    #[test]
    fn finds_the_handshake_marker_across_read_boundaries() {
        let needle = ENV_DEV_PROXY.as_bytes();
        // Reads of 7 bytes split the needle every way.
        struct Trickle(std::io::Cursor<Vec<u8>>);
        impl Read for Trickle {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = buf.len().min(7);
                self.0.read(&mut buf[..n])
            }
        }
        for offset in 0..20 {
            let mut bytes = vec![b'Z'; offset];
            bytes.extend_from_slice(b"ZAPLINE_DEV_PROX");
            bytes.extend_from_slice(needle);
            bytes.extend_from_slice(b"tail");
            assert!(contains(Trickle(std::io::Cursor::new(bytes)), needle).unwrap());
        }
        let without = b"ZAPLINE_DEV ZAPLINE_DEV_PROX ZAPLINE_DEV_PROXZ".to_vec();
        assert!(!contains(Trickle(std::io::Cursor::new(without)), needle).unwrap());
        assert!(!contains(std::io::empty(), needle).unwrap());

        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("app");
        std::fs::write(&exe, [b"\0\x7fELF".as_slice(), needle, b"\0"].concat()).unwrap();
        assert!(speaks_handshake(&exe));
        std::fs::write(&exe, b"\0\x7fELF other\0").unwrap();
        assert!(!speaks_handshake(&exe));
        assert!(!speaks_handshake(&tmp.path().join("missing")));
    }

    #[test]
    fn urls_to_click() {
        assert_eq!(
            browser_url("0.0.0.0:8080".parse().unwrap()),
            "http://localhost:8080"
        );
        assert_eq!(
            browser_url("[::1]:3000".parse().unwrap()),
            "http://localhost:3000"
        );
        assert_eq!(
            browser_url("10.0.0.2:80".parse().unwrap()),
            "http://10.0.0.2:80"
        );
    }

    #[test]
    fn keeps_errors_with_a_location() {
        let d = |file: Option<&str>| Diagnostic {
            level: "error".into(),
            message: "m".into(),
            file: file.map(str::to_owned),
            line: 1,
            column: 1,
            rendered: String::new(),
        };
        let shown = shown_errors(vec![d(Some("src/a.rs")), d(None)]);
        assert_eq!(shown, [d(Some("src/a.rs"))]);
        assert_eq!(shown_errors(vec![d(None)]).len(), 1);
        assert_eq!(plural(1, "error", "errors"), "1 error");
        assert_eq!(plural(3, "error", "errors"), "3 errors");
    }

    #[test]
    fn remembers_one_address_per_project() {
        let a = remembered_addr_file(Path::new("/work/a"));
        assert_eq!(a, remembered_addr_file(Path::new("/work/a")));
        assert_ne!(a, remembered_addr_file(Path::new("/work/b")));
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
