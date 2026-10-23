//! `zapline check` and `zapline test`: cargo's results, condensed.
//!
//! Raw `cargo check`/`cargo test` output is long: progress lines, ASCII art
//! around every diagnostic, the output of every passing test. People skim
//! it; AI assistants pay for every line of it. These commands keep what you
//! act on: one line per error or warning (`file:line:col`, code, message,
//! the first help), the failing tests with the start of their panic message,
//! and a one-line summary.
//!
//! A "not found" error for something behind one of zapline's features that
//! the project has not enabled (`use zapline::ws::..` without `ws`) gets a
//! `hint: enable the `ws` feature: zapline add ws` line.

use std::collections::BTreeSet;
use std::io::Read;
use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::build::cargo;
use crate::features;
use crate::project::Project;

/// One compiler diagnostic, condensed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// `error` or `warning`.
    pub level: String,
    /// `E0308`, `clippy::needless_borrow`, ...
    pub code: Option<String>,
    /// The headline.
    pub message: String,
    /// `src/lib.rs:12:5`.
    pub location: Option<String>,
    /// Primary span label (`expected i64, found String`) and the first help.
    pub notes: Vec<String>,
}

impl Diagnostic {
    /// `error[E0308] src/lib.rs:12:5: mismatched types (expected i64, found String)`.
    pub fn line(&self) -> String {
        let mut out = self.level.clone();
        if let Some(code) = &self.code {
            out.push_str(&format!("[{code}]"));
        }
        if let Some(loc) = &self.location {
            out.push(' ');
            out.push_str(loc);
        }
        out.push_str(": ");
        out.push_str(&self.message);
        for note in &self.notes {
            out.push_str("\n    ");
            out.push_str(note);
        }
        out
    }
}

/// Parses one `--message-format=json` line into a diagnostic, if it is one.
pub fn parse_diagnostic(line: &str) -> Option<Diagnostic> {
    let msg: Value = serde_json::from_str(line).ok()?;
    if msg.get("reason")?.as_str()? != "compiler-message" {
        return None;
    }
    let m = msg.get("message")?;
    let level = m.get("level")?.as_str()?.to_string();
    if level != "error" && level != "warning" {
        return None;
    }
    let message = m.get("message")?.as_str()?.to_string();
    // "aborting due to N previous errors" and warning counts are noise.
    if message.starts_with("aborting due to")
        || message.contains("warnings emitted")
        || message.ends_with("warning emitted")
    {
        return None;
    }
    let code = m
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let spans = m.get("spans").and_then(Value::as_array);
    let primary = spans.and_then(|s| {
        s.iter()
            .find(|sp| sp.get("is_primary").and_then(Value::as_bool) == Some(true))
    });
    let location = primary.and_then(|sp| {
        Some(format!(
            "{}:{}:{}",
            sp.get("file_name")?.as_str()?,
            sp.get("line_start")?.as_u64()?,
            sp.get("column_start")?.as_u64()?
        ))
    });
    let mut notes = Vec::new();
    if let Some(label) = primary
        .and_then(|sp| sp.get("label"))
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty())
    {
        notes.push(label.to_string());
    }
    if let Some(help) = m.get("children").and_then(Value::as_array).and_then(|c| {
        c.iter().find(|ch| {
            ch.get("level").and_then(Value::as_str) == Some("help")
                && ch.get("message").and_then(Value::as_str).is_some()
        })
    }) {
        let text = help
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let suggestion = help
            .get("spans")
            .and_then(Value::as_array)
            .and_then(|s| s.first())
            .and_then(|sp| sp.get("suggested_replacement"))
            .and_then(Value::as_str);
        notes.push(match suggestion {
            Some(s) if !s.is_empty() && s.len() < 120 => format!("help: {text}: `{s}`"),
            _ => format!("help: {text}"),
        });
    }
    Some(Diagnostic {
        level,
        code,
        message,
        location,
        notes,
    })
}

