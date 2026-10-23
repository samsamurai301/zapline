//! `zapline ship <user@host>`: build a release binary, copy it to a Linux
//! server over ssh, and swap it in without dropping a request.
//!
//! What happens, in order:
//!
//! 1. `ssh host uname -m` checks the server's CPU architecture against what
//!    will be built.
//! 2. `cargo build --release` (for `--target`, or the static musl target with
//!    `--static`), with [`ENV_EMBED_STAMP`] set so files added to embedded
//!    directories are picked up.
//! 3. `scp` uploads the binary, a systemd unit, the first-run settings file
//!    and the install script into a private directory in `/tmp`.
//! 4. `ssh host sh .../ship.sh` runs the script ([`script`]): on the first
//!    run it creates a system user, `/etc/<app>/env` (with generated
//!    secrets) and the `<app>@.service` template unit; then it installs the
//!    binary under `/opt/<app>/releases/<id>/` and starts `<app>@<id>` next
//!    to the running release on the same port (`SO_REUSEPORT`).
//!    `systemctl start` returns once the new process has answered its own
//!    health check (`ZAPLINE_READY_PATH`, `READY=1`); the script then checks
//!    the port end to end, stops the old release (which drains), and
//!    removes all but the newest releases. Any failure before the old
//!    release stops leaves it serving and removes the new one.
//!
//! `--dry-run` prints every command and every generated file without
//! running or writing anything. `--rollback` starts the previous release
//! the same way.
//!
//! Only the system `ssh` and `scp` are used: keys, agents, `~/.ssh/config`
//! and known hosts work as they do in a terminal.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::build::{cargo, executable_from_message};
use crate::generate::deploy::{env_keys, Facts};
use crate::project::Project;
use crate::ui;

/// The environment variable `zapline::embed!` lists as a build input; a new
/// value makes cargo re-read embedded directories.
pub const ENV_EMBED_STAMP: &str = "ZAPLINE_EMBED_STAMP";

/// Where releases, settings and data live on the server.
pub const REMOTE_BASE: &str = "/opt";

/// What `zapline ship` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShipOptions {
    /// `user@host` (or a `Host` from `~/.ssh/config`).
    pub host: String,
    /// Binary target to ship [default: the package's only binary].
    pub bin: Option<String>,
    /// Service name on the server [default: the package name].
    pub name: Option<String>,
    /// Port written to the settings file on the first deploy.
    pub port: u16,
    /// Health check path [default: `/health/ready` with the `Health`
    /// plugin, else `/`].
    pub health: Option<String>,
    /// Cargo target triple to build for.
    pub target: Option<String>,
    /// Build a static binary (the musl target for the server's CPU).
    pub static_build: bool,
    /// ssh port.
    pub ssh_port: Option<u16>,
    /// ssh identity file.
    pub identity: Option<PathBuf>,
    /// Settings to write into the server's env file (`KEY=VALUE`).
    pub env: Vec<String>,
    /// Releases kept on the server, including the live one.
    pub keep: usize,
    /// Print the commands instead of running them.
    pub dry_run: bool,
    /// Start the previous release instead of shipping a new one.
    pub rollback: bool,
}

impl Default for ShipOptions {
    fn default() -> Self {
        ShipOptions {
            host: String::new(),
            bin: None,
            name: None,
            port: 8080,
            health: None,
            target: None,
            static_build: false,
            ssh_port: None,
            identity: None,
            env: Vec::new(),
            keep: 3,
            dry_run: false,
            rollback: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Toolchain: what to build with

/// What the local machine can build, gathered once (and faked in tests).
#[derive(Debug, Clone)]
pub struct Toolchain {
    /// `std::env::consts::OS` of this machine.
    pub os: String,
    /// `std::env::consts::ARCH` of this machine.
    pub arch: String,
    /// Targets `rustup` has installed, or `None` without rustup.
    pub installed_targets: Option<Vec<String>>,
    /// Programs found on `PATH`.
    pub programs: Vec<String>,
}

impl Toolchain {
    /// Look at this machine.
    pub fn detect() -> Toolchain {
        let installed_targets = Command::new("rustup")
            .args(["target", "list", "--installed"])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_owned())
                    .filter(|l| !l.is_empty())
                    .collect()
            });
        let candidates = [
            "musl-gcc",
            "x86_64-linux-musl-gcc",
            "aarch64-linux-musl-gcc",
            "aarch64-linux-gnu-gcc",
            "x86_64-linux-gnu-gcc",
            "cargo-zigbuild",
            "zig",
        ];
        Toolchain {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            installed_targets,
            programs: candidates
                .iter()
                .filter(|p| on_path(p))
                .map(|p| (*p).to_owned())
                .collect(),
        }
    }

    fn has(&self, program: &str) -> bool {
        self.programs.iter().any(|p| p == program)
    }
}

/// Whether `program` is an executable on `PATH`.
pub fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        fs::metadata(&candidate).is_ok_and(|m| m.is_file())
    })
}

/// How the binary gets built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    /// `cargo` or `cargo-zigbuild`'s subcommand (`zigbuild`).
    pub subcommand: &'static str,
    /// `--target`, if any.
    pub target: Option<String>,
    /// Extra environment for cargo (C compiler and linker for the target).
    pub env: Vec<(String, String)>,
    /// The CPU the binary is for (`x86_64`, `aarch64`, ...).
    pub arch: String,
}

