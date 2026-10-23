//! `zapline fuzz`: run the project's fuzz test (`tests/fuzz.rs`), which
//! calls `app().test().fuzz()`, and show its report.
//!
//! The seed and the number of cases reach the test through the
//! `ZAPLINE_FUZZ_SEED` and `ZAPLINE_FUZZ_CASES` environment variables, which
//! `FuzzConfig` reads when the test does not set them itself.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::build;
use crate::project::Project;

/// Environment variable read by `FuzzConfig` for the seed.
pub const ENV_FUZZ_SEED: &str = "ZAPLINE_FUZZ_SEED";
/// Environment variable read by `FuzzConfig` for the cases per operation.
pub const ENV_FUZZ_CASES: &str = "ZAPLINE_FUZZ_CASES";

/// The test file `zapline fuzz` runs.
pub const FUZZ_TEST: &str = "tests/fuzz.rs";

/// What a project needs when it has no fuzz test yet.
const MISSING: &str = "\
no tests/fuzz.rs in this project (`zapline new` creates one). Add it:

    // tests/fuzz.rs
    #[tokio::test]
    async fn every_route_survives_fuzzing() {
        my_app::app().test().fuzz().await.assert_clean();
    }
";

/// The `cargo test` invocation for the fuzz test.
pub fn command(project: &Project, seed: Option<u64>, cases: Option<u64>) -> Command {
    let mut cmd = build::cargo();
    cmd.args(["test", "--quiet", "--test", "fuzz", "--", "--nocapture"])
        .current_dir(&project.root)
        .env("RUST_BACKTRACE", "0");
    if let Some(seed) = seed {
        cmd.env(ENV_FUZZ_SEED, seed.to_string());
    }
    if let Some(cases) = cases {
        cmd.env(ENV_FUZZ_CASES, cases.to_string());
    }
    cmd
}

/// Runs the fuzz test with the terminal attached; `Ok(false)` when it
/// found failures (the report is already on screen).
pub fn run(project: &Project, seed: Option<u64>, cases: Option<u64>) -> Result<bool> {
    if !project.root.join(FUZZ_TEST).is_file() {
        anyhow::bail!(MISSING);
    }
    let mut child = command(project, seed, cases)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run `cargo test --test fuzz`")?;
    // The report is the test's panic message: pass stderr through as it
    // comes and keep a copy for the MCP `last_results` tool.
    let mut seen = Vec::new();
    if let Some(mut err) = child.stderr.take() {
        let mut buf = [0u8; 8192];
        let mut out = std::io::stderr();
        loop {
            match err.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = out.write_all(&buf[..n]);
                    seen.extend_from_slice(&buf[..n]);
                }
            }
        }
    }
    let status = child
        .wait()
        .context("failed to run `cargo test --test fuzz`")?;
    let summary = summarize(&String::from_utf8_lossy(&seen), status.success());
    crate::live::save_last_results(&project.root, "fuzz", &summary);
    Ok(status.success())
}

/// Most of a report `last_results` keeps.
const SUMMARY_BUDGET: usize = 4_000;

/// The fuzz report from the test's stderr, for `last_results`.
///
/// ```
/// use zapline_cli::fuzz::summarize;
/// let err = "thread 'x' panicked at tests/fuzz.rs:3:5:\nzapline fuzz: 2 operations, 90 requests, seed 1: 1 failure\n...\nnote: run with `RUST_BACKTRACE=1`";
/// assert!(summarize(err, false).starts_with("zapline fuzz: 2 operations"));
/// assert_eq!(summarize("", true), "ok: tests/fuzz.rs passed, no failures\n");
/// ```
pub fn summarize(stderr: &str, passed: bool) -> String {
    if passed {
        return "ok: tests/fuzz.rs passed, no failures\n".into();
    }
    let mut text = match stderr.rfind("zapline fuzz: ") {
        Some(start) => {
            let report = &stderr[start..];
            let end = report.find("\nnote: run with").unwrap_or(report.len());
            report[..end].trim_end().to_string()
        }
        None => {
            // No report: it did not compile or the test failed another way.
            let lines: Vec<&str> = stderr.lines().collect();
            let tail = lines[lines.len().saturating_sub(20)..].join("\n");
            format!("failed: `cargo test --test fuzz` did not finish with a report\n{tail}")
        }
    };
    if text.len() > SUMMARY_BUDGET {
        let mut cut = SUMMARY_BUDGET;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n... (cut; run `zapline fuzz` for the whole report)");
    }
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(root: &std::path::Path) -> Project {
        Project {
            root: root.to_path_buf(),
            package_name: "demo".into(),
            lib_name: None,
            bins: vec![],
            default_run: None,
            edition: "2021".into(),
            dependencies: Default::default(),
            dev_dependencies: Default::default(),
        }
    }

    #[test]
    fn passes_the_seed_and_cases_to_the_test() {
        let p = project(std::path::Path::new("/tmp/demo"));
        let cmd = command(&p, Some(7), Some(500));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["test", "--quiet", "--test", "fuzz", "--", "--nocapture"]
        );
        let env: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(env.contains(&(ENV_FUZZ_SEED.into(), Some("7".into()))));
        assert!(env.contains(&(ENV_FUZZ_CASES.into(), Some("500".into()))));

        let cmd = command(&p, None, None);
        assert!(!cmd
            .get_envs()
            .any(|(k, _)| k == ENV_FUZZ_SEED || k == ENV_FUZZ_CASES));
    }

    #[test]
    fn keeps_only_the_report() {
        let err = "   Compiling demo v0.1.0\nthread 'fuzz' panicked at tests/fuzz.rs:4:40:\n\
                   zapline fuzz: 1 operation, 40 requests, seed 9: 1 failure\n\
                   GET /x -> 500\nnote: run with `RUST_BACKTRACE=1` to display a backtrace\n";
        let s = summarize(err, false);
        assert!(s.starts_with("zapline fuzz: 1 operation"), "{s}");
        assert!(s.contains("GET /x -> 500"), "{s}");
        assert!(
            !s.contains("Compiling") && !s.contains("RUST_BACKTRACE"),
            "{s}"
        );

        let s = summarize("error[E0425]: cannot find value `x`\n", false);
        assert!(s.starts_with("failed:") && s.contains("E0425"), "{s}");

        let long = format!("zapline fuzz: {}", "x".repeat(10_000));
        assert!(summarize(&long, false).len() < SUMMARY_BUDGET + 100);
    }

    #[test]
    fn a_project_without_a_fuzz_test_gets_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(&project(dir.path()), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no tests/fuzz.rs"), "{err}");
        assert!(err.contains(".fuzz().await.assert_clean()"), "{err}");
    }
}
