//! `zapline ship`: the build decision, the generated files, the dry-run
//! output, and the server script run against fake `systemctl`/`useradd`
//! programs in a scratch root.

use super::*;

fn linux_x86(programs: &[&str]) -> Toolchain {
    Toolchain {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        installed_targets: Some(vec![
            "x86_64-unknown-linux-gnu".to_owned(),
            "x86_64-unknown-linux-musl".to_owned(),
        ]),
        programs: programs.iter().map(|p| (*p).to_owned()).collect(),
    }
}

fn opts(host: &str) -> ShipOptions {
    ShipOptions {
        host: host.to_owned(),
        ..ShipOptions::default()
    }
}

/// A project directory with `Cargo.toml`, `src/main.rs` and whatever else
/// is passed, as `(relative path, contents)`.
fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Project) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = "[package]\nname = \"Todo_App\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                    [dependencies]\nzapline = { version = \"0.1\", features = [\"sqlite\"] }\n";
    fs::write(dir.path().join("Cargo.toml"), manifest).unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    for (rel, text) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let project = Project::parse(dir.path(), manifest).unwrap().unwrap();
    (dir, project)
}

#[test]
fn native_builds_are_plain_cargo_build() {
    let b = plan_build(&opts("h"), &linux_x86(&[]), Some("x86_64")).unwrap();
    assert_eq!(
        b,
        BuildPlan {
            subcommand: "build",
            target: None,
            env: Vec::new(),
            arch: "x86_64".to_owned()
        }
    );
}

#[test]
fn static_builds_use_musl_gcc_when_installed() {
    let o = ShipOptions {
        static_build: true,
        ..opts("h")
    };
    let b = plan_build(&o, &linux_x86(&["musl-gcc"]), Some("x86_64")).unwrap();
    assert_eq!(b.target.as_deref(), Some("x86_64-unknown-linux-musl"));
    assert_eq!(b.subcommand, "build");
    assert_eq!(
        b.env,
        [(
            "CC_x86_64_unknown_linux_musl".to_owned(),
            "musl-gcc".to_owned()
        )]
    );
}

