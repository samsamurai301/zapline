//! `zapline doctor`: check that the tools zapline projects rely on are installed.

use std::process::Command;

use anyhow::{bail, Result};

use crate::ui;

/// Oldest Rust toolchain zapline supports.
pub const MIN_RUST: Version = Version(1, 88, 0);

/// A `major.minor.patch` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Finds the first `x.y.z` (or `x.y`) version number in a tool's `--version` output.
pub fn parse_version(output: &str) -> Option<Version> {
    for token in
        output.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '/' | '[' | ']'))
    {
        let token = token.trim_start_matches('v');
        let core = token.split(['-', '+']).next().unwrap_or(token);
        let mut parts = core.split('.');
        let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
            continue;
        };
        let patch = parts.next().unwrap_or("0");
        if let (Ok(a), Ok(b), Ok(c)) = (a.parse(), b.parse(), patch.parse()) {
            return Some(Version(a, b, c));
        }
    }
    None
}

/// How much a missing tool matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Importance {
    /// Nothing works without it.
    Required,
    /// Used by generated projects (CI, scaffolding).
    Recommended,
    /// Only needed for specific commands.
    Optional,
}

/// The state of one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Installed (and new enough); the string is the version line.
    Ok(String),
    /// Installed but older than required.
    TooOld {
        /// The installed version.
        found: Version,
        /// The minimum supported version.
        need: Version,
    },
    /// Not found on `PATH`.
    Missing,
}

/// The result of checking one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Tool name shown to the user.
    pub name: &'static str,
    /// How much it matters.
    pub importance: Importance,
    /// What was found.
    pub status: Status,
    /// What the tool is needed for, shown when it is missing.
    pub purpose: &'static str,
}

impl Check {
    /// Whether this check should make `zapline doctor` fail.
    pub fn is_blocking(&self) -> bool {
        self.importance == Importance::Required && !matches!(self.status, Status::Ok(_))
    }
}

/// Runs `program args...` and returns the first line of stdout, or `None` when
/// the program cannot be started.
fn probe(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        // Some tools print their version to stderr.
        let err = String::from_utf8_lossy(&out.stderr);
        return Some(err.lines().next().unwrap_or("").trim().to_string());
    }
    Some(first.to_string())
}

fn cargo_program() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn rustc_program() -> String {
    std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into())
}

/// Classifies a version line against an optional minimum.
pub fn classify(line: Option<String>, min: Option<Version>) -> Status {
    match line {
        None => Status::Missing,
        Some(line) => match (parse_version(&line), min) {
            (Some(found), Some(need)) if found < need => Status::TooOld { found, need },
            _ => Status::Ok(line),
        },
    }
}

/// Runs every check.
pub fn run_checks() -> Vec<Check> {
    let rustc = rustc_program();
    let cargo = cargo_program();
    vec![
        Check {
            name: "rustc",
            importance: Importance::Required,
            status: classify(probe(&rustc, &["--version"]), Some(MIN_RUST)),
            purpose: "compiles your app (install via https://rustup.rs)",
        },
        Check {
            name: "cargo",
            importance: Importance::Required,
            status: classify(probe(&cargo, &["--version"]), Some(MIN_RUST)),
            purpose: "builds and runs your app (install via https://rustup.rs)",
        },
        Check {
            name: "rustfmt",
            importance: Importance::Recommended,
            status: classify(probe("rustfmt", &["--version"]), None),
            purpose: "formats code; CI in generated projects runs `cargo fmt --check` (`rustup component add rustfmt`)",
        },
        Check {
            name: "clippy",
            importance: Importance::Recommended,
            status: classify(probe(&cargo, &["clippy", "--version"]), None),
            purpose: "lints code; CI in generated projects runs `cargo clippy` (`rustup component add clippy`)",
        },
        Check {
            name: "git",
            importance: Importance::Recommended,
            status: classify(probe("git", &["--version"]), None),
            purpose: "`zapline new` initializes a repository",
        },
        Check {
            name: "docker",
            importance: Importance::Optional,
            status: classify(probe("docker", &["--version"]), None),
            purpose: "builds the container image from the generated Dockerfile",
        },
        Check {
            name: "wrk",
            importance: Importance::Optional,
            status: classify(probe("wrk", &["--version"]), None),
            purpose: "load generator for `zapline bench` on Linux/macOS (apt install wrk / brew install wrk)",
        },
        Check {
            name: "oha",
            importance: Importance::Optional,
            status: classify(probe("oha", &["--version"]), None),
            purpose: "cross-platform load generator; `zapline bench` uses it when wrk is missing (cargo install oha)",
        },
    ]
}

