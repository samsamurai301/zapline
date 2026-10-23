//! One-file apps end to end: `zapline new app.rs`, `zapline run app.rs`,
//! `zapline dev app.rs`.
//!
//! Every test gets its own cache directory (`XDG_CACHE_HOME`). The scripts
//! that are really built point zapline, serde, serde_json and tokio at tiny
//! local stand-in crates through their `cargo` header, so the tests compile in
//! a second and need no network; the starter that `zapline new` writes is
//! checked with `cargo metadata`. `tests/scaffold.rs` compiles the real starter
//! against this checkout's zapline (ignored by default, since that compiles
//! zapline).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use common::*;

const SLOW: Duration = Duration::from_secs(90);

/// A `zapline` command with its cache in `dir/cache`.
fn zapline_in(dir: &Path) -> Command {
    let mut cmd = zapline();
    cmd.current_dir(dir)
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env_remove("CARGO_BUILD_TARGET");
    cmd
}

fn run(dir: &Path, args: &[&str]) -> Output {
    zapline_in(dir).args(args).output().expect("zapline runs")
}

/// Stand-ins for the default dependencies, so builds stay small and offline.
fn stand_in_crates(dir: &Path) -> String {
    let mut header = String::from("//! [dependencies]\n");
    for name in ["zapline", "serde", "serde_json", "tokio"] {
        let root = dir.join("vendor").join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"1.0.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        // Relative paths are relative to the script.
        header.push_str(&format!("//! {name} = {{ path = \"vendor/{name}\" }}\n"));
    }
    header
}

/// A script with a shebang, a `cargo` header using the stand-ins, and `body`.
fn with_header(dir: &Path, body: &str) -> String {
    format!(
        "#!/usr/bin/env -S zapline run\n//! A test app.\n//!\n//! ```cargo\n{}//! ```\n\n{body}",
        stand_in_crates(dir)
    )
}

/// Writes `app.rs`: prints `version`, the contents of `data.txt` and its
/// arguments, then exits with the number of arguments (or sleeps, to serve).
fn write_script(dir: &Path, version: &str, serve: bool) -> PathBuf {
    let tail = if serve {
        "std::thread::sleep(std::time::Duration::from_secs(120));"
    } else {
        "std::process::exit(args.len() as i32);"
    };
    let source = with_header(
        dir,
        &format!(
            "const DATA: &str = include_str!(\"data.txt\");\n\
             \n\
             fn main() {{\n\
             \x20   let args: Vec<String> = std::env::args().skip(1).collect();\n\
             \x20   println!(\"app {version} data={{}} args={{args:?}}\", DATA.trim());\n\
             \x20   {tail}\n\
             }}\n"
        ),
    );
    if !dir.join("data.txt").exists() {
        fs::write(dir.join("data.txt"), "one").unwrap();
    }
    let path = dir.join("app.rs");
    fs::write(&path, source).unwrap();
    path
}

fn cargo_metadata(manifest: &Path) -> serde_json::Value {
    let out = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "metadata",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .arg("--manifest-path")
        .arg(manifest)
        .output()
        .expect("cargo runs");
    assert!(out.status.success(), "{}", stderr(&out));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn new_writes_a_starter_whose_hidden_project_is_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let err = expect_success(&run(&dir, &["new", "hello.rs"]));
    assert!(err.contains("Created"), "{err}");
    assert!(err.contains("zapline dev hello.rs"), "{err}");
    let source = fs::read_to_string(dir.join("hello.rs")).unwrap();
    assert!(source.starts_with("#!/usr/bin/env -S zapline run\n"));
    assert!(source.contains("```cargo"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join("hello.rs"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o100, 0o100, "{mode:o}");
    }
    // A second `new` never overwrites it.
    let err = expect_failure(&run(&dir, &["new", "hello.rs"]));
    assert!(err.contains("already exists"), "{err}");
    // Templates are for projects.
    let err = expect_failure(&run(&dir, &["new", "other.rs", "--template", "api"]));
    assert!(err.contains("one-file app"), "{err}");
    assert!(!dir.join("other.rs").exists());

    // The hidden project lives in the cache and is a valid cargo package.
    let out = run(&dir, &["run", "--print-manifest", "hello.rs"]);
    expect_success(&out);
    let manifest = PathBuf::from(stdout(&out).trim());
    assert!(
        manifest.starts_with(dir.join("cache/zapline/scripts")),
        "{manifest:?}"
    );
    let config = fs::read_to_string(manifest.with_file_name(".cargo").join("config.toml")).unwrap();
    assert!(config.contains("target-dir"), "{config}");
    assert!(config.contains("cache/zapline/target"), "{config}");

    let meta = cargo_metadata(&manifest);
    let package = &meta["packages"][0];
    assert_eq!(package["name"], "hello");
    let targets = package["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert_eq!(targets[0]["kind"][0], "bin");
    assert_eq!(
        Path::new(targets[0]["src_path"].as_str().unwrap()),
        dir.join("hello.rs")
    );
    let deps: Vec<&str> = package["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    for name in ["zapline", "serde", "serde_json", "tokio"] {
        assert!(deps.contains(&name), "{name} missing from {deps:?}");
    }
    // Its own workspace: never absorbed by a parent workspace.
    assert_eq!(
        Path::new(meta["workspace_root"].as_str().unwrap()),
        manifest.parent().unwrap()
    );
}

#[test]
fn a_bad_header_is_reported_at_its_file_and_line() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("bad.rs"),
        "//! Bad.\n//! ```cargo\n//! [dependencies]\n//! uuid = \"1\n//! ```\nfn main() {}\n",
    )
    .unwrap();
    let err = expect_failure(&run(tmp.path(), &["run", "bad.rs"]));
    assert!(err.contains("bad.rs:4:"), "{err}");
    assert!(err.contains("invalid ```cargo block"), "{err}");

    fs::write(
        tmp.path().join("bins.rs"),
        "//! ```cargo\n//! [[bin]]\n//! name = \"x\"\n//! ```\nfn main() {}\n",
    )
    .unwrap();
    let err = expect_failure(&run(tmp.path(), &["run", "bins.rs"]));
    assert!(err.contains("bins.rs:2:"), "{err}");
    assert!(err.contains("`bin` is not supported"), "{err}");

    let err = expect_failure(&run(tmp.path(), &["run", "missing.rs"]));
    assert!(err.contains("does not exist"), "{err}");
    assert!(err.contains("zapline new missing.rs"), "{err}");
}