/// Error codes for names rustc could not find: unresolved imports and
/// paths, unknown types and values, missing methods.
const NOT_FOUND_CODES: &[&str] = &["E0432", "E0433", "E0412", "E0425", "E0599"];

/// The `hint: enable the `ws` feature: zapline add ws` line for a "not
/// found" error about something behind zapline features that are all off in
/// `enabled` (see [`features::enabled`]).
///
/// ```
/// use zapline_cli::brief::{feature_hint, Diagnostic};
/// let d = Diagnostic {
///     level: "error".into(),
///     code: Some("E0432".into()),
///     message: "unresolved import `zapline::ws`".into(),
///     location: None,
///     notes: vec![],
/// };
/// let on = zapline_cli::features::enabled("[dependencies]\nzapline = \"0.1\"\n").unwrap();
/// assert_eq!(
///     feature_hint(&d, &on).as_deref(),
///     Some("hint: enable the `ws` feature: zapline add ws")
/// );
/// ```
pub fn feature_hint(d: &Diagnostic, enabled: &BTreeSet<String>) -> Option<String> {
    let code = d.code.as_deref().filter(|c| NOT_FOUND_CODES.contains(c))?;
    let mut text = d.message.clone();
    for note in &d.notes {
        text.push('\n');
        text.push_str(note);
    }
    let candidates = features::missing_for(code, &text);
    // If one of them is on, the error is about something else.
    if candidates.is_empty() || candidates.iter().any(|f| enabled.contains(f.name)) {
        return None;
    }
    let names: Vec<String> = candidates.iter().map(|f| format!("`{}`", f.name)).collect();
    Some(format!(
        "hint: enable the {} feature: zapline add {}",
        names.join(" or "),
        candidates[0].name
    ))
}

/// Appends [`feature_hint`] to each error it applies to. `manifest` is the
/// project's Cargo.toml; without a `zapline` dependency whose features it
/// declares, nothing is added.
///
/// ```
/// use zapline_cli::brief::{add_feature_hints, Diagnostic};
/// let mut diags = vec![Diagnostic {
///     level: "error".into(),
///     code: Some("E0599".into()),
///     message: "no method named `views` found for struct `App` in the current scope".into(),
///     location: None,
///     notes: vec![],
/// }];
/// add_feature_hints(&mut diags, "[dependencies]\nzapline = \"0.1\"\n");
/// assert_eq!(diags[0].notes, ["hint: enable the `views` feature: zapline add views"]);
/// ```
pub fn add_feature_hints(diags: &mut [Diagnostic], manifest: &str) {
    let Some(enabled) = features::enabled(manifest) else {
        return;
    };
    for d in diags.iter_mut().filter(|d| d.level == "error") {
        if let Some(hint) = feature_hint(d, &enabled) {
            d.notes.push(hint);
        }
    }
}

