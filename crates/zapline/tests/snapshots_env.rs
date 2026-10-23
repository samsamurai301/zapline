//! `ZAPLINE_RECORD=1` and saving snapshots into a project. One test, in its
//! own binary: it changes the process's environment and working directory.
#![cfg(all(debug_assertions, feature = "json"))]

use serde_json::Value;
use zapline::prelude::*;

fn app() -> App {
    App::new().dev_console().get("/hello", || async { "hello" })
}

#[tokio::test]
async fn records_from_startup_survives_restarts_and_saves_into_the_project() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo-app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src/routes")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn app() -> zapline::App { zapline::App::new() }\n",
    )
    .unwrap();
    // Run from a subdirectory: the project root is found above it.
    std::env::set_current_dir(root.join("src/routes")).unwrap();
    std::env::set_var(zapline::dev::ENV_RECORD, "1");

    let first = app().test();
    first.get("/hello").send().await.assert_text("hello");
    let mirror = root.join("target/zapline/recording.jsonl");
    let lines = std::fs::read_to_string(&mirror).unwrap();
    assert_eq!(lines.lines().count(), 1, "{lines}");

    // A restart (`zapline dev` rebuilt the app) picks the recording up.
    let client = app().test();
    client.get("/hello?again=1").send().await;
    client
        .get("/_dev")
        .send()
        .await
        .assert_contains("<strong>Recording</strong> · 2 request(s) recorded.");

    let res = client
        .post("/_dev/recordings/save")
        .header("accept", "application/json")
        .form(&[("name", "hello")])
        .send()
        .await;
    res.assert_status(200);
    let saved: Value = res.json();
    assert_eq!(saved["file"], "tests/snapshots/hello.json");
    assert_eq!(saved["exchanges"], 2);
    assert_eq!(saved["harness"], "tests/snapshots.rs");

    let snapshot: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("tests/snapshots/hello.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot["exchanges"][1]["request"]["query"], "again=1");
    let harness = std::fs::read_to_string(root.join("tests/snapshots.rs")).unwrap();
    assert!(harness.contains("demo_app::app()"), "{harness}");
    assert!(
        harness.contains(".replay_snapshots(\"tests/snapshots\")"),
        "{harness}"
    );
    assert!(harness.contains("#[tokio::test]"), "{harness}");

    // Saving empties the recording (and its mirror).
    assert!(!mirror.exists());
    client
        .post("/_dev/recordings/save")
        .header("accept", "application/json")
        .form(&[("name", "again")])
        .send()
        .await
        .assert_status(409);

    // The console button saves too, and the harness is only written once.
    std::fs::write(root.join("tests/snapshots.rs"), "// mine\n").unwrap();
    client.get("/hello").send().await;
    client
        .post("/_dev/recordings/save")
        .form(&[("name", "from-button")])
        .send()
        .await
        .assert_status(303)
        .assert_header(
            "location",
            "/_dev?saved=tests%2Fsnapshots%2Ffrom%2Dbutton%2Ejson",
        );
    assert!(root.join("tests/snapshots/from-button.json").is_file());
    assert_eq!(
        std::fs::read_to_string(root.join("tests/snapshots.rs")).unwrap(),
        "// mine\n"
    );
    client
        .get("/_dev?saved=tests%2Fsnapshots%2Ffrom%2Dbutton%2Ejson")
        .send()
        .await
        .assert_contains("Saved <code>tests/snapshots/from-button.json</code>.");

    // The saved snapshots replay against the app.
    app()
        .test()
        .replay_snapshots(root.join("tests/snapshots"))
        .await;
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
}
