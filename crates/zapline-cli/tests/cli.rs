//! End-to-end tests of the `zapline` binary: scaffolding, generators, help.

mod common;

use std::fs;
use std::path::Path;

use common::*;

fn files_of(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                out.push(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn read_toml(path: &Path) -> toml::Table {
    toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

// ---- help & version -----------------------------------------------------------------

#[test]
fn version_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_in(tmp.path(), &["--version"]);
    assert!(stdout(&out).starts_with("zapline "), "{}", stdout(&out));
    assert!(stdout(&out).contains(env!("CARGO_PKG_VERSION")));
    assert!(out.status.success());
}

#[test]
fn help_lists_every_command_with_examples() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_in(tmp.path(), &["--help"]);
    assert!(out.status.success());
    let help = stdout(&out);
    for cmd in [
        "new", "generate", "dev", "routes", "openapi", "doctor", "bench",
    ] {
        assert!(
            help.contains(&format!("\n  {cmd}")),
            "missing {cmd}:\n{help}"
        );
    }
    assert!(help.contains("Examples:"), "{help}");
    assert!(help.contains("zapline new my-api"), "{help}");
    assert!(help.contains("cargo zapline"), "{help}");
}

#[test]
fn subcommand_help_explains_templates_and_markers() {
    let tmp = tempfile::tempdir().unwrap();
    let help = stdout(&run_in(tmp.path(), &["new", "--help"]));
    for needle in [
        "minimal",
        "api",
        "full",
        "--template",
        "--zapline-path",
        "--no-git",
        "--path",
    ] {
        assert!(help.contains(needle), "new --help lacks {needle}:\n{help}");
    }
    let help = stdout(&run_in(tmp.path(), &["generate", "resource", "--help"]));
    assert!(help.contains("zapline:routes:start"), "{help}");
    assert!(help.contains("--plural"), "{help}");
}

#[test]
fn no_arguments_prints_usage_and_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_in(tmp.path(), &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Usage"), "{}", stderr(&out));
}

#[test]
fn unknown_command_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_in(tmp.path(), &["frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("frobnicate"));
}

// ---- new ------------------------------------------------------------------------------

#[test]
fn new_minimal_project() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_in(
        tmp.path(),
        &["new", "hello-app", "--template", "minimal", "--no-git"],
    );
    let log = expect_success(&out);
    assert!(log.contains("Created"), "{log}");
    assert!(log.contains("zapline dev"), "{log}");

    let root = tmp.path().join("hello-app");
    assert_eq!(
        files_of(&root),
        [
            ".gitignore",
            "AGENTS.md",
            "Cargo.toml",
            "README.md",
            "src/lib.rs",
            "src/main.rs",
            "tests/api.rs",
            "tests/fuzz.rs"
        ]
    );
    let manifest = read_toml(&root.join("Cargo.toml"));
    assert_eq!(manifest["package"]["name"].as_str(), Some("hello-app"));
    assert!(manifest["dependencies"]
        .as_table()
        .unwrap()
        .contains_key("zapline"));
    assert!(fs::read_to_string(root.join("src/main.rs"))
        .unwrap()
        .contains("hello_app::app()"));
    assert!(!root.join(".git").exists());
}

#[test]
fn new_api_is_the_default_template() {
    let tmp = tempfile::tempdir().unwrap();
    expect_success(&run_in(tmp.path(), &["new", "shop", "--no-git"]));
    let root = tmp.path().join("shop");
    let files = files_of(&root);
    for f in [
        "Dockerfile",
        ".dockerignore",
        ".env.example",
        ".github/workflows/ci.yml",
        "src/models.rs",
        "src/routes/mod.rs",
        "src/routes/health.rs",
        "src/routes/users.rs",
        "tests/users.rs",
        "tests/fuzz.rs",
    ] {
        assert!(files.iter().any(|g| g == f), "missing {f} in {files:?}");
    }
    let manifest = read_toml(&root.join("Cargo.toml"));
    let deps = manifest["dependencies"].as_table().unwrap();
    // Models use `#[model]`: zapline is the only dependency they need.
    assert!(deps.contains_key("zapline"));
    for d in ["serde", "schemars", "validator"] {
        assert!(!deps.contains_key(d), "unneeded dependency {d}");
    }
    let mod_rs = fs::read_to_string(root.join("src/routes/mod.rs")).unwrap();
    assert!(mod_rs.contains("// zapline:routes:start") && mod_rs.contains("// zapline:routes:end"));
}

#[test]
fn new_full_project() {
    let tmp = tempfile::tempdir().unwrap();
    expect_success(&run_in(
        tmp.path(),
        &["new", "platform", "-t", "full", "--no-git"],
    ));
    let root = tmp.path().join("platform");
    let files = files_of(&root);
    for f in [
        "config/default.toml",
        "config/development.toml",
        "config/production.toml",
        "src/config.rs",
        "src/repository.rs",
        "tests/ops.rs",
    ] {
        assert!(files.iter().any(|g| g == f), "missing {f}");
    }
    for cfg in ["default", "development", "production"] {
        read_toml(&root.join(format!("config/{cfg}.toml")));
    }
    let manifest = read_toml(&root.join("Cargo.toml"));
    assert_eq!(
        manifest["dependencies"]["zapline"]["features"][0].as_str(),
        Some("compression")
    );
}

#[test]
fn new_respects_path_and_zapline_path() {
    let tmp = tempfile::tempdir().unwrap();
    let zapline_dir = tmp.path().join("checkout/crates/zapline");
    fs::create_dir_all(&zapline_dir).unwrap();
    fs::write(
        zapline_dir.join("Cargo.toml"),
        "[package]\nname = \"zapline\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let parent = tmp.path().join("projects/nested");

    expect_success(&run_in(
        tmp.path(),
        &[
            "new",
            "svc",
            "--template",
            "minimal",
            "--no-git",
            "--path",
            parent.to_str().unwrap(),
            "--zapline-path",
            zapline_dir.to_str().unwrap(),
        ],
    ));
    let manifest = read_toml(&parent.join("svc/Cargo.toml"));
    let dep = &manifest["dependencies"]["zapline"];
    let path = dep["path"].as_str().unwrap();
    assert!(path.ends_with("crates/zapline"), "{path}");
    assert!(Path::new(path).is_absolute());
    assert!(!tmp.path().join("svc").exists());
}

#[test]
fn new_initializes_git_unless_disabled() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let inside_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(tmp.path())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let log = expect_success(&run_in(tmp.path(), &["new", "versioned", "-t", "minimal"]));
    if inside_repo {
        assert!(log.contains("already inside a repository"), "{log}");
    } else {
        assert!(tmp.path().join("versioned/.git").is_dir());
        assert!(log.contains("Initialized"), "{log}");
    }
}

#[test]
fn new_rejects_bad_names() {
    let tmp = tempfile::tempdir().unwrap();
    for bad in [
        "1app",
        "my app",
        "../escape",
        "a/b",
        "fn",
        "zapline",
        "serde",
        "nul",
        ".hidden",
    ] {
        let out = run_in(tmp.path(), &["new", bad, "--no-git"]);
        let log = expect_failure(&out);
        assert!(log.contains("error:"), "{bad}: {log}");
        assert_eq!(out.status.code(), Some(1), "{bad}");
    }
    assert_eq!(
        fs::read_dir(tmp.path()).unwrap().count(),
        0,
        "nothing may be created"
    );
}

#[test]
fn new_refuses_non_empty_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let existing = tmp.path().join("taken");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("keep.txt"), "mine").unwrap();
    let log = expect_failure(&run_in(tmp.path(), &["new", "taken", "--no-git"]));
    assert!(log.contains("not empty"), "{log}");
    assert_eq!(
        fs::read_to_string(existing.join("keep.txt")).unwrap(),
        "mine"
    );
    assert_eq!(files_of(&existing), ["keep.txt"]);

    // Creating the same project twice fails the second time.
    expect_success(&run_in(
        tmp.path(),
        &["new", "twice", "-t", "minimal", "--no-git"],
    ));
    let log = expect_failure(&run_in(
        tmp.path(),
        &["new", "twice", "-t", "minimal", "--no-git"],
    ));
    assert!(log.contains("not empty"), "{log}");
}