/// The CPU architecture of a target triple (`aarch64-unknown-linux-musl`
/// -> `aarch64`).
fn triple_arch(triple: &str) -> &str {
    triple.split('-').next().unwrap_or(triple)
}

/// `uname -m` output to Rust's architecture names.
pub fn rust_arch(uname_m: &str) -> String {
    match uname_m.trim() {
        "amd64" => "x86_64".to_owned(),
        "arm64" | "armv8l" | "armv8" => "aarch64".to_owned(),
        m if m.starts_with("armv7") => "armv7".to_owned(),
        m => m.to_owned(),
    }
}

/// `CC_aarch64_unknown_linux_musl` / `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER`.
fn target_env(prefix: &str, triple: &str, suffix: &str) -> String {
    let t = triple.replace('-', "_");
    if prefix == "CC_" {
        format!("CC_{t}")
    } else {
        format!("{prefix}{}{suffix}", t.to_ascii_uppercase())
    }
}

/// Decide how to build for a server with CPU `remote_arch` (when known).
///
/// # Errors
///
/// When this machine can't build for the server, with what to install.
pub fn plan_build(
    opts: &ShipOptions,
    tools: &Toolchain,
    remote_arch: Option<&str>,
) -> Result<BuildPlan> {
    let arch = remote_arch.unwrap_or(&tools.arch).to_owned();
    let target = match (&opts.target, opts.static_build) {
        (Some(t), _) => Some(t.clone()),
        (None, true) => Some(format!("{arch}-unknown-linux-musl")),
        (None, false) => {
            if tools.os != "linux" {
                bail!(
                    "this machine builds {} binaries, the server runs Linux\n\
                     Build a static Linux binary instead: zapline ship {} --static\n\
                     (needs `rustup target add {arch}-unknown-linux-musl` and \
                     `cargo install cargo-zigbuild` with zig installed)",
                    tools.os,
                    opts.host
                );
            }
            if arch != tools.arch {
                bail!(
                    "the server's CPU is {arch}, this machine's is {}\n\
                     Cross-compile: zapline ship {} --static (or --target {arch}-unknown-linux-gnu)",
                    tools.arch,
                    opts.host
                );
            }
            None
        }
    };
    let Some(triple) = target else {
        return Ok(BuildPlan {
            subcommand: "build",
            target: None,
            env: Vec::new(),
            arch,
        });
    };
    let target_arch = triple_arch(&triple).to_owned();
    if remote_arch.is_some_and(|r| rust_arch(r) != rust_arch(&target_arch)) {
        bail!("--target {triple} builds for {target_arch}, but the server's CPU is {arch}");
    }
    if let Some(installed) = &tools.installed_targets {
        if !installed.contains(&triple) {
            bail!(
                "the Rust standard library for {triple} is not installed\n\
                 Install it with: rustup target add {triple}"
            );
        }
    }
    let native = tools.os == "linux" && tools.arch == target_arch;
    let musl = triple.contains("musl");
    let mut env = Vec::new();
    let subcommand = if musl && native && tools.has("musl-gcc") {
        // Crates with C code (ring, SQLite) compile against musl's headers.
        env.push((target_env("CC_", &triple, ""), "musl-gcc".to_owned()));
        "build"
    } else if musl && tools.has(&format!("{target_arch}-linux-musl-gcc")) {
        let cc = format!("{target_arch}-linux-musl-gcc");
        env.push((target_env("CC_", &triple, ""), cc.clone()));
        env.push((target_env("CARGO_TARGET_", &triple, "_LINKER"), cc));
        "build"
    } else if !musl && native {
        "build"
    } else if !musl && tools.has(&format!("{target_arch}-linux-gnu-gcc")) {
        let cc = format!("{target_arch}-linux-gnu-gcc");
        env.push((target_env("CC_", &triple, ""), cc.clone()));
        env.push((target_env("CARGO_TARGET_", &triple, "_LINKER"), cc));
        "build"
    } else if tools.has("cargo-zigbuild") && tools.has("zig") {
        "zigbuild"
    } else {
        let apt = if musl {
            "musl-tools"
        } else {
            "gcc-<arch>-linux-gnu"
        };
        bail!(
            "no C toolchain for {triple} (crates such as ring and SQLite compile C code)\n\
             Install one of:\n\
             \x20 sudo apt install {apt}{}\n\
             \x20 cargo install cargo-zigbuild, plus zig (https://ziglang.org/download/)",
            if musl && native {
                "   (provides musl-gcc)"
            } else {
                ""
            }
        );
    };
    Ok(BuildPlan {
        subcommand,
        target: Some(triple),
        env,
        arch,
    })
}

// ---------------------------------------------------------------------------
// The plan

/// Everything one `zapline ship` run does, computed up front so `--dry-run`
/// shows exactly what would run.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Service name (also the system user and directory names).
    pub app: String,
    /// Binary target name.
    pub bin: String,
    /// Release id: `YYYYMMDDHHMMSS`, plus the git commit when there is one.
    pub release: String,
    /// The build, unless rolling back.
    pub build: Option<BuildPlan>,
    /// Health check path.
    pub health: String,
    /// Directory on the server the files are uploaded to.
    pub remote_dir: String,
    /// The generated files, by name.
    pub files: Files,
    /// `ssh`/`scp` options shared by every call.
    pub ssh: Vec<String>,
    /// `scp`'s spelling of the same options.
    pub scp: Vec<String>,
    /// Destination host.
    pub host: String,
    /// `ZAPLINE_EMBED_STAMP` for the build.
    pub embed_stamp: String,
}

