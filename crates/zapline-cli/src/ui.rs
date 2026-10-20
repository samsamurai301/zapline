//! Terminal output: cargo-style status lines with optional ANSI colour.
//!
//! Status and diagnostics go to stderr; data (route tables, JSON, benchmark
//! summaries) goes to stdout so it can be piped. Colour is only used on a TTY,
//! never when `NO_COLOR` is set, and can be forced with `CLICOLOR_FORCE`.

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::OnceLock;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";

/// Width the verb column of a status line is right-aligned to.
const VERB_WIDTH: usize = 12;

/// Decides whether to emit ANSI colour, given whether the stream is a TTY and
/// an environment lookup. Pure so it can be unit tested.
pub fn color_enabled_for(is_tty: bool, env: impl Fn(&str) -> Option<String>) -> bool {
    if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if env("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    if !is_tty || env("TERM").as_deref() == Some("dumb") {
        return false;
    }
    if cfg!(windows) {
        // Legacy consoles do not understand ANSI; modern terminals announce themselves.
        return ["WT_SESSION", "ANSICON", "TERM"]
            .iter()
            .any(|k| env(k).is_some());
    }
    true
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Whether stderr output should be coloured (computed once).
pub fn stderr_color() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| color_enabled_for(std::io::stderr().is_terminal(), env_var))
}

/// Whether stdout output should be coloured (computed once).
pub fn stdout_color() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| color_enabled_for(std::io::stdout().is_terminal(), env_var))
}

/// Wraps `text` in an ANSI style when `color` is on.
pub fn paint(color: bool, style: &str, text: impl Display) -> String {
    if color {
        format!("{style}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// A cargo-style status line: `    Created project ...` with a bold green verb.
pub fn format_status(color: bool, verb: &str, msg: impl Display) -> String {
    let verb = format!("{verb:>VERB_WIDTH$}");
    format!("{} {msg}", paint(color, &format!("{BOLD}{GREEN}"), verb))
}

/// Prints a status line to stderr.
pub fn status(verb: &str, msg: impl Display) {
    eprintln!("{}", format_status(stderr_color(), verb, msg));
}

/// A status line whose verb is highlighted as a warning.
pub fn format_status_with(color: bool, style: &str, verb: &str, msg: impl Display) -> String {
    let verb = format!("{verb:>VERB_WIDTH$}");
    format!("{} {msg}", paint(color, &format!("{BOLD}{style}"), verb))
}

/// Prints a yellow status line (for example `Waiting`) to stderr.
pub fn status_warn(verb: &str, msg: impl Display) {
    eprintln!("{}", format_status_with(stderr_color(), YELLOW, verb, msg));
}

/// Prints a red status line to stderr.
pub fn status_error(verb: &str, msg: impl Display) {
    eprintln!("{}", format_status_with(stderr_color(), RED, verb, msg));
}

/// Prints a cyan status line (informational, e.g. `Watching`) to stderr.
pub fn status_info(verb: &str, msg: impl Display) {
    eprintln!("{}", format_status_with(stderr_color(), CYAN, verb, msg));
}

/// `warning: ...` on stderr.
pub fn warn(msg: impl Display) {
    eprintln!(
        "{} {msg}",
        paint(stderr_color(), &format!("{BOLD}{YELLOW}"), "warning:")
    );
}

/// `note: ...` on stderr, for follow-up instructions.
pub fn note(msg: impl Display) {
    eprintln!(
        "{} {msg}",
        paint(stderr_color(), &format!("{BOLD}{CYAN}"), "note:")
    );
}

/// `error: ...` formatted for stderr.
pub fn format_error(color: bool, msg: impl Display) -> String {
    format!("{} {msg}", paint(color, &format!("{BOLD}{RED}"), "error:"))
}

/// A faint hint line, e.g. under an error.
pub fn dim(color: bool, text: impl Display) -> String {
    paint(color, DIM, text)
}

/// Bold text.
pub fn bold(color: bool, text: impl Display) -> String {
    paint(color, BOLD, text)
}

/// Colours an HTTP method for the route table.
pub fn method_color(method: &str) -> &'static str {
    match method {
        "GET" => GREEN,
        "POST" => BLUE,
        "PUT" => YELLOW,
        "PATCH" => MAGENTA,
        "DELETE" => RED,
        _ => CYAN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn color_requires_a_tty() {
        assert!(!color_enabled_for(false, env(&[])));
        if !cfg!(windows) {
            assert!(color_enabled_for(true, env(&[])));
        }
    }

    #[test]
    fn no_color_wins_over_everything() {
        assert!(!color_enabled_for(true, env(&[("NO_COLOR", "1")])));
        assert!(!color_enabled_for(
            true,
            env(&[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")])
        ));
        // An empty NO_COLOR is ignored (no-color.org).
        if !cfg!(windows) {
            assert!(color_enabled_for(true, env(&[("NO_COLOR", "")])));
        }
    }

    #[test]
    fn clicolor_force_enables_color_without_tty() {
        assert!(color_enabled_for(false, env(&[("CLICOLOR_FORCE", "1")])));
        assert!(!color_enabled_for(false, env(&[("CLICOLOR_FORCE", "0")])));
    }

    #[test]
    fn dumb_terminals_get_no_color() {
        assert!(!color_enabled_for(true, env(&[("TERM", "dumb")])));
    }

    #[test]
    fn status_lines_align_and_paint() {
        assert_eq!(format_status(false, "Created", "x"), "     Created x");
        assert_eq!(format_status(false, "Compiling", "y"), "   Compiling y");
        assert_eq!(
            format_status(true, "Built", "z"),
            "\x1b[1m\x1b[32m       Built\x1b[0m z"
        );
    }

    #[test]
    fn plain_text_is_untouched_without_color() {
        assert_eq!(paint(false, BOLD, "hi"), "hi");
        assert_eq!(format_error(false, "boom"), "error: boom");
        assert_eq!(dim(false, "x"), "x");
        assert_eq!(bold(false, "x"), "x");
    }

    #[test]
    fn methods_have_stable_colors() {
        assert_eq!(method_color("GET"), GREEN);
        assert_eq!(method_color("DELETE"), RED);
        assert_eq!(method_color("ANY"), CYAN);
    }
}
