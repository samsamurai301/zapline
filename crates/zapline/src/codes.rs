//! Stable diagnostic codes for the mistakes developers (and AI assistants)
//! make while building an app: a missing provider, a duplicate route, a
//! config key nobody set, a template that does not parse.
//!
//! Each code (`ZL0002`) has a title, an explanation and the fix, written as
//! the code to add. They appear in startup errors and panics
//! (`startup failed: no provider registered for `app::Repo` [ZL0002]`), on
//! the [dev error page](crate::App::dev_errors), in the
//! [dev console](crate::dev)'s JSON, and in `zapline explain ZL0002`. A code
//! is never renumbered or reused, so it is safe to search for and to match
//! on.
//!
//! ```
//! let code = zapline::codes::lookup("ZL0002").unwrap();
//! assert_eq!(code.title, "No provider for a dependency");
//! assert!(code.fix.contains(".provide("));
//! ```
//!
//! Codes are diagnostics, not part of the JSON clients see: an error's
//! client-facing `code` (`missing_dependency`, `invalid_json`) is unchanged.
//! Read an error's diagnostic code with [`Error::diagnostic_code`](crate::Error::diagnostic_code).

/// One entry of the registry.
///
/// ```
/// let c = zapline::codes::lookup("ZL0044").unwrap();
/// println!("{}: {}\n{}\nfix: {}", c.code, c.title, c.explanation, c.fix);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code {
    /// `ZL` and four digits, e.g. `"ZL0044"`.
    pub code: &'static str,
    /// A few words, e.g. `"No database registered"`.
    pub title: &'static str,
    /// What causes it.
    pub explanation: &'static str,
    /// The code to add. `{T}` stands for what the error names (a type, a
    /// route, a config key); see [`Code::fix_for`].
    pub fix: &'static str,
}

impl Code {
    /// The fix with `{T}` replaced by `subject` (a type name, a route, a
    /// config key), or by `T` when there is none.
    ///
    /// ```
    /// let c = zapline::codes::lookup("ZL0001").unwrap();
    /// assert!(c.fix_for(Some("my_app::Db")).contains("value: my_app::Db"));
    /// assert!(c.fix_for(None).contains("value: T"));
    /// ```
    pub fn fix_for(&self, subject: Option<&str>) -> String {
        self.fix.replace("{T}", subject.unwrap_or("T"))
    }
}

/// Every code, in order.
///
/// ```
/// assert!(zapline::codes::ALL.iter().all(|c| c.code.starts_with("ZL")));
/// ```
pub const ALL: &[Code] = include!("codes.in");

/// The entry for `code`. Accepts `ZL0002`, `zl0002`, `ZL2` and `2`.
///
/// ```
/// assert_eq!(zapline::codes::lookup("zl2").unwrap().code, "ZL0002");
/// assert!(zapline::codes::lookup("ZL9999").is_none());
/// ```
pub fn lookup(code: &str) -> Option<&'static Code> {
    let code = code.trim();
    let digits = match code.get(..2) {
        Some(p) if p.eq_ignore_ascii_case("zl") => &code[2..],
        _ => code,
    };
    let n: u32 = digits.parse().ok().filter(|_| digits.len() <= 4)?;
    let wanted = format!("ZL{n:04}");
    ALL.iter().find(|c| c.code == wanted)
}

/// Where the guide documents `code`.
pub(crate) fn docs_url(code: &str) -> String {
    format!(
        "https://samsamurai301.github.io/zapline/errors.html#{}",
        code.to_ascii_lowercase()
    )
}

/// `message [ZL0010]`, then the fix and where to read more, for panics on
/// invalid configuration (which the developer reads in a terminal).
pub(crate) fn with_help(code: &'static str, subject: Option<&str>, message: &str) -> String {
    let mut out = format!("{message} [{code}]");
    out.push_str(&help(code, subject));
    out
}

/// The indented `fix:` and `see:` lines that follow an error in a terminal.
pub(crate) fn help(code: &str, subject: Option<&str>) -> String {
    match lookup(code) {
        Some(c) => format!(
            "\n  fix: {}\n  see: zapline explain {}",
            c.fix_for(subject),
            c.code
        ),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_ordered_and_complete() {
        let mut last = 0;
        for c in ALL {
            assert_eq!(c.code.len(), 6, "{c:?}");
            assert!(c.code.starts_with("ZL"), "{c:?}");
            let n: u32 = c.code[2..].parse().expect("four digits");
            assert!(n > last, "{} is out of order or repeated", c.code);
            last = n;
            assert!(!c.title.is_empty() && !c.title.ends_with('.'), "{c:?}");
            assert!(c.explanation.len() > 20, "{c:?}");
            assert!(!c.fix.is_empty(), "{c:?}");
            assert!(
                !c.explanation.contains("  "),
                "{}: line continuations left double spaces",
                c.code
            );
        }
    }

    #[test]
    fn lookup_accepts_short_and_lowercase_forms() {
        for q in ["ZL0002", "zl0002", "ZL2", "2", " 0002 "] {
            assert_eq!(lookup(q).map(|c| c.code), Some("ZL0002"), "{q}");
        }
        for q in ["", "ZL", "ZLabc", "ZL00002", "ZL9999", "-1"] {
            assert!(lookup(q).is_none(), "{q}");
        }
    }

    #[test]
    fn help_names_the_fix_and_the_explain_command() {
        let text = with_help("ZL0010", Some("GET /x"), "duplicate route: GET /x");
        assert!(text.starts_with("duplicate route: GET /x [ZL0010]\n  fix: "));
        assert!(text.contains("registrations of GET /x"), "{text}");
        assert!(text.ends_with("see: zapline explain ZL0010"));
        assert_eq!(help("ZL9999", None), "");
    }

    /// Every code the framework emits is in the registry.
    #[test]
    fn every_code_used_in_the_source_is_registered() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        let mut used = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("codes.rs")
                {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let mut rest = text.as_str();
                    while let Some(i) = rest.find("\"ZL") {
                        let code = rest.get(i + 1..i + 7).unwrap_or("");
                        if code[2..].bytes().all(|b| b.is_ascii_digit()) && code.len() == 6 {
                            used.push((code.to_owned(), path.clone()));
                        }
                        rest = &rest[i + 3..];
                    }
                }
            }
        }
        assert!(used.len() > 20, "found only {used:?}");
        for (code, path) in used {
            assert!(
                lookup(&code).is_some(),
                "{code} (used in {}) is not in codes.in",
                path.display()
            );
        }
    }
}
