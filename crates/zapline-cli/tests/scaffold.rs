//! Slow tests: generated projects must really compile and pass their own tests.
//!
//! They are `#[ignore]`d because they compile zapline itself. Run them with
//!
//! ```text
//! cargo test -p zapline-cli --test scaffold -- --ignored --test-threads=1
//! ```
//!
//! Dependencies are compiled once into `target/scaffold-check` (override with
//! `ZAPLINE_SCAFFOLD_TARGET`), and each project starts from the workspace's
//! `Cargo.lock` so dependency versions match what the workspace already built.
//! Point `ZAPLINE_SCAFFOLD_ZAPLINE` at another checkout's `crates/zapline` to
//! test against it instead of this workspace.
//!
//! The `api` and `full` templates use the `openapi`, `validate`, `health`,
//! `metrics`, `config`, `di` and `tasks` modules, so those tests only pass once
//! those modules are integrated.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::*;

/// The zapline checkout generated projects depend on: this workspace's
/// `crates/zapline`, or the one named by `ZAPLINE_SCAFFOLD_ZAPLINE`.
fn zapline_checkout() -> PathBuf {
    match std::env::var_os("ZAPLINE_SCAFFOLD_ZAPLINE") {
        Some(p) => PathBuf::from(p),
        None => workspace_root().join("crates/zapline"),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root exists")
}

fn target_dir() -> PathBuf {
    match std::env::var_os("ZAPLINE_SCAFFOLD_TARGET") {
        Some(p) => PathBuf::from(p),
        None => workspace_root().join("target/scaffold-check"),
    }
}

/// Generates a project with a path dependency on this checkout's zapline.
fn generate(dir: &Path, name: &str, template: &str) -> PathBuf {
    let zapline = zapline_checkout();
    expect_success(&run_in(
        dir,
        &[
            "new",
            name,
            "--template",
            template,
            "--no-git",
            "--zapline-path",
            zapline.to_str().unwrap(),
        ],
    ));
    let root = dir.join(name);
    // The checkout's workspace lockfile pins versions that are already compiled.
    let lock = zapline.join("../../Cargo.lock");
    if lock.is_file() {
        fs::copy(lock, root.join("Cargo.lock")).unwrap();
    }
    root
}

fn cargo(project: &Path, args: &[&str]) -> Output {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(args)
        .current_dir(project)
        .env("CARGO_TARGET_DIR", target_dir())
        .env("NO_COLOR", "1")
        .output()
        .expect("cargo runs")
}

fn expect_cargo_ok(project: &Path, args: &[&str]) -> String {
    let out = cargo(project, args);
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        out.status.success(),
        "`cargo {}` failed in {}:\n{text}",
        args.join(" "),
        project.display()
    );
    text
}

/// Builds, tests, formats and lints the project, like its CI workflow would.
fn check_like_ci(project: &Path) -> String {
    let tests = expect_cargo_ok(project, &["test"]);
    // No `--all`: it would also format the path dependency on zapline.
    expect_cargo_ok(project, &["fmt", "--", "--check"]);
    expect_cargo_ok(
        project,
        &["clippy", "--all-targets", "--", "-D", "warnings"],
    );
    tests
}

#[test]
#[ignore = "slow: compiles zapline"]
fn minimal_project_builds_and_passes_its_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let project = generate(tmp.path(), "scaffold-minimal", "minimal");
    let tests = check_like_ci(&project);
    assert!(tests.contains("test result: ok. 3 passed"), "{tests}");
}

#[test]
#[ignore = "slow: compiles zapline; needs the openapi and validate modules"]
fn api_project_builds_and_passes_its_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let project = generate(tmp.path(), "scaffold-api", "api");
    check_like_ci(&project);
}

#[test]
#[ignore = "slow: compiles zapline; needs all feature modules"]
fn full_project_builds_and_passes_its_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let project = generate(tmp.path(), "scaffold-full", "full");
    check_like_ci(&project);
}