/// The files uploaded next to the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    /// `ship.sh`: the install-and-swap script.
    pub script: String,
    /// `unit`: the systemd template unit `<app>@.service`.
    pub unit: String,
    /// `env`: the settings file for the first deploy (secrets are added on
    /// the server).
    pub env: String,
    /// `env.set`: `--env` settings merged into the settings file.
    pub env_set: String,
}

/// Service names: lowercase letters, digits, `-` and `_`, starting with a
/// letter (they are also user names).
pub fn service_name(name: &str) -> Result<String> {
    let out: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if !out.starts_with(|c: char| c.is_ascii_lowercase()) || out.len() > 32 {
        bail!("`{name}` can't be a service name: use --name with a short lowercase name starting with a letter");
    }
    Ok(out)
}

/// The release id for `secs` (UTC) and commit `git`.
pub fn release_id(secs: u64, git: Option<&str>) -> String {
    let ts = crate::db::timestamp(secs);
    match git
        .map(str::trim)
        .filter(|g| !g.is_empty() && g.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        Some(g) => format!("{ts}-{g}"),
        None => ts,
    }
}

/// A hash of the project's file names (not contents: cargo already tracks
/// those): changes when a file is added or removed, so `embed!` re-reads.
pub fn embed_stamp(root: &Path) -> String {
    let mut names = Vec::new();
    collect_names(root, "", &mut names);
    names.sort();
    let mut h = Sha256::new();
    for n in &names {
        h.update(n.as_bytes());
        h.update([0]);
    }
    format!("{:x}", h.finalize())[..16].to_owned()
}

fn collect_names(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || (prefix.is_empty() && name == "target") {
            continue;
        }
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_names(&entry.path(), &rel, out),
            Ok(_) => out.push(rel),
            Err(_) => {}
        }
    }
}

/// `KEY=VALUE` settings from `--env`, checked.
pub fn parse_env_settings(settings: &[String]) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for s in settings {
        let Some((k, v)) = s.split_once('=') else {
            bail!("--env {s}: expected KEY=VALUE");
        };
        let k = k.trim();
        let valid = !k.is_empty()
            && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !k.starts_with(|c: char| c.is_ascii_digit());
        if !valid {
            bail!("--env {s}: `{k}` is not a valid variable name");
        }
        if v.contains(['\n', '\r', '\0']) {
            bail!("--env {k}: values can't contain line breaks");
        }
        out.retain(|(key, _)| key != k);
        out.push((k.to_owned(), v.to_owned()));
    }
    Ok(out)
}

/// Settings the host generates (long random values) on the first deploy.
pub fn is_secret(key: &str) -> bool {
    key.ends_with("_SECRET") || key.ends_with("_KEY") || key == "SECRET"
}

/// What the generated files depend on.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// Service name.
    pub app: String,
    /// Binary name.
    pub bin: String,
    /// Release id (empty for a rollback: the script picks one).
    pub release: String,
    /// Port for the first deploy.
    pub port: u16,
    /// Health check path.
    pub health: String,
    /// Releases kept.
    pub keep: usize,
    /// SHA-256 of the binary (empty for a rollback or a dry run).
    pub sha256: String,
    /// Facts about the project (database, secrets).
    pub facts: Facts,
    /// `.env.example`, for the settings the app reads.
    pub env_example: String,
    /// `--env` settings.
    pub env_set: Vec<(String, String)>,
}

/// The systemd template unit `<app>@.service`; the instance name is the
/// release id.
pub fn unit_file(i: &Inputs) -> String {
    let app = &i.app;
    let mut out = format!(
        "# Generated by `zapline ship`, rewritten on every deploy.\n\
         # One instance per release (`{app}@<release>`); settings go in /etc/{app}/env.\n\
         [Unit]\n\
         Description={app} release %i (zapline)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         # Ready once the app answers its own health check (READY=1).\n\
         Type=notify\n\
         NotifyAccess=main\n\
         ExecStart={REMOTE_BASE}/{app}/releases/%i/{bin}\n\
         WorkingDirectory=/var/lib/{app}\n\
         EnvironmentFile=/etc/{app}/env\n\
         Environment=ZAPLINE_READY_PATH={health}\n\
         Environment=ZAPLINE_READY_TIMEOUT=60\n\
         # The new release binds the port while the old one still serves.\n\
         Environment=ZAPLINE_REUSE_PORT=1\n\
         User={app}\n\
         Group={app}\n\
         StateDirectory={app}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         TimeoutStartSec=90\n\
         # Longer than the app's shutdown timeout (30s by default).\n\
         TimeoutStopSec=40\n\
         KillSignal=SIGTERM\n\
         LimitNOFILE=65536\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         ProtectKernelTunables=yes\n\
         ProtectControlGroups=yes\n\
         ReadWritePaths=/var/lib/{app}\n",
        bin = i.bin,
        health = i.health,
    );
    if i.port < 1024 {
        out.push_str(
            "# Ports below 1024 need this capability.\nAmbientCapabilities=CAP_NET_BIND_SERVICE\n",
        );
    }
    out.push_str("\n[Install]\nWantedBy=multi-user.target\n");
    out
}

