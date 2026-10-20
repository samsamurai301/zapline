//! End-to-end tests of `routes`, `openapi`, `dev` and `bench`, driven against a
//! tiny dependency-free app that speaks zapline's env-var protocols.

mod common;

use std::fs;
use std::time::Duration;

use common::*;

const SLOW: Duration = Duration::from_secs(90);

// ---- routes ---------------------------------------------------------------------------

#[test]
fn routes_prints_an_aligned_table() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "routes-app");
    let out = run_in(&app, &["routes"]);
    expect_success(&out);
    assert_eq!(
        stdout(&out),
        "\
METHOD  PATH
GET     /health
POST    /users/{id}

2 routes
"
    );
}

#[test]
fn routes_json_passes_the_apps_output_through() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "routes-app");
    let out = run_in(&app, &["routes", "--json"]);
    expect_success(&out);
    assert_eq!(
        stdout(&out).trim(),
        r#"[{"method":"GET","path":"/health"},{"method":"POST","path":"/users/{id}"}]"#
    );
}

#[test]
fn routes_works_from_a_subdirectory_and_with_dash_c() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "routes-app");
    let out = run_in(&app.join("src"), &["routes", "--json"]);
    expect_success(&out);
    let out = run_in(
        tmp.path(),
        &["-C", app.to_str().unwrap(), "routes", "--json"],
    );
    expect_success(&out);
}

#[test]
fn routes_rejects_unknown_binaries() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "routes-app");
    let log = expect_failure(&run_in(&app, &["routes", "--bin", "nope"]));
    assert!(log.contains("no binary named `nope`"), "{log}");
    assert!(log.contains("routes-app"), "{log}");
}

#[test]
fn routes_reports_apps_that_print_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "silent-app");
    fs::write(app.join("src/main.rs"), "fn main() {}\n").unwrap();
    let log = expect_failure(&run_in(&app, &["routes"]));
    assert!(log.contains("did not print a route table"), "{log}");
    assert!(log.contains("App::run"), "{log}");
}

#[test]
fn routes_reports_build_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "broken-app");
    fs::write(app.join("src/main.rs"), "fn main() { this is not rust }\n").unwrap();
    let log = expect_failure(&run_in(&app, &["routes"]));
    assert!(log.contains("error"), "{log}");
    assert!(log.contains("build failed"), "{log}");
}

#[test]
fn routes_outside_a_project_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let log = expect_failure(&run_in(tmp.path(), &["routes"]));
    assert!(log.contains("Cargo.toml"), "{log}");
}

#[test]
fn routes_needs_a_bin_when_there_are_several() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "multi");
    fs::create_dir_all(app.join("src/bin")).unwrap();
    fs::write(
        app.join("src/bin/other.rs"),
        "fn main() { println!(\"[]\"); }\n",
    )
    .unwrap();
    let log = expect_failure(&run_in(&app, &["routes"]));
    assert!(log.contains("--bin"), "{log}");
    assert!(log.contains("multi, other"), "{log}");
    let out = run_in(&app, &["routes", "--bin", "other"]);
    assert_eq!(stdout(&out), "No routes registered.\n");
    expect_success(&out);
}

// ---- openapi ---------------------------------------------------------------------------

#[test]
fn openapi_writes_the_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "spec-app");
    let log = expect_success(&run_in(&app, &["openapi"]));
    assert!(log.contains("Wrote"), "{log}");
    assert!(log.contains("openapi.json"), "{log}");
    assert!(log.contains("1 paths, 2 operations"), "{log}");
    assert!(log.contains("fake v1.2.3"), "{log}");
    let spec: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(app.join("openapi.json")).unwrap()).unwrap();
    assert_eq!(spec["info"]["title"], "fake");
    // No temporary files are left behind.
    let leftovers: Vec<_> = fs::read_dir(&app)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("zapline-tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn openapi_output_path_is_relative_to_the_working_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "spec-app");
    expect_success(&run_in(&app, &["openapi", "-o", "docs/api/spec.json"]));
    assert!(app.join("docs/api/spec.json").is_file());

    let abs = tmp.path().join("elsewhere/spec.json");
    expect_success(&run_in(
        &app,
        &["openapi", "--output", abs.to_str().unwrap()],
    ));
    assert!(abs.is_file());
}