/// Formats the checklist.
pub fn render(checks: &[Check], color: bool) -> String {
    let mut out = String::new();
    out.push_str(&ui::bold(color, "zapline doctor"));
    out.push_str("\n\n");
    for c in checks {
        let (tag, style, detail) = match &c.status {
            Status::Ok(line) => {
                // Prefer the bare version number; fall back to the tool's own words.
                let version = parse_version(line).map_or_else(|| line.clone(), |v| v.to_string());
                ("ok", "\x1b[32m", format!("{:<8} {version}", c.name))
            }
            Status::TooOld { found, need } => (
                "fail",
                "\x1b[31m",
                format!(
                    "{} {found} is too old; zapline needs {need} or newer (run `rustup update`)",
                    c.name
                ),
            ),
            Status::Missing => {
                let (tag, style) = match c.importance {
                    Importance::Required => ("fail", "\x1b[31m"),
                    _ => ("warn", "\x1b[33m"),
                };
                let kind = match c.importance {
                    Importance::Required => "required",
                    Importance::Recommended => "recommended",
                    Importance::Optional => "optional",
                };
                (
                    tag,
                    style,
                    format!("{} not found ({kind}): {}", c.name, c.purpose),
                )
            }
        };
        out.push_str(&format!(
            "  {} {detail}\n",
            ui::paint(color, style, format!("{tag:<4}"))
        ));
    }
    out.push('\n');
    let blocking = checks.iter().filter(|c| c.is_blocking()).count();
    if blocking > 0 {
        out.push_str(&ui::paint(
            color,
            "\x1b[31m",
            "Fix the failing checks above before using zapline.\n",
        ));
    } else if checks.iter().any(|c| !matches!(c.status, Status::Ok(_))) {
        out.push_str("Everything required is installed. Optional tools above are only needed for specific features.\n");
    } else {
        out.push_str("Everything looks good.\n");
    }
    out
}