/// The first deploy's `/etc/<app>/env` (without the secrets, which the
/// server generates).
pub fn env_file(i: &Inputs) -> String {
    let app = &i.app;
    let mut out = format!(
        "# Settings for {app}, read by systemd. Written by the first `zapline ship`;\n\
         # yours from then on: edit it, then ship again (or `zapline ship --env K=V`).\n\
         PORT={port}\n\
         APP_ENV=production\n\
         RUST_LOG=info\n",
        port = i.port
    );
    if i.facts.postgres {
        let _ = writeln!(
            out,
            "# Required: the Postgres database.\n# DATABASE_URL=postgres://{app}:password@localhost/{app}"
        );
    } else if i.facts.sqlite {
        let _ = writeln!(out, "DATABASE_URL=sqlite:///var/lib/{app}/app.db?mode=rwc");
    }
    if i.facts.redis {
        out.push_str("# REDIS_URL=redis://127.0.0.1:6379\n");
    }
    let handled = [
        "ADDR",
        "PORT",
        "DATABASE_URL",
        "REDIS_URL",
        "APP_ENV",
        "RUST_LOG",
    ];
    let example: Vec<(String, String)> = example_values(&i.env_example);
    let others: Vec<&(String, String)> = example
        .iter()
        .filter(|(k, _)| !handled.contains(&k.as_str()) && !is_secret(k))
        .collect();
    if !others.is_empty() {
        out.push_str("# From .env.example:\n");
        for (k, v) in others {
            let _ = writeln!(out, "# {k}={v}");
        }
    }
    out
}

/// `KEY` -> example value from a `.env.example` (commented lines too).
fn example_values(text: &str) -> Vec<(String, String)> {
    let keys = env_keys(text);
    keys.into_iter()
        .map(|k| {
            let value = text
                .lines()
                .map(|l| l.trim().trim_start_matches('#').trim())
                .filter_map(|l| l.trim_start_matches("export ").split_once('='))
                .find(|(key, _)| key.trim() == k)
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default();
            (k, value)
        })
        .collect()
}

/// The settings the server generates on the first deploy.
fn secret_keys(i: &Inputs) -> Vec<String> {
    let mut keys: Vec<String> = env_keys(&i.env_example)
        .into_iter()
        .filter(|k| is_secret(k))
        .collect();
    keys.retain(|k| !i.env_set.iter().any(|(s, _)| s == k));
    keys
}

