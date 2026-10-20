//! Helpers shared by unit tests.

use std::io::Write;
use std::process::{Command, Stdio};

/// Whether `rustfmt` can be run (tests that need it are skipped otherwise).
pub fn rustfmt_available() -> bool {
    Command::new("rustfmt").arg("--version").output().is_ok()
}

/// Asserts that `source` parses and is already formatted exactly as rustfmt
/// would format it (default config, edition 2021).
pub fn assert_rustfmt_clean(label: &str, source: &str) {
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2021", "--emit", "stdout", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rustfmt starts");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(source.as_bytes())
        .expect("write to rustfmt");
    let out = child.wait_with_output().expect("rustfmt finishes");
    assert!(
        out.status.success(),
        "{label} does not parse:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let formatted = String::from_utf8_lossy(&out.stdout);
    if formatted != source {
        panic!(
            "{label} is not rustfmt-clean:\n{}",
            first_difference(source, &formatted)
        );
    }
}

fn first_difference(have: &str, want: &str) -> String {
    for (i, (h, w)) in have.lines().zip(want.lines()).enumerate() {
        if h != w {
            return format!("line {}:\n  have: {h}\n  want: {w}", i + 1);
        }
    }
    format!(
        "line counts differ ({} vs {})",
        have.lines().count(),
        want.lines().count()
    )
}
