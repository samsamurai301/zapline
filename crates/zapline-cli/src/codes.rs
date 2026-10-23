//! `zapline explain`: zapline's diagnostic codes (`ZL0002`), with what
//! causes each one and the code that fixes it.
//!
//! The registry is the framework's `crates/zapline/src/codes.in`, embedded
//! here from `assets/codes.in` (copied by `scripts/gen-llms.sh`, so the
//! published crate has it too). The index in `docs/src/errors.md` is
//! generated from it (between `<!--codes:start-->` markers, without the
//! space the guide splitter looks for): `ZAPLINE_BLESS=1 cargo test -p
//! zapline-cli codes::`.

/// One diagnostic code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code {
    /// `ZL` and four digits.
    pub code: &'static str,
    /// A few words.
    pub title: &'static str,
    /// What causes it.
    pub explanation: &'static str,
    /// The code to add; `{T}` stands for what the error names.
    pub fix: &'static str,
}

/// Every code, in order.
pub const ALL: &[Code] = include!("../assets/codes.in");

/// Where the guide documents a code.
pub const DOCS: &str = "https://samsamurai301.github.io/zapline/errors.html";

/// The entry for `code`: `ZL0002`, `zl0002`, `ZL2` or `2`.
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

/// The fix as written in the registry, with `{T}` shown as `T`.
fn fix(c: &Code) -> String {
    c.fix.replace("{T}", "T")
}

/// What `zapline explain ZL0002` prints.
pub fn explain(code: &str) -> anyhow::Result<String> {
    let Some(c) = lookup(code) else {
        anyhow::bail!(
            "unknown code `{code}`: run `zapline explain` for the list ({} codes, ZL0001 to {})",
            ALL.len(),
            ALL.last().map_or("", |c| c.code)
        );
    };
    Ok(format!(
        "{}: {}\n\n{}\n\nFix:\n    {}\n\nDocs: {DOCS}#{}\n",
        c.code,
        c.title,
        wrap(c.explanation, 80),
        fix(c),
        c.code.to_ascii_lowercase()
    ))
}

/// What `zapline explain` without a code prints: one line per code.
pub fn list() -> String {
    let mut out = String::new();
    for c in ALL {
        out.push_str(&format!("{}  {}\n", c.code, c.title));
    }
    out.push_str("\nzapline explain <CODE> for the cause and the fix.\n");
    out
}

/// Every code in the text, in order of appearance, without repeats.
pub fn find_in(text: &str) -> Vec<&'static Code> {
    let mut found: Vec<&'static Code> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(at) = text[i..].find("ZL") {
        let start = i + at;
        let digits = &bytes[start + 2..(start + 6).min(bytes.len())];
        let boundary = bytes.get(start + 6).is_none_or(|b| !b.is_ascii_digit());
        if digits.len() == 4 && digits.iter().all(u8::is_ascii_digit) && boundary {
            if let Some(c) = lookup(&text[start..start + 6]) {
                if !found.iter().any(|f| f.code == c.code) {
                    found.push(c);
                }
            }
        }
        i = start + 2;
    }
    found
}

/// The generated part of `docs/src/errors.md`.
pub fn markdown_index() -> String {
    let mut out = String::from(
        "<!--codes:start (generated from crates/zapline/src/codes.in; do not edit by hand)-->\n",
    );
    out.push_str("\n| Code | Title |\n|---|---|\n");
    for c in ALL {
        out.push_str(&format!(
            "| [{}](#{}) | {} |\n",
            c.code,
            c.code.to_ascii_lowercase(),
            c.title
        ));
    }
    for c in ALL {
        out.push_str(&format!(
            "\n### {}\n\n**{}.** {}\n\n```text\n{}\n```\n",
            c.code,
            c.title,
            c.explanation,
            fix(c)
        ));
    }
    out.push_str("\n<!--codes:end-->");
    out
}

/// Word-wraps `text` at `width` columns.
fn wrap(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut line = 0;
    for word in text.split_whitespace() {
        if line > 0 && line + 1 + word.len() > width {
            out.push('\n');
            line = 0;
        } else if line > 0 {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += word.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn explain_prints_title_cause_fix_and_link() {
        let text = explain("ZL0003").unwrap();
        assert!(
            text.starts_with("ZL0003: A dependency failed to initialize\n\n"),
            "{text}"
        );
        assert!(text.contains("\nFix:\n    Fix the cause named"), "{text}");
        assert!(text.ends_with("errors.html#zl0003\n"), "{text}");
        assert!(text.lines().all(|l| l.len() <= 80 || l.starts_with("    ")));
        // `{T}` reads as a type parameter.
        assert!(explain("zl1").unwrap().contains("// value: T;"));
    }

    #[test]
    fn explain_rejects_unknown_codes_helpfully() {
        let err = explain("ZL9999").unwrap_err().to_string();
        assert!(err.contains("run `zapline explain` for the list"), "{err}");
        assert!(explain("nonsense").is_err());
    }

    #[test]
    fn list_has_one_line_per_code() {
        let text = list();
        assert_eq!(
            text.lines().filter(|l| l.starts_with("ZL")).count(),
            ALL.len()
        );
        assert!(text.contains("ZL0010  Duplicate route\n"));
    }

    #[test]
    fn find_in_spots_codes_in_text() {
        let found = find_in("startup failed: ... [ZL0004]\n then ZL0002, ZL0004 again, ZL00021 no");
        let codes: Vec<&str> = found.iter().map(|c| c.code).collect();
        assert_eq!(codes, ["ZL0004", "ZL0002"]);
        assert!(find_in("ZL9999 ZL").is_empty());
    }

    /// The embedded copy is the framework's registry.
    #[test]
    fn the_embedded_registry_matches_the_framework() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../zapline/src/codes.in");
        // Absent when built from the published crate.
        if let Ok(source) = std::fs::read_to_string(source) {
            assert!(
                source == include_str!("../assets/codes.in"),
                "crates/zapline-cli/assets/codes.in is out of date: run ./scripts/gen-llms.sh"
            );
        }
    }

    /// docs/src/errors.md lists every code. `ZAPLINE_BLESS=1` rewrites it.
    #[test]
    fn the_docs_index_is_up_to_date() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/src/errors.md");
        let Ok(doc) = std::fs::read_to_string(&path) else {
            return;
        };
        let (Some(start), Some(end)) = (doc.find("<!--codes:start"), doc.find("<!--codes:end-->"))
        else {
            panic!("docs/src/errors.md has no <!--codes:start--> ... <!--codes:end--> block");
        };
        let end = end + "<!--codes:end-->".len();
        let want = markdown_index();
        if doc[start..end] == want {
            return;
        }
        if std::env::var_os("ZAPLINE_BLESS").is_some() {
            let updated = format!("{}{want}{}", &doc[..start], &doc[end..]);
            std::fs::write(&path, updated).unwrap();
            return;
        }
        panic!(
            "docs/src/errors.md is out of date: run `ZAPLINE_BLESS=1 cargo test -p zapline-cli codes::` then ./scripts/gen-llms.sh"
        );
    }
}