/// `'text'` for sh.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b))
    {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The script that installs and swaps the release on the server. POSIX
/// `sh`; every path is under `$ZAPLINE_SHIP_ROOT` (empty: `/`), so it can
/// be exercised in a scratch directory.
pub fn script(i: &Inputs, rollback: bool) -> String {
    let q = sh_quote;
    let mut out = String::new();
    let _ = write!(
        out,
        "#!/bin/sh\n\
         # Generated by `zapline ship`: installs {app} and swaps it in without downtime.\n\
         set -eu\n\
         \n\
         APP={app}\n\
         BIN={bin}\n\
         RELEASE={release}\n\
         MODE={mode}\n\
         HEALTH={health}\n\
         DEFAULT_PORT={port}\n\
         KEEP={keep}\n\
         SHA256={sha}\n\
         SECRETS={secrets}\n",
        app = q(&i.app),
        bin = q(&i.bin),
        release = q(&i.release),
        mode = if rollback { "rollback" } else { "deploy" },
        health = q(&i.health),
        port = i.port,
        keep = i.keep.max(1),
        sha = q(&i.sha256),
        secrets = q(&secret_keys(i).join(" ")),
    );
    out.push_str(SCRIPT_BODY);
    out
}

const SCRIPT_BODY: &str = r##"
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=${ZAPLINE_SHIP_ROOT:-}
BASE=$ROOT/opt/$APP
ETC=$ROOT/etc/$APP
ENV_FILE=$ETC/env
UNIT_FILE=$ROOT/etc/systemd/system/$APP@.service
SYSCTL_FILE=$ROOT/etc/sysctl.d/60-zapline-reuseport.conf
DATA=$ROOT/var/lib/$APP
LOCK=$BASE/.ship-lock

say() { printf 'zapline ship: %s\n' "$*"; }
fail() { printf 'zapline ship: error: %s\n' "$*" >&2; exit 1; }

if [ "$(id -u)" -eq 0 ]; then
  SUDO=
else
  command -v sudo >/dev/null 2>&1 || fail "log in as root, or install sudo"
  SUDO=sudo
fi
as_root() { $SUDO "$@"; }

command -v systemctl >/dev/null 2>&1 || fail "this server has no systemd (systemctl not found)"

locked=
cleanup() {
  if [ -n "$locked" ]; then as_root rmdir "$LOCK" 2>/dev/null || true; fi
  case $HERE in
    */zapline-ship.*) rm -rf "$HERE" ;;
  esac
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

as_root mkdir -p "$BASE/releases"
as_root mkdir "$LOCK" 2>/dev/null || fail "another deploy of $APP is running (if not, remove $LOCK)"
locked=1

secret() { od -An -tx1 -N32 /dev/urandom | tr -d ' \n'; }

# ---- first run: user, directories, settings, kernel ------------------------
if ! id "$APP" >/dev/null 2>&1; then
  say "creating system user $APP"
  as_root useradd --system --user-group --home-dir "/var/lib/$APP" --no-create-home \
    --shell /usr/sbin/nologin "$APP"
fi
as_root mkdir -p "$DATA" "$ETC"
as_root chown "$APP:$APP" "$DATA"
as_root chmod 0750 "$DATA" "$ETC"
first=
if [ ! -f "$ENV_FILE" ]; then
  first=1
  say "writing $ENV_FILE (yours to edit from now on)"
  cp "$HERE/env" "$HERE/env.new"
  for key in $SECRETS; do
    printf '%s=%s\n' "$key" "$(secret)" >>"$HERE/env.new"
  done
  as_root install -m 0640 "$HERE/env.new" "$ENV_FILE"
  as_root chgrp "$APP" "$ENV_FILE"
fi
if [ -s "$HERE/env.set" ]; then
  say "setting $(cut -d= -f1 "$HERE/env.set" | tr '\n' ' ')in $ENV_FILE"
  as_root cat "$ENV_FILE" >"$HERE/env.old"
  awk -F= 'NR == FNR { set[$1] = $0; next } !($1 in set) { print } END { for (k in set) print set[k] }' \
    "$HERE/env.set" "$HERE/env.old" >"$HERE/env.merged"
  as_root install -m 0640 "$HERE/env.merged" "$ENV_FILE"
  as_root chgrp "$APP" "$ENV_FILE"
fi
as_root mkdir -p "$(dirname "$UNIT_FILE")"
if [ -f "$UNIT_FILE" ]; then as_root cat "$UNIT_FILE" >"$HERE/unit.old"; fi
as_root install -m 0644 "$HERE/unit" "$UNIT_FILE"
if [ ! -f "$SYSCTL_FILE" ]; then
  printf '%s\n' "# zapline ship: connections queued on a stopping release go to the new one." \
    "net.ipv4.tcp_migrate_req = 1" >"$HERE/sysctl.conf"
  as_root mkdir -p "$(dirname "$SYSCTL_FILE")"
  as_root install -m 0644 "$HERE/sysctl.conf" "$SYSCTL_FILE"
fi
as_root sysctl -q -w net.ipv4.tcp_migrate_req=1 >/dev/null 2>&1 ||
  say "note: this kernel lacks net.ipv4.tcp_migrate_req (Linux < 5.14); a request may be reset during swaps"

# ---- the release to start --------------------------------------------------
previous=$(readlink "$BASE/current" 2>/dev/null || true)
previous=${previous##*/}
if [ "$MODE" = rollback ]; then
  RELEASE=$(ls -1 "$BASE/releases" | sort | awk -v cur="$previous" 'cur == "" || $0 < cur' | tail -n 1)
  [ -n "$RELEASE" ] || fail "no release older than ${previous:-the current one} to roll back to"
  [ -x "$BASE/releases/$RELEASE/$BIN" ] || fail "release $RELEASE has no $BIN binary"
  say "rolling back from ${previous:-nothing} to $RELEASE"
else
  if [ -n "$SHA256" ] && command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$SHA256" "$HERE/$BIN" | sha256sum -c - >/dev/null 2>&1 ||
      fail "the uploaded binary is corrupted (checksum mismatch)"
  fi
  as_root mkdir -p "$BASE/releases/$RELEASE"
  as_root install -m 0755 "$HERE/$BIN" "$BASE/releases/$RELEASE/$BIN"
fi
NEW=$APP@$RELEASE.service
OLD=$(systemctl list-units --type=service --state=active --plain --no-legend "$APP@*.service" 2>/dev/null |
  awk '{ print $1 }' | grep -vxF "$NEW" || true)

# Where the end-to-end check connects: the port the app listens on.
port=$(as_root sed -n 's/^PORT=//p' "$ENV_FILE" | tail -n 1)
addr=$(as_root sed -n 's/^ADDR=//p' "$ENV_FILE" | tail -n 1)
host=127.0.0.1
if [ -n "$addr" ]; then
  port=${addr##*:}
  case ${addr%:*} in
    '' | 0.0.0.0 | '[::]' | '::') ;;
    *) host=${addr%:*} ;;
  esac
fi
port=${port:-$DEFAULT_PORT}
url=http://$host:$port$HEALTH
probe() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsS -o /dev/null --max-time 5 "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O /dev/null -T 5 "$url"
  else
    return 0
  fi
}

# Undo a release that never took over: the old one keeps serving.
abandon() {
  as_root journalctl -u "$NEW" -n 40 --no-pager -o cat 2>/dev/null || true
  as_root systemctl stop "$NEW" >/dev/null 2>&1 || true
  as_root systemctl reset-failed "$NEW" >/dev/null 2>&1 || true
  if [ "$MODE" = deploy ]; then as_root rm -rf "$BASE/releases/$RELEASE"; fi
  # The running release restarts with the unit it started with.
  if [ -f "$HERE/unit.old" ]; then
    as_root install -m 0644 "$HERE/unit.old" "$UNIT_FILE"
    as_root systemctl daemon-reload || true
  fi
  fail "$1; still serving: ${OLD:-nothing}"
}