/// Runs the checks, prints the checklist, and fails if a required tool is missing.
pub fn run() -> Result<()> {
    let checks = run_checks();
    print!("{}", render(&checks, ui::stdout_color()));
    if checks.iter().any(Check::is_blocking) {
        bail!("required tools are missing or too old");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_versions() {
        assert_eq!(
            parse_version("rustc 1.97.0 (2d8144b78 2026-07-07)"),
            Some(Version(1, 97, 0))
        );
        assert_eq!(
            parse_version("cargo 1.88.0 (873a06493 2025-05-10)"),
            Some(Version(1, 88, 0))
        );
        assert_eq!(parse_version("git version 2.43.0"), Some(Version(2, 43, 0)));
        assert_eq!(
            parse_version("Docker version 27.1.1, build 6312585"),
            Some(Version(27, 1, 1))
        );
        assert_eq!(
            parse_version("wrk 4.2.0 [epoll] Copyright (C) 2012 Will Glozer"),
            Some(Version(4, 2, 0))
        );
        assert_eq!(
            parse_version("wrk debian/4.1.0-4build2 [epoll] Copyright (C) 2012 Will Glozer"),
            Some(Version(4, 1, 0))
        );
        assert_eq!(
            parse_version("rustfmt 1.8.0-stable (2d8144b78 2026-07-07)"),
            Some(Version(1, 8, 0))
        );
        assert_eq!(
            parse_version("clippy 0.1.97 (2d8144b78 2026-07-07)"),
            Some(Version(0, 1, 97))
        );
        assert_eq!(
            parse_version("rustc 1.90.0-nightly (abc 2025-01-01)"),
            Some(Version(1, 90, 0))
        );
        assert_eq!(parse_version("tool v3.4"), Some(Version(3, 4, 0)));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(Version(1, 9, 0) < Version(1, 88, 0));
        assert!(Version(1, 88, 0) >= MIN_RUST);
        assert!(Version(1, 87, 9) < MIN_RUST);
        assert!(Version(2, 0, 0) > MIN_RUST);
        assert_eq!(MIN_RUST.to_string(), "1.88.0");
    }

    #[test]
    fn classifies_probe_results() {
        assert_eq!(classify(None, Some(MIN_RUST)), Status::Missing);
        assert_eq!(
            classify(Some("rustc 1.70.0 (x)".into()), Some(MIN_RUST)),
            Status::TooOld {
                found: Version(1, 70, 0),
                need: MIN_RUST
            }
        );
        assert!(matches!(
            classify(Some("rustc 1.88.0 (x)".into()), Some(MIN_RUST)),
            Status::Ok(_)
        ));
        assert!(matches!(
            classify(Some("git version 2.0.0".into()), None),
            Status::Ok(_)
        ));
        // Unparseable output from an installed tool still counts as installed.
        assert!(matches!(
            classify(Some("weird".into()), Some(MIN_RUST)),
            Status::Ok(_)
        ));
    }

    fn check(name: &'static str, importance: Importance, status: Status) -> Check {
        Check {
            name,
            importance,
            status,
            purpose: "testing",
        }
    }

    #[test]
    fn only_required_failures_block() {
        assert!(check("rustc", Importance::Required, Status::Missing).is_blocking());
        assert!(check(
            "rustc",
            Importance::Required,
            Status::TooOld {
                found: Version(1, 0, 0),
                need: MIN_RUST
            }
        )
        .is_blocking());
        assert!(!check(
            "rustc",
            Importance::Required,
            Status::Ok("rustc 1.97.0".into())
        )
        .is_blocking());
        assert!(!check("docker", Importance::Optional, Status::Missing).is_blocking());
        assert!(!check("git", Importance::Recommended, Status::Missing).is_blocking());
    }

    #[test]
    fn renders_a_checklist() {
        let checks = [
            check(
                "rustc",
                Importance::Required,
                Status::Ok("rustc 1.97.0 (x)".into()),
            ),
            check("git", Importance::Recommended, Status::Missing),
            check("wrk", Importance::Optional, Status::Missing),
        ];
        let text = render(&checks, false);
        assert!(text.starts_with("zapline doctor\n\n"), "{text}");
        assert!(text.contains("  ok   rustc    1.97.0\n"), "{text}");
        assert!(
            text.contains("  warn git not found (recommended): testing\n"),
            "{text}"
        );
        assert!(
            text.contains("  warn wrk not found (optional): testing\n"),
            "{text}"
        );
        assert!(text.contains("Everything required is installed."), "{text}");
    }

    #[test]
    fn renders_failures() {
        let checks = [
            check(
                "rustc",
                Importance::Required,
                Status::TooOld {
                    found: Version(1, 70, 0),
                    need: MIN_RUST,
                },
            ),
            check("cargo", Importance::Required, Status::Missing),
        ];
        let text = render(&checks, false);
        assert!(
            text.contains("fail rustc 1.70.0 is too old; zapline needs 1.88.0 or newer"),
            "{text}"
        );
        assert!(text.contains("fail cargo not found (required)"), "{text}");
        assert!(text.contains("Fix the failing checks"), "{text}");
    }

    #[test]
    fn all_good_message() {
        let checks = [check(
            "rustc",
            Importance::Required,
            Status::Ok("rustc 1.97.0".into()),
        )];
        assert!(render(&checks, false).ends_with("Everything looks good.\n"));
    }

    #[test]
    fn the_real_checks_find_the_toolchain_running_the_tests() {
        let checks = run_checks();
        let rustc = checks.iter().find(|c| c.name == "rustc").unwrap();
        assert!(matches!(rustc.status, Status::Ok(_)), "{rustc:?}");
        let cargo = checks.iter().find(|c| c.name == "cargo").unwrap();
        assert!(matches!(cargo.status, Status::Ok(_)), "{cargo:?}");
    }
}