#[test]
fn failed_exports_never_clobber_the_existing_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let app = fake_app(tmp.path(), "spec-app");
    fs::write(app.join("openapi.json"), "precious").unwrap();

    // The app exits successfully without writing anything.
    fs::write(app.join("src/main.rs"), "fn main() {}\n").unwrap();
    let log = expect_failure(&run_in(&app, &["openapi"]));
    assert!(
        log.contains("without writing the OpenAPI document"),
        "{log}"
    );
    assert!(log.contains("App::openapi"), "{log}");
    assert_eq!(
        fs::read_to_string(app.join("openapi.json")).unwrap(),
        "precious"
    );

    // The app writes garbage.
    fs::write(
        app.join("src/main.rs"),
        r#"fn main() {
    if let Some(p) = std::env::var_os("ZAPLINE_EXPORT_OPENAPI") {
        std::fs::write(p, "not json at all").unwrap();
    }
}
"#,
    )
    .unwrap();
    let log = expect_failure(&run_in(&app, &["openapi"]));
    assert!(log.contains("not valid JSON"), "{log}");
    assert_eq!(
        fs::read_to_string(app.join("openapi.json")).unwrap(),
        "precious"
    );
    assert!(!app.join(".openapi.json.zapline-tmp").exists());

    // The app crashes.
    fs::write(
        app.join("src/main.rs"),
        "fn main() { std::process::exit(3); }\n",
    )
    .unwrap();
    let log = expect_failure(&run_in(&app, &["openapi"]));
    assert!(log.contains("exited with"), "{log}");
    assert_eq!(
        fs::read_to_string(app.join("openapi.json")).unwrap(),
        "precious"
    );
}

// ---- dev ---------------------------------------------------------------------------------

#[cfg(unix)]
mod dev {
    use super::*;
    use std::process::Command;

    fn dev_command(app: &std::path::Path, extra: &[&str]) -> Command {
        let mut cmd = zapline();
        cmd.arg("dev")
            .args(["--debounce-ms", "100"])
            .args(extra)
            .current_dir(app);
        cmd
    }

    #[test]
    fn rebuilds_and_restarts_on_change_then_shuts_down_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let app = fake_app(tmp.path(), "dev-app");
        let mut dev = Watched::spawn(dev_command(&app, &["--", "--port", "9000"]));

