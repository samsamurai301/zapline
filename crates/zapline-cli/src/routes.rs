//! `zapline routes`: print the route table of the app.
//!
//! The app is run with `ZAPLINE_PRINT_ROUTES=1`; `App::run` then prints
//! `[{"method":"GET","path":"/x"}, ...]` to stdout and exits.

use std::ffi::OsStr;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::build;
use crate::project::Project;
use crate::ui;

/// Environment variable that makes `App::run` print its routes and exit.
pub const ENV_PRINT_ROUTES: &str = "ZAPLINE_PRINT_ROUTES";

/// How long the app may take to print its routes once built.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// One route as printed by the app.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RouteEntry {
    /// HTTP method, or `ANY`.
    pub method: String,
    /// Path pattern such as `/users/{id}`.
    pub path: String,
}

/// Extracts the route list from the app's stdout.
///
/// The JSON is normally the only output, but apps may print other lines first
/// (for example from `println!` in `main`), so the last line that parses as a
/// route array wins.
pub fn parse_routes_json(stdout: &str) -> Result<Vec<RouteEntry>> {
    if let Ok(routes) = serde_json::from_str::<Vec<RouteEntry>>(stdout.trim()) {
        return Ok(routes);
    }
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| l.starts_with('['))
        .find_map(|l| serde_json::from_str::<Vec<RouteEntry>>(l).ok())
        .context("the app did not print a route table")
}

/// The raw JSON line, for `--json`.
fn raw_json_line(stdout: &str) -> Option<&str> {
    let trimmed = stdout.trim();
    if serde_json::from_str::<Vec<RouteEntry>>(trimmed).is_ok() {
        return Some(trimmed);
    }
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('[') && serde_json::from_str::<Vec<RouteEntry>>(l).is_ok())
}

fn method_rank(method: &str) -> usize {
    ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
        .iter()
        .position(|m| *m == method)
        .unwrap_or(usize::MAX)
}

/// Renders an aligned `METHOD  PATH` table, sorted by path then method.
pub fn render_table(entries: &[RouteEntry], color: bool) -> String {
    if entries.is_empty() {
        return "No routes registered.\n".to_string();
    }
    let mut rows: Vec<&RouteEntry> = entries.iter().collect();
    rows.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(method_rank(&a.method).cmp(&method_rank(&b.method)))
            .then(a.method.cmp(&b.method))
    });
    let width = rows
        .iter()
        .map(|r| r.method.len())
        .max()
        .unwrap_or(0)
        .max("METHOD".len());

    let mut out = String::new();
    out.push_str(&ui::bold(color, format!("{:<width$}  PATH", "METHOD")));
    out.push('\n');
    for r in rows {
        let padded = format!("{:<width$}", r.method);
        out.push_str(&ui::paint(color, ui::method_color(&r.method), padded));
        out.push_str("  ");
        out.push_str(&r.path);
        out.push('\n');
    }
    let n = entries.len();
    out.push_str(&ui::dim(
        color,
        format!("\n{n} route{}", if n == 1 { "" } else { "s" }),
    ));
    out.push('\n');
    out
}

/// Runs the app and returns its route table: the parsed entries and the
/// app's own JSON line.
pub fn capture(project: &Project, bin: Option<&str>) -> Result<(Vec<RouteEntry>, String)> {
    let captured = build::run_app(
        project,
        bin,
        &[(ENV_PRINT_ROUTES, OsStr::new("1"))],
        RUN_TIMEOUT,
    )?;
    if !captured.status.success() {
        return Err(captured.failure("before printing its routes"));
    }
    let entries = parse_routes_json(&captured.stdout).context(
        "the app did not print a route table\n\
         hint: `main` must call `App::run`, which honours ZAPLINE_PRINT_ROUTES=1",
    )?;
    let raw = raw_json_line(&captured.stdout).unwrap_or("[]").to_string();
    Ok((entries, raw))
}

/// Runs the app and prints its routes.
pub fn run(project: &Project, bin: Option<&str>, json: bool) -> Result<()> {
    let (entries, raw) = capture(project, bin)?;
    if json {
        // Pass the app's own JSON through untouched.
        println!("{raw}");
    } else {
        print!("{}", render_table(&entries, ui::stdout_color()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(method: &str, path: &str) -> RouteEntry {
        RouteEntry {
            method: method.into(),
            path: path.into(),
        }
    }

    #[test]
    fn parses_the_printed_json() {
        let out = r#"[{"method":"GET","path":"/health"},{"method":"POST","path":"/users/{id}"}]"#;
        assert_eq!(
            parse_routes_json(out).unwrap(),
            vec![entry("GET", "/health"), entry("POST", "/users/{id}")]
        );
        assert_eq!(parse_routes_json("  []\n").unwrap(), vec![]);
    }

    #[test]
    fn tolerates_other_output_before_the_json() {
        let out = "starting up...\nconfig loaded\n[{\"method\":\"GET\",\"path\":\"/\"}]\n";
        assert_eq!(parse_routes_json(out).unwrap(), vec![entry("GET", "/")]);
        assert_eq!(
            raw_json_line(out),
            Some("[{\"method\":\"GET\",\"path\":\"/\"}]")
        );
    }

    #[test]
    fn ignores_unknown_fields_and_escaped_paths() {
        let out = r#"[{"method":"ANY","path":"/a\"b","summary":"x"}]"#;
        assert_eq!(parse_routes_json(out).unwrap(), vec![entry("ANY", "/a\"b")]);
    }

    #[test]
    fn rejects_output_without_a_route_table() {
        assert!(parse_routes_json("").is_err());
        assert!(parse_routes_json("hello\nworld\n").is_err());
        assert!(parse_routes_json("[1,2,3]").is_err());
        assert!(parse_routes_json("[{\"method\":\"GET\"}]").is_err());
    }

    #[test]
    fn table_is_aligned_and_sorted() {
        let table = render_table(
            &[
                entry("POST", "/users"),
                entry("GET", "/users"),
                entry("DELETE", "/users/{id}"),
                entry("GET", "/health"),
                entry("ANY", "/zzz"),
            ],
            false,
        );
        assert_eq!(
            table,
            "\
METHOD  PATH
GET     /health
GET     /users
POST    /users
DELETE  /users/{id}
ANY     /zzz

5 routes
"
        );
    }

    #[test]
    fn table_pads_to_the_longest_method() {
        let table = render_table(&[entry("OPTIONS", "/a"), entry("GET", "/b")], false);
        assert!(table.starts_with("METHOD   PATH\n"), "{table}");
        assert!(table.contains("OPTIONS  /a\n"), "{table}");
        assert!(table.contains("GET      /b\n"), "{table}");
        assert!(table.ends_with("2 routes\n"));
    }

    #[test]
    fn singular_and_empty() {
        assert!(render_table(&[entry("GET", "/")], false).ends_with("1 route\n"));
        assert_eq!(render_table(&[], false), "No routes registered.\n");
    }

    #[test]
    fn colored_table_pads_before_painting() {
        let table = render_table(&[entry("GET", "/a"), entry("DELETE", "/b")], true);
        // The visible text is padded to the same width, escape codes excluded.
        assert!(table.contains("\x1b[32mGET   \x1b[0m  /a"), "{table:?}");
    }
}
