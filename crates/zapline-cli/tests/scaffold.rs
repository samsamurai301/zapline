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
}

/// Schema evolution on a real fullstack app: `g resource`, then `g migration
/// add_views_to_posts views:int=0` patches the module, page and test, and the
/// project's own tests (which round-trip every column through the JSON API)
/// pass with the new column; `db status` follows the database along.
#[test]
#[ignore = "slow: compiles zapline with sqlite and views"]
fn added_columns_round_trip_and_db_status_tracks_them() {
    let tmp = tempfile::tempdir().unwrap();
    let project = generate(tmp.path(), "scaffold-evolve", "fullstack");
    expect_success(&run_in(&project, &["g", "resource", "post", "title"]));
    let log = expect_success(&run_in(
        &project,
        &["g", "migration", "add_views_to_posts", "views:int=0"],
    ));
    for file in [
        "src/routes/posts.rs",
        "templates/posts/index.html",
        "tests/posts.rs",
    ] {
        assert!(log.contains(&format!("Modified {file}")), "{log}");
    }
    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .flatten()
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_add_views_to_posts.sql")
        })
        .expect("the migration exists")
        .path();
    let sql = fs::read_to_string(&migration).unwrap();
    assert!(
        sql.contains("ALTER TABLE posts ADD COLUMN views BIGINT NOT NULL DEFAULT 0;"),
        "{sql}"
    );
    let module = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(module.contains("pub views: i64,"), "{module}");
    assert!(
        module.contains("INSERT INTO posts (title, views) VALUES ($1, $2)"),
        "{module}"
    );
    let test = fs::read_to_string(project.join("tests/posts.rs")).unwrap();
    assert!(test.contains("\"views\": 42,"), "{test}");

    // `crud_round_trip` posts the sample (with views) and checks every field
    // comes back from POST, GET, the list and PUT.
    let tests = check_like_ci(&project);
    assert!(tests.contains("crud_round_trip ... ok"), "{tests}");

    let zapline_in = |args: &[&str]| {
        zapline()
            .args(args)
            .current_dir(&project)
            .env("CARGO_TARGET_DIR", target_dir())
            .env("DATABASE_URL", "sqlite://status.db?mode=rwc")
            .output()
            .unwrap()
    };
    let out = zapline_in(&["db", "status"]);
    expect_success(&out);
    let table = stdout(&out);
    assert!(table.contains("pending   add views to posts"), "{table}");
    assert!(table.contains("0 applied, 3 pending"), "{table}");

    expect_success(&zapline_in(&["db", "migrate"]));
    let out = zapline_in(&["db", "status"]);
    expect_success(&out);
    assert!(
        stdout(&out).contains("3 applied, 0 pending"),
        "{}",
        stdout(&out)
    );

    // Editing an applied migration is an error, with the file to restore.
    fs::write(&migration, format!("{sql}-- edited\n")).unwrap();
    let out = zapline_in(&["db", "status"]);
    assert!(!out.status.success());
    let table = stdout(&out);
    assert!(table.contains("modified  add views to posts"), "{table}");
    assert!(
        table.contains("_add_views_to_posts.sql as it was"),
        "{table}"
    );
}
