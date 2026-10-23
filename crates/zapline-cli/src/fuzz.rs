//! `zapline fuzz`: run the project's fuzz test (`tests/fuzz.rs`), which
//! calls `app().test().fuzz()`, and show its report.
//!
//! The seed and the number of cases reach the test through the
//! `ZAPLINE_FUZZ_SEED` and `ZAPLINE_FUZZ_CASES` environment variables, which
//! `FuzzConfig` reads when the test does not set them itself.

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
    let status = command(project, seed, cases)
        .stdin(Stdio::null())
        .status()
        .context("failed to run `cargo test --test fuzz`")?;
    Ok(status.success())
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
    fn a_project_without_a_fuzz_test_gets_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(&project(dir.path()), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no tests/fuzz.rs"), "{err}");
        assert!(err.contains(".fuzz().await.assert_clean()"), "{err}");
    }
}