        dev.wait_for("Watching", SLOW);
        let running = dev.wait_for("Running", SLOW);
        let first = dev.wait_for("fake-app v1 running", SLOW);
        // Arguments after `--` reach the app.
        assert!(
            dev.seen[first].contains(r#"args=["--port", "9000"]"#),
            "{:?}",
            dev.seen[first]
        );
        let old_pid = pid_from_line(&dev.seen[running]).expect("pid in Running line");
        assert!(process_alive(old_pid));

        // Edit the source: the app is rebuilt and restarted.
        write_fake_main(&app, "v2");
        dev.wait_for("fake-app v2 running", SLOW);
        let stopped = dev.wait_for("Stopping", SLOW);
        assert!(
            dev.seen[stopped].contains(&old_pid.to_string()),
            "{:?}",
            dev.seen[stopped]
        );
        // The previous process is gone.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while process_alive(old_pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_alive(old_pid), "old app still running");

        // Ctrl-C: dev stops the app and exits successfully.
        let new_pid = {
            let line = dev
                .seen
                .iter()
                .rev()
                .find(|l| l.contains("Running"))
                .unwrap();
            pid_from_line(line).unwrap()
        };
        signal(dev.child.id(), "INT");
        let status = dev.wait_exit(Duration::from_secs(30));
        assert!(status.success(), "{status}; {:?}", dev.seen);
        assert!(
            dev.seen.iter().any(|l| l.contains("Stopped")),
            "{:?}",
            dev.seen
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while process_alive(new_pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_alive(new_pid), "app outlived dev");
    }

    #[test]
    fn a_failed_build_keeps_the_previous_version_running() {
        let tmp = tempfile::tempdir().unwrap();
        let app = fake_app(tmp.path(), "dev-app");
        let mut dev = Watched::spawn(dev_command(&app, &[]));
        dev.wait_for("fake-app v1 running", SLOW);

        fs::write(app.join("src/main.rs"), "fn main() { broken broken }\n").unwrap();
        let failed = dev.wait_for("build failed", SLOW);
        assert!(
            dev.seen[failed].contains("previous version keeps running"),
            "{:?}",
            dev.seen[failed]
        );
        dev.assert_absent_for("Stopping", Duration::from_millis(500));

        // Fixing the error brings the new version up.
        write_fake_main(&app, "v3");
        dev.wait_for("fake-app v3 running", SLOW);
        dev.wait_for("Stopping", SLOW);

        signal(dev.child.id(), "TERM");
        assert!(dev.wait_exit(Duration::from_secs(30)).success());
    }

    #[test]
    fn editor_temp_files_and_target_do_not_trigger_rebuilds() {
        let tmp = tempfile::tempdir().unwrap();
        let app = fake_app(tmp.path(), "dev-app");
        let mut dev = Watched::spawn(dev_command(&app, &[]));
        dev.wait_for("fake-app v1 running", SLOW);
        let seen_before = dev.seen.len();

        fs::write(app.join("src/main.rs.swp"), "swap").unwrap();
        fs::write(app.join("src/main.rs~"), "backup").unwrap();
        fs::write(app.join("src/.#main.rs"), "lock").unwrap();
        fs::write(app.join("README.md"), "docs").unwrap();
        fs::create_dir_all(app.join("target/debug")).unwrap();
        fs::write(app.join("target/debug/junk"), "x").unwrap();
        dev.assert_absent_for("Changed", Duration::from_secs(2));
        assert_eq!(
            dev.seen
                .iter()
                .skip(seen_before)
                .filter(|l| l.contains("Building"))
                .count(),
            0
        );

        signal(dev.child.id(), "INT");
        assert!(dev.wait_exit(Duration::from_secs(30)).success());
    }

    #[test]
    fn manifest_changes_trigger_a_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let app = fake_app(tmp.path(), "dev-app");
        let mut dev = Watched::spawn(dev_command(&app, &[]));
        dev.wait_for("fake-app v1 running", SLOW);

        let manifest = fs::read_to_string(app.join("Cargo.toml")).unwrap();
        fs::write(app.join("Cargo.toml"), format!("{manifest}\n# touched\n")).unwrap();
        let changed = dev.wait_for("Changed", SLOW);
        assert!(
            dev.seen[changed].contains("Cargo.toml"),
            "{:?}",
            dev.seen[changed]
        );
        dev.wait_for_after(changed, "fake-app v1 running", SLOW);

        signal(dev.child.id(), "INT");
        assert!(dev.wait_exit(Duration::from_secs(30)).success());
    }

    #[test]
    fn reports_an_app_that_exits_and_waits_for_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let app = fake_app(tmp.path(), "dev-app");
        fs::write(
            app.join("src/main.rs"),
            "fn main() { println!(\"quick\"); std::process::exit(7); }\n",
        )
        .unwrap();
        let mut dev = Watched::spawn(dev_command(&app, &[]));
        dev.wait_for("quick", SLOW);
        let crashed = dev.wait_for("Crashed", SLOW);
        assert!(
            dev.seen[crashed].contains("exit status: 7"),
            "{:?}",
            dev.seen[crashed]
        );