#[test]
fn new_accepts_an_existing_empty_directory() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("blank")).unwrap();
    expect_success(&run_in(
        tmp.path(),
        &["new", "blank", "-t", "minimal", "--no-git"],
    ));
    assert!(tmp.path().join("blank/Cargo.toml").is_file());
}

#[test]
fn new_rejects_a_bad_zapline_path() {
    let tmp = tempfile::tempdir().unwrap();
    let log = expect_failure(&run_in(
        tmp.path(),
        &[
            "new",
            "x",
            "--no-git",
            "--zapline-path",
            "/definitely/not/here",
        ],
    ));
    assert!(log.contains("--zapline-path"), "{log}");
    assert!(!tmp.path().join("x").exists());
}

#[test]
fn new_rejects_unknown_templates() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_in(tmp.path(), &["new", "x", "--template", "huge"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("minimal"),
        "possible values listed: {}",
        stderr(&out)
    );
}

// ---- generate ---------------------------------------------------------------------------

fn new_project(dir: &Path, name: &str, template: &str) -> std::path::PathBuf {
    expect_success(&run_in(dir, &["new", name, "-t", template, "--no-git"]));
    dir.join(name)
}

#[test]
fn generate_resource_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_project(tmp.path(), "blog", "api");

    let log = expect_success(&run_in(&root, &["generate", "resource", "post"]));
    assert!(log.contains("Created src/routes/posts.rs"), "{log}");
    assert!(log.contains("Created tests/posts.rs"), "{log}");
    assert!(log.contains("Modified src/routes/mod.rs"), "{log}");

    let mod_rs = fs::read_to_string(root.join("src/routes/mod.rs")).unwrap();
    assert!(mod_rs.contains("pub mod posts;"), "{mod_rs}");
    let start = mod_rs.find("// zapline:routes:start").unwrap();
    let end = mod_rs.find("// zapline:routes:end").unwrap();
    let mount = mod_rs.find("app = app.plugin(posts::mount);").unwrap();
    assert!(start < mount && mount < end, "{mod_rs}");

    // The alias works, and a second resource lands next to the first.
    let log = expect_success(&run_in(&root, &["g", "resource", "BlogComment"]));
    assert!(log.contains("src/routes/blog_comments.rs"), "{log}");
    let mod_rs = fs::read_to_string(root.join("src/routes/mod.rs")).unwrap();
    assert!(
        mod_rs.contains("pub mod blog_comments;\npub mod health;\npub mod posts;\npub mod users;"),
        "{mod_rs}"
    );
}