/// Runs `cargo <args> --message-format=json` and collects unique diagnostics.
fn diagnostics(project: &Project, args: &[&str]) -> Result<(bool, Vec<Diagnostic>)> {
    let output = cargo()
        .args(args)
        .arg("--message-format=json")
        .current_dir(&project.root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("failed to run `cargo {}`", args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found: Vec<Diagnostic> = Vec::new();
    for d in stdout.lines().filter_map(parse_diagnostic) {
        // The same diagnostic is reported once per target (lib, tests...).
        if !found.contains(&d) {
            found.push(d);
        }
    }
    if let Ok(manifest) = std::fs::read_to_string(project.root.join("Cargo.toml")) {
        add_feature_hints(&mut found, &manifest);
    }
    Ok((output.status.success(), found))
}

/// `zapline check`: errors and warnings of the whole project, tests included.
pub fn check(project: &Project, clippy: bool) -> Result<(bool, String)> {
    let args: &[&str] = if clippy {
        &["clippy", "--all-targets", "--quiet"]
    } else {
        &["check", "--all-targets", "--quiet"]
    };
    let (ok, diags) = diagnostics(project, args)?;
    let errors = diags.iter().filter(|d| d.level == "error").count();
    let warnings = diags.len() - errors;
    let mut out = String::new();
    for d in &diags {
        out.push_str(&d.line());
        out.push('\n');
    }
    out.push_str(&if ok && diags.is_empty() {
        "ok: no errors, no warnings".to_string()
    } else {
        format!(
            "{}: {errors} error{}, {warnings} warning{}",
            if ok { "ok" } else { "failed" },
            plural(errors),
            plural(warnings)
        )
    });
    out.push('\n');
    Ok((ok, out))
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Totals and failures from `cargo test` output.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TestReport {
    /// Passed tests.
    pub passed: u64,
    /// Failed tests.
    pub failed: u64,
    /// Ignored tests.
    pub ignored: u64,
    /// `(test name, condensed failure output)`.
    pub failures: Vec<(String, String)>,
}

/// Lines kept from a failing test's output.
const FAILURE_LINES: usize = 8;

/// Parses the human-readable output of `cargo test` (stdout of the test
/// binaries).
pub fn parse_test_output(text: &str) -> TestReport {
    let mut report = TestReport::default();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("test result: ") {
            for part in rest.split(';') {
                let mut words = part.split_whitespace();
                let (Some(n), Some(what)) = (words.next(), words.next()) else {
                    continue;
                };
                let n = n
                    .trim_start_matches("ok.")
                    .trim_start_matches("FAILED.")
                    .trim();
                let n: u64 = match n.parse() {
                    Ok(n) => n,
                    // "ok. 3 passed": the number is the second word.
                    Err(_) => match what.parse() {
                        Ok(n) => {
                            if let Some(kind) = words.next() {
                                tally(&mut report, kind, n);
                            }
                            continue;
                        }
                        Err(_) => continue,
                    },
                };
                tally(&mut report, what, n);
            }
        } else if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|l| l.strip_suffix(" stdout ----"))
        {
            let mut body: Vec<&str> = Vec::new();
            while let Some(next) = lines.peek() {
                if next.starts_with("---- ")
                    || next.starts_with("failures:")
                    || next.is_empty() && body.len() >= FAILURE_LINES
                {
                    break;
                }
                let next = lines.next().unwrap_or_default();
                if next.starts_with("note: run with `RUST_BACKTRACE") || next.trim().is_empty() {
                    continue;
                }
                if body.len() < FAILURE_LINES {
                    body.push(next);
                }
            }
            report.failures.push((name.to_string(), body.join("\n")));
        }
    }
    report
}

fn tally(report: &mut TestReport, kind: &str, n: u64) {
    match kind.trim_end_matches([',', ';']) {
        "passed" => report.passed += n,
        "failed" => report.failed += n,
        "ignored" => report.ignored += n,
        _ => {}
    }
}

/// `zapline test [filter]`: compile errors if the tests don't build,
/// otherwise the totals and each failure.
pub fn test(project: &Project, filter: Option<&str>) -> Result<(bool, String)> {
    // The filter may come from an MCP client: it names tests, never flags
    // (`--config` would let it run any command).
    if filter.is_some_and(|f| f.starts_with('-')) {
        anyhow::bail!("a test filter can't start with '-'");
    }
    // Compile first so build errors come back condensed, not as a wall of text.
    let (built, diags) = diagnostics(project, &["test", "--no-run", "--quiet"])?;
    if !built {
        let mut out = String::from("tests do not compile:\n");
        for d in diags.iter().filter(|d| d.level == "error") {
            out.push_str(&d.line());
            out.push('\n');
        }
        return Ok((false, out));
    }
    let mut cmd = cargo();
    cmd.args(["test", "--quiet", "--no-fail-fast"]);
    match filter {
        // `zapline test users` runs tests/users.rs when there is one.
        Some(f)
            if f.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                && project.root.join("tests").join(format!("{f}.rs")).is_file() =>
        {
            cmd.args(["--test", f]);
        }
        Some(f) => {
            cmd.args(["--", f]);
        }
        None => {}
    }
    let mut child = cmd
        .current_dir(&project.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("RUST_BACKTRACE", "0")
        .spawn()
        .context("failed to run `cargo test`")?;
    let mut text = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut text).ok();
    }
    let status = child.wait().context("failed to wait for cargo test")?;
    let report = parse_test_output(&text);
    let mut out = String::new();
    for (name, body) in &report.failures {
        out.push_str(&format!("FAILED {name}\n"));
        for l in body.lines() {
            out.push_str("    ");
            out.push_str(l.trim_end());
            out.push('\n');
        }
    }
    out.push_str(&format!(
        "{}: {} passed, {} failed, {} ignored\n",
        if status.success() { "ok" } else { "failed" },
        report.passed,
        report.failed,
        report.ignored
    ));
    if report.passed + report.failed == 0 {
        if let Some(f) = filter {
            out.push_str(&format!(
                "hint: no test name contains `{f}`; pass part of a test name or a tests/<file> name\n"
            ));
        }
    }
    Ok((status.success(), out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filters_cannot_be_cargo_flags() {
        let project = Project {
            root: std::path::PathBuf::from("/nonexistent"),
            package_name: "demo".into(),
            lib_name: None,
            bins: vec![],
            default_run: None,
            edition: "2021".into(),
            dependencies: Default::default(),
            dev_dependencies: Default::default(),
        };
        let err = test(&project, Some("--config=build.rustc-wrapper='sh'")).unwrap_err();
        assert!(err.to_string().contains("can't start with '-'"), "{err}");
    }

    #[test]
    fn diagnostics_are_condensed_to_a_line_and_notes() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/lib.rs","line_start":12,"column_start":5,"is_primary":true,"label":"expected `i64`, found `String`"}],"children":[{"level":"help","message":"try converting","spans":[{"suggested_replacement":"x.parse()?"}]}]}}"#;
        let d = parse_diagnostic(line).unwrap();
        assert_eq!(
            d.line(),
            "error[E0308] src/lib.rs:12:5: mismatched types\n    expected `i64`, found `String`\n    help: try converting: `x.parse()?`"
        );
        let noise = r#"{"reason":"compiler-message","message":{"level":"error","message":"aborting due to 2 previous errors","spans":[],"children":[]}}"#;
        assert!(parse_diagnostic(noise).is_none());
        assert!(parse_diagnostic(r#"{"reason":"compiler-artifact"}"#).is_none());
    }

    /// Recorded from `cargo check --message-format=json` on a project that
    /// uses `zapline::ws` without the feature (explanation shortened).
    const UNRESOLVED_WS: &str = r##"{"reason":"compiler-message","message":{"rendered":null,"$message_type":"diagnostic","children":[{"children":[],"code":null,"level":"note","message":"found an item that was configured out","rendered":null,"spans":[{"byte_end":6834,"byte_start":6820,"column_end":21,"column_start":7,"expansion":null,"file_name":"/zapline/src/lib.rs","is_primary":false,"label":"the item is gated behind the `ws` feature","line_end":147,"line_start":147,"suggested_replacement":null,"suggestion_applicability":null,"text":[{"highlight_end":21,"highlight_start":7,"text":"#[cfg(feature = \"ws\")]"}]},{"byte_end":6847,"byte_start":6845,"column_end":11,"column_start":9,"expansion":null,"file_name":"/zapline/src/lib.rs","is_primary":true,"label":null,"line_end":148,"line_start":148,"suggested_replacement":null,"suggestion_applicability":null,"text":[{"highlight_end":11,"highlight_start":9,"text":"pub mod ws;"}]}]}],"level":"error","message":"unresolved import `zapline::ws`","spans":[{"byte_end":15,"byte_start":13,"column_end":16,"column_start":14,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":"could not find `ws` in `zapline`","line_end":1,"line_start":1,"suggested_replacement":null,"suggestion_applicability":null,"text":[{"highlight_end":16,"highlight_start":14,"text":"use zapline::ws::WebSocketUpgrade;"}]}],"code":{"code":"E0432","explanation":"An import was unresolved.\n"}}}"##;

    #[test]
    fn a_missing_feature_gets_a_hint() {
        let mut diags = vec![parse_diagnostic(UNRESOLVED_WS).unwrap()];
        add_feature_hints(&mut diags, "[dependencies]\nzapline = \"0.1\"\n");
        assert_eq!(
            diags[0].line(),
            "error[E0432] src/lib.rs:1:14: unresolved import `zapline::ws`\n    could not find `ws` in `zapline`\n    hint: enable the `ws` feature: zapline add ws"
        );

        // Already on: the error is about something else.
        let mut diags = vec![parse_diagnostic(UNRESOLVED_WS).unwrap()];
        add_feature_hints(
            &mut diags,
            "[dependencies]\nzapline = { version = \"0.1\", features = [\"full\"] }\n",
        );
        assert_eq!(diags[0].notes, ["could not find `ws` in `zapline`"]);

        // Not a zapline project: no hints.
        let mut diags = vec![parse_diagnostic(UNRESOLVED_WS).unwrap()];
        add_feature_hints(&mut diags, "[dependencies]\nserde = \"1\"\n");
        assert_eq!(diags[0].notes.len(), 1);
    }

    #[test]
    fn hints_for_types_methods_and_either_database() {
        let on = features::enabled("[dependencies]\nzapline = \"0.1\"\n").unwrap();
        let diag = |code: &str, message: &str, label: &str| Diagnostic {
            level: "error".into(),
            code: Some(code.into()),
            message: message.into(),
            location: None,
            notes: vec![label.into()],
        };
        let hint = |d: &Diagnostic| feature_hint(d, &on);
        // Labels and messages as rustc 1.9x prints them.
        assert_eq!(
            hint(&diag(
                "E0425",
                "cannot find type `Db` in this scope",
                "not found in this scope"
            ))
            .as_deref(),
            Some("hint: enable the `sqlite` or `postgres` feature: zapline add sqlite")
        );
        assert_eq!(
            hint(&diag(
                "E0599",
                "no method named `views` found for struct `zapline::App` in the current scope",
                "method not found in `zapline::App`"
            ))
            .as_deref(),
            Some("hint: enable the `views` feature: zapline add views")
        );
        assert_eq!(
            hint(&diag(
                "E0433",
                "cannot find `cache` in `zapline`",
                "could not find `cache` in `zapline`"
            ))
            .as_deref(),
            Some("hint: enable the `cache` feature: zapline add cache")
        );
        assert_eq!(
            hint(&diag(
                "E0432",
                "unresolved import `zapline::auth::Jwt`",
                "no `Jwt` in `auth`"
            ))
            .as_deref(),
            Some("hint: enable the `jwt` feature: zapline add jwt")
        );
        assert_eq!(
            hint(&diag("E0308", "mismatched types", "expected `WebSocket`")),
            None
        );
        // `Json` is a default feature, so this is some other problem.
        assert_eq!(
            hint(&diag("E0425", "cannot find type `Json` in this scope", "")),
            None
        );
    }

    #[test]
    fn test_output_is_summarized() {
        let text = "\
running 3 tests
...F
failures:

---- users::rejects_bad_email stdout ----

thread 'users::rejects_bad_email' panicked at tests/users.rs:31:5:
assertion `left == right` failed
  left: 201
 right: 422
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

failures:
    users::rejects_bad_email

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

running 1 test
.
test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let r = parse_test_output(text);
        assert_eq!((r.passed, r.failed, r.ignored), (3, 1, 1));
        assert_eq!(r.failures.len(), 1);
        assert_eq!(r.failures[0].0, "users::rejects_bad_email");
        assert!(r.failures[0].1.contains("left: 201"));
        assert!(!r.failures[0].1.contains("RUST_BACKTRACE"));
    }
}