# ---- swap ------------------------------------------------------------------
as_root systemctl daemon-reload
say "starting $NEW (ready when GET $HEALTH succeeds)"
as_root systemctl start "$NEW" || abandon "release $RELEASE did not become ready"
ok=
for attempt in 1 2 3 4 5; do
  if probe; then ok=1; break; fi
  sleep 1
done
[ -n "$ok" ] || abandon "release $RELEASE started, but $url fails"

as_root systemctl enable --quiet "$NEW"
as_root ln -sfn "releases/$RELEASE" "$BASE/current"
for unit in $OLD; do
  say "stopping $unit (it finishes its requests first)"
  as_root systemctl disable --quiet "$unit" || true
  as_root systemctl stop "$unit" || true
done
if ! systemctl is-active --quiet "$NEW"; then
  say "$NEW exited right after taking over; restarting ${OLD:-nothing}"
  for unit in $OLD; do
    as_root systemctl enable --quiet "$unit" || true
    as_root systemctl start "$unit" || true
  done
  if [ -n "$previous" ]; then as_root ln -sfn "releases/$previous" "$BASE/current"; fi
  fail "release $RELEASE failed after the swap (see: journalctl -u $NEW)"
fi

# ---- keep the newest $KEEP releases -----------------------------------------
ls -1 "$BASE/releases" | sort -r | tail -n +"$((KEEP + 1))" | while read -r old; do
  [ "$old" = "$RELEASE" ] && continue
  systemctl is-active --quiet "$APP@$old.service" && continue
  as_root rm -rf "$BASE/releases/$old"
done

say "release $RELEASE is live on port $port"
if [ -n "$first" ]; then
  say "settings: $ENV_FILE    logs: journalctl -u '$APP@*' -f"
fi
"##;

/// Builds the plan for `opts` in `project`.
///
/// # Errors
///
/// Bad options, a project the CLI can't read, or no way to build for the
/// server.
pub fn plan(
    project: &Project,
    opts: &ShipOptions,
    tools: &Toolchain,
    remote_arch: Option<&str>,
    release: &str,
) -> Result<Plan> {
    if opts.host.trim().is_empty() || opts.host.starts_with('-') {
        bail!("expected the server as user@host");
    }
    let bin = crate::build::resolve_bin(project, opts.bin.as_deref())?;
    let app = service_name(opts.name.as_deref().unwrap_or(&project.package_name))?;
    let facts = Facts::read(project);
    let health = match &opts.health {
        Some(h) if h.starts_with('/') => h.clone(),
        Some(h) => bail!("--health {h}: expected a path such as /health/ready"),
        None if facts.health => "/health/ready".to_owned(),
        None => "/".to_owned(),
    };
    if health.contains(|c: char| c.is_whitespace() || c == '\'') {
        bail!("--health {health}: no spaces or quotes");
    }
    let env_set = parse_env_settings(&opts.env)?;
    let build = if opts.rollback {
        None
    } else {
        Some(plan_build(opts, tools, remote_arch)?)
    };
    let release = if opts.rollback {
        String::new()
    } else {
        release.to_owned()
    };
    let inputs = Inputs {
        app: app.clone(),
        bin: bin.clone(),
        release: release.clone(),
        port: opts.port,
        health: health.clone(),
        keep: opts.keep,
        sha256: String::new(),
        facts,
        env_example: fs::read_to_string(project.root.join(".env.example")).unwrap_or_default(),
        env_set: env_set.clone(),
    };
    let files = files(&inputs, opts.rollback);
    let tag = if release.is_empty() {
        "rollback"
    } else {
        &release
    };
    let remote_dir = format!("/tmp/zapline-ship.{app}.{tag}.{}", std::process::id());
    let (ssh, scp) = ssh_options(opts);
    Ok(Plan {
        app,
        bin,
        release,
        build,
        health,
        remote_dir,
        files,
        ssh,
        scp,
        host: opts.host.clone(),
        embed_stamp: embed_stamp(&project.root),
    })
}

/// Renders the uploaded files from `inputs`.
pub fn files(inputs: &Inputs, rollback: bool) -> Files {
    let env_set: String = inputs
        .env_set
        .iter()
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect();
    Files {
        script: script(inputs, rollback),
        unit: unit_file(inputs),
        env: env_file(inputs),
        env_set,
    }
}

/// `ssh` and `scp` options: one shared connection for the whole run, plus
/// the port and identity asked for.
pub fn ssh_options(opts: &ShipOptions) -> (Vec<String>, Vec<String>) {
    let mut common = vec![
        "-o".to_owned(),
        "ControlMaster=auto".to_owned(),
        "-o".to_owned(),
        "ControlPath=~/.ssh/zapline-%C".to_owned(),
        "-o".to_owned(),
        "ControlPersist=60".to_owned(),
    ];
    if let Some(id) = &opts.identity {
        common.push("-i".to_owned());
        common.push(id.display().to_string());
    }
    let mut ssh = common.clone();
    let mut scp = common;
    if let Some(p) = opts.ssh_port {
        ssh.extend(["-p".to_owned(), p.to_string()]);
        scp.extend(["-P".to_owned(), p.to_string()]);
    }
    (ssh, scp)
}

/// One command of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What it is for, shown before it runs.
    pub what: String,
    /// Environment set for it.
    pub env: Vec<(String, String)>,
    /// Program and arguments.
    pub argv: Vec<String>,
}