#[test]
fn generate_resource_twice_needs_force() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_project(tmp.path(), "blog", "api");
    expect_success(&run_in(&root, &["g", "resource", "post"]));
    let log = expect_failure(&run_in(&root, &["g", "resource", "post"]));
    assert!(log.contains("already exists"), "{log}");
    expect_success(&run_in(&root, &["g", "resource", "post", "--force"]));
}

#[test]
fn generate_resource_without_markers_prints_instructions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_project(tmp.path(), "blog", "api");
    fs::write(
        root.join("src/routes/mod.rs"),
        "pub mod health;\npub mod users;\n",
    )
    .unwrap();
    let log = expect_success(&run_in(&root, &["g", "resource", "post"]));
    assert!(log.contains("note:"), "{log}");
    assert!(log.contains("zapline:routes:start"), "{log}");
    assert!(log.contains("pub mod posts;"), "{log}");
    assert_eq!(
        fs::read_to_string(root.join("src/routes/mod.rs")).unwrap(),
        "pub mod health;\npub mod users;\n"
    );
}

#[test]
fn generate_in_a_minimal_project_explains_what_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_project(tmp.path(), "tiny", "minimal");
    let log = expect_success(&run_in(&root, &["g", "resource", "post"]));
    assert!(log.contains("cargo add --dev serde_json"), "{log}");
    assert!(log.contains("pub mod routes;"), "{log}");
    assert!(root.join("src/routes/mod.rs").is_file());
}

#[test]
fn generate_handler_and_middleware() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_project(tmp.path(), "svc", "api");

    let log = expect_success(&run_in(
        &root,
        &[
            "generate",
            "handler",
            "send_email",
            "--method",
            "post",
            "--path",
            "/emails/{id}",
        ],
    ));
    assert!(log.contains("src/routes/send_email.rs"), "{log}");
    let src = fs::read_to_string(root.join("src/routes/send_email.rs")).unwrap();
    assert!(
        src.contains("#[post(\"/emails/{id}\", status = 201)]"),
        "{src}"
    );
    assert!(fs::read_to_string(root.join("src/routes/mod.rs"))
        .unwrap()
        .contains("app = app.plugin(send_email::mount);"));

    let log = expect_success(&run_in(&root, &["generate", "middleware", "audit"]));
    assert!(log.contains("src/middleware/audit.rs"), "{log}");
    assert!(log.contains("pub mod middleware;"), "{log}");
    assert!(root.join("src/middleware/mod.rs").is_file());
    assert!(root.join("tests/audit_middleware.rs").is_file());
}