#[test]
fn static_builds_explain_what_to_install() {
    let o = ShipOptions {
        static_build: true,
        ..opts("h")
    };
    let err = plan_build(&o, &linux_x86(&[]), Some("x86_64"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("sudo apt install musl-tools"), "{err}");
    assert!(err.contains("cargo-zigbuild"), "{err}");

    // The Rust target itself missing comes first, with the rustup command.
    let mut tools = linux_x86(&["musl-gcc"]);
    tools.installed_targets = Some(vec!["x86_64-unknown-linux-gnu".to_owned()]);
    let err = plan_build(&o, &tools, Some("x86_64"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("rustup target add x86_64-unknown-linux-musl"),
        "{err}"
    );
}

#[test]
fn cross_builds_use_a_cross_compiler_or_zigbuild() {
    let o = ShipOptions {
        static_build: true,
        ..opts("h")
    };
    let mut tools = linux_x86(&["aarch64-linux-musl-gcc"]);
    tools.installed_targets = Some(vec!["aarch64-unknown-linux-musl".to_owned()]);
    let b = plan_build(&o, &tools, Some("aarch64")).unwrap();
    assert_eq!(b.target.as_deref(), Some("aarch64-unknown-linux-musl"));
    assert!(b.env.contains(&(
        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER".to_owned(),
        "aarch64-linux-musl-gcc".to_owned()
    )));

    tools.programs = vec!["cargo-zigbuild".to_owned(), "zig".to_owned()];
    let b = plan_build(&o, &tools, Some("aarch64")).unwrap();
    assert_eq!(b.subcommand, "zigbuild");
    assert!(b.env.is_empty());
}

#[test]
fn mismatched_machines_are_explained() {
    // A Mac building for Linux.
    let mut mac = linux_x86(&[]);
    mac.os = "macos".to_owned();
    let err = plan_build(&opts("web1"), &mac, Some("x86_64"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("zapline ship web1 --static"), "{err}");

    // An x86 laptop and an ARM server.
    let err = plan_build(&opts("web1"), &linux_x86(&[]), Some("aarch64"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("the server's CPU is aarch64"), "{err}");

    // --target for the wrong CPU.
    let o = ShipOptions {
        target: Some("x86_64-unknown-linux-gnu".to_owned()),
        ..opts("h")
    };
    let err = plan_build(&o, &linux_x86(&[]), Some("aarch64"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("builds for x86_64"), "{err}");
}

#[test]
fn names_ids_and_settings() {
    assert_eq!(rust_arch("amd64\n"), "x86_64");
    assert_eq!(rust_arch("arm64"), "aarch64");
    assert_eq!(rust_arch("x86_64"), "x86_64");

    assert_eq!(service_name("Todo_App").unwrap(), "todo_app");
    assert_eq!(service_name("my.app").unwrap(), "my-app");
    assert!(service_name("9lives").is_err());

    assert_eq!(release_id(0, Some("abc1234\n")), "19700101000000-abc1234");
    assert_eq!(release_id(86_400, None), "19700102000000");
    assert_eq!(release_id(0, Some("a b")), "19700101000000");

    let set = parse_env_settings(&["A=1".into(), "B=x=y".into(), "A=2".into()]).unwrap();
    assert_eq!(set, [("B".into(), "x=y".into()), ("A".into(), "2".into())]);
    assert!(parse_env_settings(&["NOPE".into()]).is_err());
    assert!(parse_env_settings(&["1A=x".into()]).is_err());
    assert!(parse_env_settings(&["A-B=x".into()]).is_err());

    assert_eq!(sh_quote("abc/def"), "abc/def");
    assert_eq!(sh_quote(""), "''");
    assert_eq!(sh_quote("it's"), r"'it'\''s'");
}

#[test]
fn embed_stamp_changes_when_files_are_added() {
    let (dir, _) = project(&[("templates/a.html", "a")]);
    let before = embed_stamp(dir.path());
    fs::write(dir.path().join("templates/a.html"), "changed").unwrap();
    assert_eq!(embed_stamp(dir.path()), before, "contents are cargo's job");
    fs::write(dir.path().join("templates/b.html"), "b").unwrap();
    assert_ne!(embed_stamp(dir.path()), before);
    // The build output doesn't count.
    let after = embed_stamp(dir.path());
    fs::create_dir_all(dir.path().join("target/release")).unwrap();
    fs::write(dir.path().join("target/release/x"), "x").unwrap();
    assert_eq!(embed_stamp(dir.path()), after);
}

fn inputs(port: u16) -> Inputs {
    Inputs {
        app: "todo".to_owned(),
        bin: "todo".to_owned(),
        release: "20260101000000-abc1234".to_owned(),
        port,
        health: "/health/ready".to_owned(),
        keep: 3,
        sha256: String::new(),
        facts: Facts {
            name: "todo".to_owned(),
            sqlite: true,
            postgres: false,
            redis: false,
            health: true,
            reads_port: true,
            secrets: vec!["SESSION_KEY".to_owned()],
        },
        env_example: "PORT=8080\nSESSION_KEY=change-me\n# SMTP_HOST=localhost\n".to_owned(),
        env_set: Vec::new(),
    }
}

#[test]
fn the_unit_is_a_notify_template_with_reuse_port() {
    let unit = unit_file(&inputs(8080));
    for line in [
        "Type=notify",
        "ExecStart=/opt/todo/releases/%i/todo",
        "EnvironmentFile=/etc/todo/env",
        "Environment=ZAPLINE_READY_PATH=/health/ready",
        "Environment=ZAPLINE_REUSE_PORT=1",
        "User=todo",
        "StateDirectory=todo",
        "WorkingDirectory=/var/lib/todo",
        "ProtectSystem=strict",
        "WantedBy=multi-user.target",
    ] {
        assert!(unit.lines().any(|l| l == line), "missing {line}:\n{unit}");
    }
    assert!(!unit.contains("CAP_NET_BIND_SERVICE"));
    assert!(unit_file(&inputs(80)).contains("AmbientCapabilities=CAP_NET_BIND_SERVICE"));
}

#[test]
fn the_env_file_has_the_port_database_and_documented_settings() {
    let env = env_file(&inputs(8080));
    assert!(env.contains("PORT=8080\n"));
    assert!(env.contains("APP_ENV=production\n"));
    assert!(env.contains("DATABASE_URL=sqlite:///var/lib/todo/app.db?mode=rwc\n"));
    assert!(env.contains("# SMTP_HOST=localhost\n"));
    // Secrets are generated on the server, never written here.
    assert!(!env.contains("SESSION_KEY"));
    let script = script(&inputs(8080), false);
    assert!(script.contains("\nSECRETS=SESSION_KEY\n"));

    let mut pg = inputs(8080);
    pg.facts.sqlite = false;
    pg.facts.postgres = true;
    assert!(env_file(&pg).contains("# DATABASE_URL=postgres://"));

    // A secret set with --env isn't generated.
    let mut set = inputs(8080);
    set.env_set = vec![("SESSION_KEY".to_owned(), "mine".to_owned())];
    assert!(script_of(&set).contains("\nSECRETS=''\n"));
}

fn script_of(i: &Inputs) -> String {
    script(i, false)
}

#[test]
fn the_script_is_valid_posix_sh() {
    for rollback in [false, true] {
        let s = script(&inputs(8080), rollback);
        let out = Command::new("sh")
            .args(["-n", "-c", &s])
            .output()
            .expect("sh runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn dry_run_shows_every_command_and_file() {
    let (dir, project) = project(&[
        ("src/routes.rs", "fn r() { Health::new(); }"),
        (".env.example", "SESSION_KEY=x\n"),
        ("templates/index.html", "hi"),
    ]);
    let o = ShipOptions {
        env: vec!["SMTP_HOST=mail".to_owned()],
        ssh_port: Some(2222),
        identity: Some(PathBuf::from("/keys/deploy")),
        ..opts("deploy@web1")
    };
    let plan = plan(
        &project,
        &o,
        &linux_x86(&[]),
        None,
        "20260101000000-abc1234",
    )
    .unwrap();
    assert_eq!(plan.app, "todo_app");
    assert_eq!(plan.health, "/health/ready");
    let text = describe(
        &plan,
        Path::new("/t/release/Todo_App"),
        Path::new("/tmp/stage"),
    );
    let stamp = embed_stamp(dir.path());
    for want in [
        "# zapline ship --dry-run: todo_app release 20260101000000-abc1234 on deploy@web1",
        "ssh -o ControlMaster=auto -o 'ControlPath=~/.ssh/zapline-%C' -o ControlPersist=60 -i /keys/deploy -p 2222 deploy@web1 'uname -sm'",
        &format!("ZAPLINE_EMBED_STAMP={stamp} cargo build --release --bin Todo_App"),
        "'mkdir -m 700 /tmp/zapline-ship.todo_app.20260101000000-abc1234.",
        "scp -q -C -o ControlMaster=auto -o 'ControlPath=~/.ssh/zapline-%C' -o ControlPersist=60 -i /keys/deploy -P 2222 /t/release/Todo_App /tmp/stage/ship.sh /tmp/stage/unit /tmp/stage/env /tmp/stage/env.set deploy@web1:/tmp/zapline-ship.todo_app.",
        "deploy@web1 'sh /tmp/zapline-ship.todo_app.",
        "# ---- /tmp/stage/ship.sh ----\n#!/bin/sh\n",
        "# ---- /tmp/stage/unit ----\n",
        "Type=notify",
        "# ---- /tmp/stage/env.set ----\nSMTP_HOST=mail\n",
        "\nSECRETS=SESSION_KEY\n",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }

    // A rollback builds and uploads no binary.
    let o = ShipOptions {
        rollback: true,
        ..opts("deploy@web1")
    };
    let plan = super::plan(&project, &o, &linux_x86(&[]), None, "ignored").unwrap();
    let text = describe(&plan, Path::new("/t/release/Todo_App"), Path::new("/s"));
    assert!(!text.contains("cargo"), "{text}");
    assert!(!text.contains("/t/release/Todo_App"), "{text}");
    assert!(text.contains("# Rolling back\n"), "{text}");
    assert!(text.contains("\nMODE=rollback\n"), "{text}");
}

#[test]
fn bad_options_are_rejected() {
    let (_dir, project) = project(&[]);
    let tools = linux_x86(&[]);
    let try_plan = |o: ShipOptions| plan(&project, &o, &tools, None, "r").map(|_| ());
    assert!(try_plan(opts("")).is_err());
    assert!(try_plan(opts("-oProxyCommand=x")).is_err());
    assert!(try_plan(ShipOptions {
        health: Some("health".into()),
        ..opts("h")
    })
    .is_err());
    assert!(try_plan(ShipOptions {
        bin: Some("other".into()),
        ..opts("h")
    })
    .is_err());
    assert!(try_plan(ShipOptions {
        env: vec!["A=b\nc".into()],
        ..opts("h")
    })
    .is_err());
    // Without the Health plugin the root path is the check.
    let plan = plan(&project, &opts("h"), &tools, None, "r").unwrap();
    assert_eq!(plan.health, "/");
}

// ---------------------------------------------------------------------------
// The server script, against fake system programs

/// A scratch server: `root` stands for `/`, `bin` holds fake `systemctl`,
/// `useradd`, `id`, ... that record what they were asked in `state`.
struct Server {
    _dir: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    state: PathBuf,
    uploads: usize,
}

const FAKES: &[(&str, &str)] = &[
    (
        "id",
        r#"if [ "$1" = -u ]; then echo 0; exit 0; fi
[ -f "$SHIM_STATE/user-$1" ]"#,
    ),
    (
        "useradd",
        r#"for a; do n=$a; done
echo "useradd $*" >>"$SHIM_STATE/log"
touch "$SHIM_STATE/user-$n""#,
    ),
    ("chown", "exit 0"),
    ("chgrp", "exit 0"),
    ("journalctl", "echo \"journal for $2\""),
    ("sysctl", "echo \"sysctl $*\" >>\"$SHIM_STATE/log\""),
    ("curl", "exit ${SHIM_CURL_EXIT:-0}"),
    (
        "systemctl",
        r#"echo "systemctl $*" >>"$SHIM_STATE/log"
mkdir -p "$SHIM_STATE/active"
for a; do last=$a; done
case $1 in
  list-units) ls "$SHIM_STATE/active" | while read -r u; do echo "$u loaded active running app"; done ;;
  start)
    case " ${SHIM_FAIL_START:-} " in *" $2 "*) exit 1 ;; esac
    touch "$SHIM_STATE/active/$2" ;;
  stop) rm -f "$SHIM_STATE/active/$2" ;;
  is-active)
    case " ${SHIM_DIES:-} " in *" $last "*) rm -f "$SHIM_STATE/active/$last"; exit 3 ;; esac
    [ -f "$SHIM_STATE/active/$last" ]; exit ;;
esac
exit 0"#,
    ),
];

struct Outcome {
    ok: bool,
    output: String,
}

impl Server {
    fn new() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let bin = dir.path().join("bin");
        let state = dir.path().join("state");
        for d in [&root, &bin, &state] {
            fs::create_dir_all(d).unwrap();
        }
        for (name, body) in FAKES {
            let path = bin.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        Server {
            _dir: dir,
            root,
            bin,
            state,
            uploads: 0,
        }
    }

    /// Uploads the files for `i` as `zapline ship` would and runs the script.
    fn ship(&mut self, i: &Inputs, rollback: bool, env: &[(&str, &str)]) -> Outcome {
        self.uploads += 1;
        let upload = self
            .root
            .join(format!("tmp/zapline-ship.{}.{}", i.app, self.uploads));
        fs::create_dir_all(&upload).unwrap();
        let files = files(i, rollback);
        for (name, text) in [
            ("ship.sh", &files.script),
            ("unit", &files.unit),
            ("env", &files.env),
            ("env.set", &files.env_set),
        ] {
            fs::write(upload.join(name), text).unwrap();
        }
        if !rollback {
            fs::write(upload.join(&i.bin), format!("binary {}", i.release)).unwrap();
        }
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("sh");
        cmd.arg(upload.join("ship.sh"))
            .env("PATH", path)
            .env("ZAPLINE_SHIP_ROOT", &self.root)
            .env("SHIM_STATE", &self.state);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(!upload.exists(), "the upload directory is removed");
        Outcome {
            ok: out.status.success(),
            output: format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        }
    }

    fn active(&self) -> Vec<String> {
        let mut units: Vec<String> = fs::read_dir(self.state.join("active"))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        units.sort();
        units
    }

    fn releases(&self) -> Vec<String> {
        let mut r: Vec<String> = fs::read_dir(self.root.join("opt/todo/releases"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        r.sort();
        r
    }

    fn current(&self) -> String {
        fs::read_link(self.root.join("opt/todo/current"))
            .unwrap()
            .display()
            .to_string()
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.root.join(rel)).unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.state.join("log")).unwrap_or_default()
    }
}

fn release(n: u32) -> Inputs {
    let mut i = inputs(8080);
    i.release = format!("2026010100000{n}-abc1234");
    i
}

fn sh_available() -> bool {
    cfg!(unix) && Command::new("sh").arg("-c").arg("true").status().is_ok()
}

#[test]
fn first_deploy_installs_everything() {
    if !sh_available() {
        return;
    }
    let mut server = Server::new();
    let mut first = release(1);
    first.sha256 = "0".repeat(64);
    // A wrong checksum stops before anything is started.
    let out = server.ship(&first, false, &[]);
    assert!(!out.ok, "{}", out.output);
    assert!(out.output.contains("checksum mismatch"), "{}", out.output);
    assert!(server.active().is_empty());

    first.sha256 = String::new();
    let out = server.ship(&first, false, &[]);
    assert!(out.ok, "{}", out.output);
    assert!(out
        .output
        .contains("release 20260101000001-abc1234 is live on port 8080"));
    assert!(server.log().contains("useradd --system"));
    assert!(server
        .log()
        .contains("sysctl -q -w net.ipv4.tcp_migrate_req=1"));
    assert_eq!(server.active(), ["todo@20260101000001-abc1234.service"]);
    assert_eq!(server.current(), "releases/20260101000001-abc1234");
    assert_eq!(
        server.read("opt/todo/releases/20260101000001-abc1234/todo"),
        "binary 20260101000001-abc1234"
    );
    assert_eq!(
        server.read("etc/systemd/system/todo@.service"),
        first_unit()
    );
    assert!(server
        .read("etc/sysctl.d/60-zapline-reuseport.conf")
        .contains("net.ipv4.tcp_migrate_req = 1"));
    let env = server.read("etc/todo/env");
    assert!(env.starts_with(&env_file(&first)));
    let secret = env
        .lines()
        .find_map(|l| l.strip_prefix("SESSION_KEY="))
        .expect("a generated secret");
    assert_eq!(secret.len(), 64, "{env}");
    assert!(!server.root.join("opt/todo/.ship-lock").exists());
}

fn first_unit() -> String {
    unit_file(&release(1))
}

#[test]
fn later_deploys_swap_and_keep_settings() {
    if !sh_available() {
        return;
    }
    let mut server = Server::new();
    assert!(server.ship(&release(1), false, &[]).ok);
    let env_before = server.read("etc/todo/env");

    let mut second = release(2);
    second.env_set = vec![
        ("SMTP_HOST".to_owned(), "mail.local".to_owned()),
        ("PORT".to_owned(), "9000".to_owned()),
    ];
    let out = server.ship(&second, false, &[]);
    assert!(out.ok, "{}", out.output);
    assert!(out
        .output
        .contains("stopping todo@20260101000001-abc1234.service"));
    assert!(
        out.output.contains("is live on port 9000"),
        "{}",
        out.output
    );
    assert_eq!(server.active(), ["todo@20260101000002-abc1234.service"]);
    assert_eq!(server.current(), "releases/20260101000002-abc1234");
    let env = server.read("etc/todo/env");
    assert!(env.contains("SMTP_HOST=mail.local\n"));
    assert!(env.contains("PORT=9000\n") && !env.contains("PORT=8080"));
    // The generated secret survives.
    let secret = |e: &str| {
        e.lines()
            .find(|l| l.starts_with("SESSION_KEY="))
            .map(str::to_owned)
    };
    assert_eq!(secret(&env), secret(&env_before));
    // The new release started before the old one stopped.
    let log = server.log();
    let start = log
        .find("systemctl start todo@20260101000002-abc1234.service")
        .unwrap();
    let stop = log
        .find("systemctl stop todo@20260101000001-abc1234.service")
        .unwrap();
    assert!(start < stop, "{log}");
}

#[test]
fn a_release_that_fails_to_start_leaves_the_old_one_serving() {
    if !sh_available() {
        return;
    }
    let mut server = Server::new();
    assert!(server.ship(&release(1), false, &[]).ok);
    let mut second = release(2);
    second.health = "/other".to_owned();
    let out = server.ship(
        &second,
        false,
        &[("SHIM_FAIL_START", "todo@20260101000002-abc1234.service")],
    );
    // The unit the running release started with is back.
    assert_eq!(
        server.read("etc/systemd/system/todo@.service"),
        first_unit()
    );
    assert!(!out.ok);
    assert!(
        out.output.contains("did not become ready")
            && out
                .output
                .contains("still serving: todo@20260101000001-abc1234.service"),
        "{}",
        out.output
    );
    assert!(out.output.contains("journal for todo@20260101000002"));
    assert_eq!(server.active(), ["todo@20260101000001-abc1234.service"]);
    assert_eq!(server.releases(), ["20260101000001-abc1234"]);
    assert_eq!(server.current(), "releases/20260101000001-abc1234");
    assert!(!server.root.join("opt/todo/.ship-lock").exists());

    // Started, but the port doesn't answer: also abandoned.
    let out = server.ship(&release(3), false, &[("SHIM_CURL_EXIT", "7")]);
    assert!(!out.ok);
    assert!(out
        .output
        .contains("started, but http://127.0.0.1:8080/health/ready fails"));
    assert_eq!(server.active(), ["todo@20260101000001-abc1234.service"]);
    assert_eq!(server.releases(), ["20260101000001-abc1234"]);
}

#[test]
fn a_release_that_dies_after_the_swap_brings_the_old_one_back() {
    if !sh_available() {
        return;
    }
    let mut server = Server::new();
    assert!(server.ship(&release(1), false, &[]).ok);
    let out = server.ship(
        &release(2),
        false,
        &[("SHIM_DIES", "todo@20260101000002-abc1234.service")],
    );
    assert!(!out.ok);
    assert!(
        out.output.contains("failed after the swap"),
        "{}",
        out.output
    );
    assert_eq!(server.active(), ["todo@20260101000001-abc1234.service"]);
    assert_eq!(server.current(), "releases/20260101000001-abc1234");
}

#[test]
fn rollback_starts_the_previous_release_and_old_ones_are_pruned() {
    if !sh_available() {
        return;
    }
    let mut server = Server::new();
    for n in 1..=4 {
        let out = server.ship(&release(n), false, &[]);
        assert!(out.ok, "{}", out.output);
    }
    // keep = 3
    assert_eq!(
        server.releases(),
        [
            "20260101000002-abc1234",
            "20260101000003-abc1234",
            "20260101000004-abc1234"
        ]
    );
    let mut back = inputs(8080);
    back.release = String::new();
    let out = server.ship(&back, true, &[]);
    assert!(out.ok, "{}", out.output);
    assert!(out
        .output
        .contains("rolling back from 20260101000004-abc1234 to 20260101000003-abc1234"));
    assert_eq!(server.active(), ["todo@20260101000003-abc1234.service"]);
    assert_eq!(server.current(), "releases/20260101000003-abc1234");
    // The release rolled back from stays, for a deploy forward again.
    assert_eq!(server.releases().len(), 3);

    let out = server.ship(&back, true, &[]);
    assert!(out.ok, "{}", out.output);
    assert_eq!(server.current(), "releases/20260101000002-abc1234");
    let out = server.ship(&back, true, &[]);
    assert!(!out.ok);
    assert!(out
        .output
        .contains("no release older than 20260101000002-abc1234"));
}

#[test]
fn concurrent_deploys_are_refused() {
    if !sh_available() {
        return;
    }
    let mut server = Server::new();
    fs::create_dir_all(server.root.join("opt/todo/.ship-lock")).unwrap();
    let out = server.ship(&release(1), false, &[]);
    assert!(!out.ok);
    assert!(out.output.contains("another deploy of todo is running"));
    assert!(
        server.root.join("opt/todo/.ship-lock").exists(),
        "not ours to remove"
    );
}

/// The staging directory is never one that already exists (its name is
/// predictable, in a shared temp dir), and on unix it and the secret files in
/// it are private to the deployer.
#[test]
fn staging_is_fresh_and_private() {
    let tmp = tempfile::tempdir().unwrap();
    let stem = tmp.path().join("zapline-stage.app.1.2");
    // Someone pre-creates the predictable name: it is not reused.
    fs::create_dir(&stem).unwrap();
    let a = create_staging(&stem).unwrap();
    let b = create_staging(&stem).unwrap();
    assert_ne!(a, stem);
    assert_ne!(a, b);
    assert!(fs::read_dir(&a).unwrap().next().is_none());

    write_private(&a.join("env.set"), "SECRET=x\n").unwrap();
    assert_eq!(fs::read_to_string(a.join("env.set")).unwrap(), "SECRET=x\n");
    // Never follows or overwrites what is already there.
    assert!(write_private(&a.join("env.set"), "other").is_err());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&a), 0o700);
        assert_eq!(mode(&a.join("env.set")), 0o600);
        let target = tmp.path().join("victim");
        fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, a.join("ship.sh")).unwrap();
        assert!(write_private(&a.join("ship.sh"), "evil").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    }
}