#[test]
fn run_builds_once_then_starts_without_cargo() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    write_script(&dir, "v1", false);

    // First run: compiles, forwards arguments, exits with the app's code.
    let out = run(&dir, &["run", "app.rs", "a", "--flag"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(stdout(&out), "app v1 data=one args=[\"a\", \"--flag\"]\n");
    assert!(stderr(&out).contains("Compiling app"), "{}", stderr(&out));
    // The shared target directory is in the cache.
    assert!(dir.join("cache/zapline/target/debug").is_dir());

    // Unchanged: cargo is not run at all.
    let started = Instant::now();
    let out = run(&dir, &["run", "app.rs", "--", "x"]);
    let took = started.elapsed();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stdout(&out), "app v1 data=one args=[\"x\"]\n");
    assert_eq!(stderr(&out), "", "cargo ran for an unchanged script");
    assert!(took < Duration::from_secs(1), "second run took {took:?}");

    // A file pulled in with include_str! is an input too.
    std::thread::sleep(Duration::from_millis(20));
    fs::write(dir.join("data.txt"), "two").unwrap();
    let out = run(&dir, &["run", "app.rs"]);
    assert_eq!(
        stdout(&out),
        "app v1 data=two args=[]\n",
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("Compiling app"), "{}", stderr(&out));

    // Editing the script rebuilds it.
    std::thread::sleep(Duration::from_millis(20));
    write_script(&dir, "v2", false);
    let out = run(&dir, &["run", "app.rs"]);
    assert_eq!(
        stdout(&out),
        "app v2 data=two args=[]\n",
        "{}",
        stderr(&out)
    );

    // Compiler errors point at the script itself.
    fs::write(
        dir.join("broken.rs"),
        with_header(&dir, "fn main() {\n    let x: u32 = \"no\";\n}\n"),
    )
    .unwrap();
    let out = run(&dir, &["run", "broken.rs"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains(&format!("{}:", dir.join("broken.rs").display())),
        "{err}"
    );
    assert!(err.contains("could not compile `broken.rs`"), "{err}");
}

#[cfg(unix)]
#[test]
fn the_shebang_line_makes_the_file_executable() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let script = write_script(&dir, "v1", false);
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    // `env -S zapline run` finds zapline on the PATH.
    let bin_dir = Path::new(zapline_bin()).parent().unwrap().to_path_buf();
    let path = std::env::join_paths(
        std::iter::once(bin_dir).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let out = Command::new(&script)
        .args(["p", "q", "r"])
        .current_dir(&dir)
        .env("PATH", path)
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("NO_COLOR", "1")
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    // `env -S` is missing on some old systems; nothing to test there.
    if stderr(&out).contains("invalid option") {
        return;
    }
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert_eq!(stdout(&out), "app v1 data=one args=[\"p\", \"q\", \"r\"]\n");
}

#[test]
fn check_works_on_a_one_file_app() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let script = write_script(&dir, "v1", false);
    let out = run(&dir, &["check", "app.rs"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("no errors"), "{}", stdout(&out));

    let source = fs::read_to_string(&script).unwrap();
    fs::write(
        &script,
        source.replace("fn main() {", "fn main() {\n    let unused = 1;"),
    )
    .unwrap();
    let out = run(&dir, &["check", "app.rs"]);
    let report = stdout(&out);
    assert!(report.contains("unused"), "{report}");
    assert!(report.contains("app.rs:"), "{report}");
}

#[cfg(unix)]
#[test]
fn dev_rebuilds_a_one_file_app_on_save() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    write_script(&dir, "v1", true);
    let mut cmd = zapline_in(&dir);
    cmd.args([
        "dev",
        "--debounce-ms",
        "100",
        "app.rs",
        "--",
        "--port",
        "9000",
    ]);
    let mut dev = Watched::spawn(cmd);

    dev.wait_for("Watching", SLOW);
    let first = dev.wait_for("app v1", SLOW);
    assert!(
        dev.seen[first].contains(r#"args=["--port", "9000"]"#),
        "{:?}",
        dev.seen[first]
    );

    // Saving the file rebuilds and restarts.
    write_script(&dir, "v2", true);
    dev.wait_for("app v2", SLOW);
    dev.wait_for("Stopping", SLOW);

    // A broken header is reported at its line; the running version stays up.
    let source = fs::read_to_string(dir.join("app.rs")).unwrap();
    fs::write(
        dir.join("app.rs"),
        source.replacen("//! [dependencies]", "//! [dependencies", 1),
    )
    .unwrap();
    let failed = dev.wait_for("invalid ```cargo block", SLOW);
    assert!(
        dev.seen[failed].contains("app.rs:5:"),
        "{:?}",
        dev.seen[failed]
    );

    // Fixing it brings up the next version.
    write_script(&dir, "v3", true);
    dev.wait_for("app v3", SLOW);

    signal(dev.child.id(), "INT");
    let status = dev.wait_exit(Duration::from_secs(30));
    assert!(status.success(), "{status}; {:?}", dev.seen);
}