#[test]
#[ignore = "slow: compiles zapline with sqlite, views and live"]
fn fullstack_project_survives_its_fuzz_test() {
    let tmp = tempfile::tempdir().unwrap();
    let project = generate(tmp.path(), "scaffold-fullstack", "fullstack");
    let out = expect_cargo_ok(&project, &["test", "--test", "fuzz", "--", "--nocapture"]);
    assert!(out.contains("no failures"), "{out}");
}

#[test]
#[ignore = "slow: compiles zapline; needs the openapi and validate modules"]
fn generated_resources_handlers_and_middleware_compile_and_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let project = generate(tmp.path(), "scaffold-gen", "api");
    expect_success(&run_in(&project, &["g", "resource", "blog-post"]));
    expect_success(&run_in(&project, &["g", "resource", "category"]));
    expect_success(&run_in(
        &project,
        &["g", "handler", "get_item", "--path", "/items/{id}"],
    ));
    expect_success(&run_in(
        &project,
        &["g", "handler", "make_thing", "--method", "post"],
    ));
    for (name, method, path) in [
        ("move_item", "post", "/boards/{board}/items/{id}"),
        ("rename_item", "patch", "/items/{id}/name"),
        ("replace_item", "put", "/items/{id}"),
        ("remove_item", "delete", "/items/{id}"),
        ("show_file", "get", "/files/{*rest}"),
        ("send_ping", "delete", "/ping"),
    ] {
        expect_success(&run_in(
            &project,
            &["g", "handler", name, "-m", method, "-p", path],
        ));
    }
    expect_success(&run_in(&project, &["g", "middleware", "audit"]));

    // Follow the instruction printed by `generate middleware`.
    let lib = fs::read_to_string(project.join("src/lib.rs")).unwrap();
    fs::write(
        project.join("src/lib.rs"),
        lib.replace("pub mod models;", "pub mod middleware;\npub mod models;"),
    )
    .unwrap();

    let tests = check_like_ci(&project);
    for name in [
        "blog_posts",
        "categories",
        "get_item",
        "make_thing",
        "move_item",
        "rename_item",
        "replace_item",
        "remove_item",
        "show_file",
        "send_ping",
        "audit_middleware",
    ] {
        assert!(
            tests.contains(&format!("tests/{name}.rs")),
            "{name} did not run:\n{tests}"
        );
    }
}

/// The database-backed generator on SQLite: dates, references (foreign key,
/// nested list, `<select>`), timestamps, PATCH, show/edit pages, plus a
/// `generate migration` that adds a column. The generated tests must pass.
#[test]
#[ignore = "slow: compiles zapline with sqlite, views and chrono"]
fn sql_resources_with_references_compile_and_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let project = generate(tmp.path(), "scaffold-sql", "fullstack");
    expect_success(&run_in(&project, &["g", "resource", "author", "name"]));
    let log = expect_success(&run_in(
        &project,
        &[
            "g",
            "resource",
            "post",
            "title",
            "body:text",
            "published_on:date",
            "author:references",
        ],
    ));
    assert!(log.contains("Created templates/posts/edit.html"), "{log}");
    assert!(!log.contains("no migration creates"), "{log}");
    let log = expect_success(&run_in(
        &project,
        &[
            "g",
            "migration",
            "add_rating_to_authors",
            "rating:int",
            "starts_at:datetime?",
        ],
    ));
    assert!(log.contains("_add_rating_to_authors.sql"), "{log}");
    let manifest = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(manifest.contains("\"chrono\""), "{manifest}");

    expect_cargo_ok(&project, &["check", "--all-targets"]);
    let tests = check_like_ci(&project);
    for name in [
        "crud_round_trip",
        "patch_changes_only_the_given_fields",
        "show_and_edit_pages",
        "listed_under_their_author",
    ] {
        assert!(
            tests.contains(&format!("test {name} ... ok")),
            "{name} did not pass:\n{tests}"
        );
    }
    assert!(tests.contains("tests/posts.rs"), "{tests}");
    assert!(tests.contains("tests/authors.rs"), "{tests}");
}