#[test]
fn generate_handler_validates_the_route_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_project(tmp.path(), "svc", "api");
    let log = expect_failure(&run_in(&root, &["g", "handler", "x", "--path", "no-slash"]));
    assert!(log.contains("must start with `/`"), "{log}");
    assert!(!root.join("src/routes/x.rs").exists());
    let out = run_in(&root, &["g", "handler", "x", "--method", "teleport"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn generate_outside_a_project_fails_helpfully() {
    let tmp = tempfile::tempdir().unwrap();
    let log = expect_failure(&run_in(tmp.path(), &["g", "resource", "post"]));
    assert!(log.contains("Cargo.toml"), "{log}");
    assert!(log.contains("zapline new"), "{log}");
}

#[test]
fn generate_rejects_invalid_names() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_project(tmp.path(), "svc", "api");
    for bad in ["type", "9lives", "a/b", "../x"] {
        expect_failure(&run_in(&root, &["g", "resource", bad]));
    }
    assert!(!root.join("src/routes/types.rs").exists());
}

#[test]
fn dash_c_runs_in_another_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_project(tmp.path(), "svc", "api");
    expect_success(&run_in(
        tmp.path(),
        &["-C", root.to_str().unwrap(), "g", "resource", "tag"],
    ));
    assert!(root.join("src/routes/tags.rs").is_file());
    let log = expect_failure(&run_in(
        tmp.path(),
        &["-C", "/definitely/missing", "doctor"],
    ));
    assert!(log.contains("cannot change to directory"), "{log}");
}

// ---- cargo subcommand ---------------------------------------------------------------------

#[test]
fn cargo_zapline_strips_the_subcommand_name() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_cargo-zapline");

    // This is how cargo invokes it: `cargo-zapline zapline <args>`.
    let out = std::process::Command::new(bin)
        .args(["zapline", "new", "viacargo", "-t", "minimal", "--no-git"])
        .current_dir(tmp.path())
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    expect_success(&out);
    assert!(tmp.path().join("viacargo/Cargo.toml").is_file());

    let out = std::process::Command::new(bin)
        .args(["zapline", "--help"])
        .output()
        .unwrap();
    let help = stdout(&out);
    assert!(help.contains("Usage: cargo zapline"), "{help}");

    // Direct invocation without the leading `zapline` also works.
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .unwrap();
    assert!(stdout(&out).contains(env!("CARGO_PKG_VERSION")));
}

// ---- doctor ----------------------------------------------------------------------------

#[test]
fn doctor_reports_the_toolchain() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_in(tmp.path(), &["doctor"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}\n{}", stderr(&out));
    assert!(text.contains("zapline doctor"), "{text}");
    assert!(text.contains("rustc"), "{text}");
    assert!(text.contains("cargo"), "{text}");
    assert!(text.contains("wrk"), "{text}");
    assert!(text.contains("docker"), "{text}");
}

#[test]
fn generate_job_registers_it_in_the_app() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_project(tmp.path(), "shop", "fullstack");
    let log = expect_success(&run_in(&root, &["g", "job", "send-receipt"]));
    assert!(log.contains("Created src/jobs/send_receipt.rs"), "{log}");
    assert!(log.contains("Created tests/send_receipt_job.rs"), "{log}");
    assert!(log.contains("Created src/jobs/mod.rs"), "{log}");
    assert!(log.contains("Modified src/lib.rs"), "{log}");

    expect_success(&run_in(&root, &["g", "job", "purge"]));
    let jobs = fs::read_to_string(root.join("src/jobs/mod.rs")).unwrap();
    assert!(
        jobs.contains("pub mod purge;\npub mod send_receipt;\n"),
        "{jobs}"
    );
    assert!(
        jobs.contains(".register::<send_receipt::SendReceipt>()"),
        "{jobs}"
    );
    assert!(jobs.contains(".register::<purge::Purge>()"), "{jobs}");
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert_eq!(lib.matches(".jobs(jobs::register)").count(), 1, "{lib}");
    assert!(lib.contains("pub mod jobs;\npub mod routes;"), "{lib}");

    // Jobs live in the database.
    let api = new_project(tmp.path(), "svc", "api");
    let log = expect_failure(&run_in(&api, &["g", "job", "x"]));
    assert!(log.contains("no migrations/ directory"), "{log}");
}

#[test]
fn explain_prints_a_code_and_lists_them_all() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_in(tmp.path(), &["explain", "zl2"]);
    expect_success(&out);
    let text = stdout(&out);
    assert!(
        text.starts_with("ZL0002: No provider for a dependency\n"),
        "{text}"
    );
    assert!(text.contains("\nFix:\n    .provide(value)"), "{text}");

    let list = stdout(&run_in(tmp.path(), &["explain"]));
    assert!(list.contains("ZL0010  Duplicate route\n"), "{list}");

    let err = expect_failure(&run_in(tmp.path(), &["explain", "ZL9999"]));
    assert!(err.contains("unknown code `ZL9999`"), "{err}");
}