        write_fake_main(&app, "v2");
        dev.wait_for("fake-app v2 running", SLOW);
        signal(dev.child.id(), "INT");
        assert!(dev.wait_exit(Duration::from_secs(30)).success());
    }

    #[test]
    fn unknown_bin_fails_fast() {
        let tmp = tempfile::tempdir().unwrap();
        let app = fake_app(tmp.path(), "dev-app");
        let log = expect_failure(&dev_command(&app, &["--bin", "nope"]).output().unwrap());
        assert!(log.contains("no binary named `nope`"), "{log}");
    }

    #[test]
    fn dev_outside_a_project_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let log = expect_failure(&dev_command(tmp.path(), &[]).output().unwrap());
        assert!(log.contains("Cargo.toml"), "{log}");
    }
}

// ---- bench ---------------------------------------------------------------------------------

#[cfg(unix)]
mod bench {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const WRK_OUTPUT: &str = "\
Running 1s test @ http://127.0.0.1:1/
  2 threads and 8 connections
  Thread Stats   Avg      Stdev     Max   +/- Stdev
    Latency     1.20ms  512.30us  25.12ms   88.41%
    Req/Sec    52.31k     4.11k   61.20k    73.00%
  Latency Distribution
     50%    1.10ms
     75%    1.30ms
     90%    1.80ms
     99%    3.20ms
  2087654 requests in 1.00s, 254.84MB read
Requests/sec: 208765.43
Transfer/sec:     25.48MB";

    fn fake_wrk(dir: &std::path::Path, record: &std::path::Path) {
        let script = dir.join("wrk");
        // Only shell builtins: the tests run with a PATH that contains nothing else.
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" > '{}'\nprintf '%s\\n' '{WRK_OUTPUT}'\n",
                record.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn bench_wraps_wrk_and_summarizes() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let record = tmp.path().join("args.txt");
        fake_wrk(&bin, &record);

        let out = zapline()
            .args([
                "bench",
                "http://127.0.0.1:1/",
                "-c",
                "8",
                "-d",
                "1s",
                "-t",
                "2",
            ])
            .env("PATH", &bin)
            .output()
            .unwrap();
        expect_success(&out);
        let text = stdout(&out);
        assert!(
            text.contains("http://127.0.0.1:1/  (8 connections, 2 threads, 1s)"),
            "{text}"
        );
        assert!(text.contains("208,765.43"), "{text}");
        assert!(text.contains("p99 3.20ms"), "{text}");
        assert_eq!(
            fs::read_to_string(&record).unwrap().trim(),
            "-t2 -c8 -d1s --latency http://127.0.0.1:1/"
        );
    }

    #[test]
    fn bench_raw_prints_wrks_report() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fake_wrk(&bin, &tmp.path().join("args.txt"));
        let out = zapline()
            .args(["bench", "http://127.0.0.1:1/", "--raw", "-d", "1"])
            .env("PATH", &bin)
            .output()
            .unwrap();
        expect_success(&out);
        assert!(stdout(&out).contains("Thread Stats"), "{}", stdout(&out));
    }

    #[test]
    fn bench_without_wrk_explains_how_to_install_it() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty");
        fs::create_dir(&empty).unwrap();
        let out = zapline()
            .args(["bench", "http://127.0.0.1:1/"])
            .env("PATH", &empty)
            .output()
            .unwrap();
        let log = expect_failure(&out);
        assert!(log.contains("`wrk` was not found"), "{log}");
        assert!(log.contains("install"), "{log}");
    }

    #[test]
    fn bench_validates_its_arguments_before_running() {
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            vec!["bench", "localhost:8080"],
            vec!["bench", "http://x", "-d", "soon"],
            vec!["bench", "http://x", "-c", "0"],
            vec!["bench", "http://x", "-t", "0"],
        ] {
            let out = run_in(tmp.path(), &args);
            let log = expect_failure(&out);
            assert!(log.contains("error"), "{args:?}: {log}");
        }
    }
}