/// `zapline routes` and `zapline openapi` against a real app built on zapline
/// (not the dependency-free fake the fast tests use).
#[test]
#[ignore = "slow: compiles zapline; needs the openapi and validate modules"]
fn routes_and_openapi_work_on_a_real_project() {
    let tmp = tempfile::tempdir().unwrap();
    let project = generate(tmp.path(), "scaffold-real", "api");
    let zapline_in = |args: &[&str]| {
        zapline()
            .args(args)
            .current_dir(&project)
            .env("CARGO_TARGET_DIR", target_dir())
            .output()
            .unwrap()
    };

    let out = zapline_in(&["routes", "--json"]);
    expect_success(&out);
    let routes: Vec<serde_json::Value> = serde_json::from_str(stdout(&out).trim()).unwrap();
    let has = |method: &str, path: &str| {
        routes
            .iter()
            .any(|r| r["method"] == method && r["path"] == path)
    };
    assert!(has("GET", "/health"), "{routes:?}");
    assert!(has("GET", "/users"), "{routes:?}");
    assert!(has("POST", "/users"), "{routes:?}");
    assert!(has("PUT", "/users/{id}"), "{routes:?}");
    assert!(has("DELETE", "/users/{id}"), "{routes:?}");
    assert!(has("GET", "/openapi.json"), "{routes:?}");

    let out = zapline_in(&["routes"]);
    expect_success(&out);
    assert!(
        stdout(&out).starts_with("METHOD  PATH\n"),
        "{}",
        stdout(&out)
    );

    let log = expect_success(&zapline_in(&["openapi", "-o", "docs/openapi.json"]));
    assert!(log.contains("Wrote"), "{log}");
    let spec: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(project.join("docs/openapi.json")).unwrap())
            .unwrap();
    assert_eq!(spec["info"]["title"], "scaffold-real");
    assert!(spec["paths"]["/users"]["post"].is_object(), "{spec}");
    assert!(spec["paths"]["/users/{id}"]["delete"].is_object(), "{spec}");

    let out = zapline_in(&["fuzz", "--cases", "20", "--seed", "3"]);
    expect_success(&out);
    let log = stdout(&out);
    assert!(log.contains("zapline fuzz: "), "{log}");
    assert!(log.contains("seed 3: no failures"), "{log}");
}

/// `zapline new hello.rs`: the one-file starter compiles against this checkout,
/// passes the tests inside it, serves its routes and passes clippy.
#[test]
#[ignore = "slow: compiles zapline"]
fn one_file_starter_builds_and_passes_its_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let zapline_cmd = |args: &[&str]| {
        let out = zapline()
            .args(args)
            .current_dir(dir)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("CARGO_TARGET_DIR", target_dir())
            .output()
            .expect("zapline runs");
        (
            out.status.success(),
            format!("{}{}", stdout(&out), stderr(&out)),
        )
    };
    let checkout = zapline_checkout();
    let (ok, text) = zapline_cmd(&[
        "new",
        "hello.rs",
        "--zapline-path",
        checkout.to_str().unwrap(),
    ]);
    assert!(ok, "{text}");
    // Start from the workspace's lockfile, like the project tests.
    let (ok, manifest) = zapline_cmd(&["run", "--print-manifest", "hello.rs"]);
    assert!(ok, "{manifest}");
    let lock = checkout.join("../../Cargo.lock");
    if lock.is_file() {
        fs::copy(
            lock,
            Path::new(manifest.trim()).with_file_name("Cargo.lock"),
        )
        .unwrap();
    }

    let (ok, tests) = zapline_cmd(&["test", "hello.rs"]);
    assert!(ok, "{tests}");
    assert!(tests.contains("2 passed"), "{tests}");
    let (ok, routes) = zapline_cmd(&["routes", "hello.rs"]);
    assert!(ok, "{routes}");
    for route in ["/hello/{name}", "/notes"] {
        assert!(routes.contains(route), "{routes}");
    }
    let (ok, lints) = zapline_cmd(&["check", "--clippy", "hello.rs"]);
    assert!(ok && lints.contains("no errors, no warnings"), "{lints}");
}