impl Step {
    /// The command as one shell line.
    pub fn shell(&self) -> String {
        let mut parts: Vec<String> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", sh_quote(v)))
            .collect();
        parts.extend(self.argv.iter().map(|a| sh_quote(a)));
        parts.join(" ")
    }
}

impl Plan {
    /// The cargo command, if building.
    pub fn build_step(&self) -> Option<Step> {
        let b = self.build.as_ref()?;
        let mut argv = vec![
            "cargo".to_owned(),
            b.subcommand.to_owned(),
            "--release".to_owned(),
            "--bin".to_owned(),
            self.bin.clone(),
        ];
        if let Some(t) = &b.target {
            argv.extend(["--target".to_owned(), t.clone()]);
        }
        let mut env = vec![(ENV_EMBED_STAMP.to_owned(), self.embed_stamp.clone())];
        env.extend(b.env.iter().cloned());
        Some(Step {
            what: "Building".to_owned(),
            env,
            argv,
        })
    }

    fn ssh(&self, what: &str, tty: bool, remote: String) -> Step {
        let mut argv = vec!["ssh".to_owned()];
        argv.extend(self.ssh.iter().cloned());
        if tty {
            argv.push("-t".to_owned());
        }
        argv.push(self.host.clone());
        argv.push(remote);
        Step {
            what: what.to_owned(),
            env: Vec::new(),
            argv,
        }
    }

    /// Asks the server for its CPU architecture.
    pub fn probe_step(&self) -> Step {
        self.ssh("Checking", false, "uname -sm".to_owned())
    }

    /// Every remote command, in order, with `binary` as the local path of
    /// the built executable and `staging` the local directory holding the
    /// generated files.
    pub fn remote_steps(&self, binary: &Path, staging: &Path, tty: bool) -> Vec<Step> {
        let dir = &self.remote_dir;
        let mut steps = vec![self.ssh(
            "Preparing",
            false,
            format!("mkdir -m 700 {}", sh_quote(dir)),
        )];
        let mut scp = vec!["scp".to_owned(), "-q".to_owned(), "-C".to_owned()];
        scp.extend(self.scp.iter().cloned());
        if self.build.is_some() {
            scp.push(binary.display().to_string());
        }
        for name in ["ship.sh", "unit", "env", "env.set"] {
            scp.push(staging.join(name).display().to_string());
        }
        scp.push(format!("{}:{}/", self.host, dir));
        steps.push(Step {
            what: "Uploading".to_owned(),
            env: Vec::new(),
            argv: scp,
        });
        steps.push(self.ssh(
            if self.build.is_some() {
                "Swapping"
            } else {
                "Rolling back"
            },
            tty,
            format!("sh {}/ship.sh", sh_quote(dir)),
        ));
        steps
    }

    /// The generated files, with their content (for `--dry-run`).
    pub fn file_listing(&self) -> Vec<(&'static str, &str)> {
        vec![
            ("ship.sh", self.files.script.as_str()),
            ("unit", self.files.unit.as_str()),
            ("env", self.files.env.as_str()),
            ("env.set", self.files.env_set.as_str()),
        ]
    }
}

/// The dry-run output: every command, then every file.
pub fn describe(plan: &Plan, binary: &Path, staging: &Path) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# zapline ship --dry-run: {app} {what} on {host}\n# Nothing below has run.\n",
        app = plan.app,
        what = if plan.build.is_some() {
            format!("release {}", plan.release)
        } else {
            "rollback".to_owned()
        },
        host = plan.host,
    );
    if plan.build.is_some() {
        out.push_str(
            "# The server's CPU (from `uname -sm`) picks the build target, and\n\
             # SHA256 in ship.sh is set to the built binary's checksum.\n\n",
        );
    }
    let mut steps = Vec::new();
    if plan.build.is_some() {
        steps.push(plan.probe_step());
    }
    steps.extend(plan.build_step());
    steps.extend(plan.remote_steps(binary, staging, false));
    for s in &steps {
        let _ = writeln!(out, "# {}\n{}\n", s.what, s.shell());
    }
    for (name, text) in plan.file_listing() {
        let _ = writeln!(
            out,
            "# ---- {} ----\n{}",
            staging.join(name).display(),
            if text.is_empty() { "(empty)\n" } else { text }
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Running it

/// `zapline ship`.
///
/// # Errors
///
/// Any step that fails; the server keeps serving the old release.
pub fn run(project: &Project, opts: &ShipOptions) -> Result<()> {
    let tools = Toolchain::detect();
    let release = release_id(now_secs(), git_commit(&project.root).as_deref());
    if opts.dry_run {
        let plan = plan(project, opts, &tools, None, &release)?;
        let staging = staging_dir(&plan);
        let binary = expected_binary(project, &plan);
        print!("{}", describe(&plan, &binary, &staging));
        return Ok(());
    }

    ensure_ssh_dir();
    // The server's CPU decides what to build.
    let remote_arch = if opts.rollback {
        None
    } else {
        let probe = plan(
            project,
            &ShipOptions {
                rollback: true,
                ..opts.clone()
            },
            &tools,
            None,
            &release,
        )?
        .probe_step();
        ui::status("Checking", format!("{} (ssh)", opts.host));
        let out = output(&probe)?;
        let mut words = out.split_whitespace();
        let (os, arch) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
        if os != "Linux" {
            bail!(
                "{} runs {os:?}; zapline ship installs on Linux with systemd",
                opts.host
            );
        }
        Some(rust_arch(arch))
    };
    let mut plan = plan(project, opts, &tools, remote_arch.as_deref(), &release)?;

    let binary = match plan.build_step() {
        Some(step) => {
            if project.root.join(crate::tailwind::INPUT).is_file() {
                ui::status("Styling", crate::tailwind::OUTPUT);
                crate::tailwind::build(&project.root, true)?;
            }
            ui::status(
                "Building",
                format!(
                    "{} (release{})",
                    plan.bin,
                    plan.build
                        .as_ref()
                        .and_then(|b| b.target.as_deref())
                        .map(|t| format!(", {t}"))
                        .unwrap_or_default()
                ),
            );
            build(project, &step, &plan.bin)?
        }
        None => PathBuf::new(),
    };
    if plan.build.is_some() {
        let sha = crate::tailwind::sha256_file(&binary)?;
        plan.files.script =
            plan.files
                .script
                .replacen("\nSHA256=''\n", &format!("\nSHA256={sha}\n"), 1);
    }

    let staging = create_staging(&staging_dir(&plan))?;
    for (name, text) in plan.file_listing() {
        write_private(&staging.join(name), text)?;
    }
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let result = (|| {
        for step in plan.remote_steps(&binary, &staging, tty) {
            ui::status(&step.what, describe_remote(&step, &plan));
            run_step(&step)?;
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

fn describe_remote(step: &Step, plan: &Plan) -> String {
    match step.what.as_str() {
        "Uploading" => format!("{} -> {}:{}", plan.bin, plan.host, plan.remote_dir),
        "Preparing" => format!("{}:{}", plan.host, plan.remote_dir),
        _ => format!("{} on {}", plan.app, plan.host),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The short commit hash, when the project is a git checkout.
fn git_commit(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn staging_dir(plan: &Plan) -> PathBuf {
    let tag = if plan.release.is_empty() {
        "rollback"
    } else {
        &plan.release
    };
    std::env::temp_dir().join(format!(
        "zapline-stage.{}.{tag}.{}",
        plan.app,
        std::process::id()
    ))
}

/// Creates a fresh, private directory named `stem` plus a random suffix:
/// mode 0700 on unix, and never one that already exists, since `stem` is
/// predictable and the temp dir is shared. The files in it hold `--env`
/// secrets and the script the server runs.
fn create_staging(stem: &Path) -> Result<PathBuf> {
    use std::hash::{BuildHasher, Hasher};
    let mut last = None;
    for attempt in 0u32..16 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(attempt);
        h.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        let mut name = stem.as_os_str().to_owned();
        name.push(format!(".{:016x}", h.finish()));
        let path = PathBuf::from(name);
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(path),
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", path.display())),
        }
    }
    bail!(
        "cannot create a fresh staging directory (last tried {})",
        last.map(|p| p.display().to_string()).unwrap_or_default()
    )
}

/// Writes a new file readable only by the owner (0600 on unix), refusing to
/// follow or overwrite anything already at `path`.
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .with_context(|| format!("cannot write {}", path.display()))
}

/// Where cargo will put the binary (for `--dry-run`, which doesn't build).
fn expected_binary(project: &Project, plan: &Plan) -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| project.root.join("target"));
    let mut path = target_dir;
    if let Some(t) = plan.build.as_ref().and_then(|b| b.target.as_ref()) {
        path.push(t);
    }
    path.join("release").join(&plan.bin)
}

/// `ControlPath=~/.ssh/...` needs the directory.
fn ensure_ssh_dir() {
    if let Some(home) = std::env::var_os("HOME") {
        let dir = Path::new(&home).join(".ssh");
        if !dir.exists() && fs::create_dir_all(&dir).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
            }
        }
    }
}

fn command(step: &Step) -> Command {
    let (program, args) = step
        .argv
        .split_first()
        .expect("a step always has a program");
    let mut cmd = if program == "cargo" {
        cargo()
    } else {
        Command::new(program)
    };
    cmd.args(args);
    for (k, v) in &step.env {
        cmd.env(k, v);
    }
    cmd
}

fn run_step(step: &Step) -> Result<()> {
    let status = command(step)
        .status()
        .with_context(|| format!("cannot run `{}` (is it installed?)", step.argv[0]))?;
    if !status.success() {
        bail!("`{}` failed ({status})", step.shell());
    }
    Ok(())
}

fn output(step: &Step) -> Result<String> {
    let out = command(step)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("cannot run `{}` (is it installed?)", step.argv[0]))?;
    if !out.status.success() {
        bail!("`{}` failed ({})", step.shell(), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs the cargo step and returns the executable it produced.
fn build(project: &Project, step: &Step, bin: &str) -> Result<PathBuf> {
    let mut cmd = command(step);
    cmd.arg("--message-format=json-render-diagnostics")
        .current_dir(&project.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if ui::stderr_color() {
        cmd.args(["--color", "always"]);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot run `{}`", step.shell()))?;
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
        bail!("the release build failed; nothing was shipped");
    }
    executable.ok_or_else(|| anyhow!("cargo did not report an executable for `{bin}`"))
}

#[cfg(test)]
mod tests;
