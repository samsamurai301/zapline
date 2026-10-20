//! Text edits that wire a generated module into `src/routes/mod.rs` and
//! `src/middleware/mod.rs`.
//!
//! The edits are plain string transformations (no parsing of Rust), so they are
//! predictable, idempotent and easy to test: running the same registration twice
//! changes nothing the second time.

use anyhow::{bail, Result};

/// Line that opens the region where mount calls are inserted.
pub const ROUTES_START: &str = "// zapline:routes:start";
/// Line that closes the region where mount calls are inserted.
pub const ROUTES_END: &str = "// zapline:routes:end";

/// Outcome of registering a route module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    /// The updated file contents.
    pub text: String,
    /// A `pub mod` line was added.
    pub declared: bool,
    /// A mount line was added between the markers.
    pub mounted: bool,
}

/// Registers `module` in the text of a `routes/mod.rs`: adds `pub mod <module>;`
/// among the existing module declarations and the statement `mount_stmt` (for
/// example `app = app.plugin(posts::mount);`) just above the end marker.
///
/// Returns `Ok(None)` when the file has no markers, so the caller can print
/// manual instructions instead of guessing. Mismatched markers are an error.
pub fn register_route_module(
    text: &str,
    module: &str,
    mount_stmt: &str,
) -> Result<Option<Registered>> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = lines.iter().position(|l| l.trim() == ROUTES_START);
    let end = lines.iter().position(|l| l.trim() == ROUTES_END);
    let (start, end) = match (start, end) {
        (None, None) => return Ok(None),
        (Some(s), Some(e)) if s < e => (s, e),
        (Some(_), Some(_)) => bail!("`{ROUTES_START}` must come before `{ROUTES_END}`"),
        (Some(_), None) => bail!("found `{ROUTES_START}` but no `{ROUTES_END}`"),
        (None, Some(_)) => bail!("found `{ROUTES_END}` but no `{ROUTES_START}`"),
    };

    let mounted = !lines[start + 1..end].iter().any(|l| l.trim() == mount_stmt);
    if mounted {
        let indent: String = lines[end]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        lines.insert(end, format!("{indent}{mount_stmt}"));
    }
    let declared = declare_module(&mut lines, module);

    Ok(Some(Registered {
        text: join_lines(&lines, text),
        declared,
        mounted,
    }))
}

/// Adds `pub mod <module>;` to the text of a module file (`src/middleware/mod.rs`).
/// Returns the new text and whether anything changed.
pub fn declare_in_module_file(text: &str, module: &str) -> (String, bool) {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let changed = declare_module(&mut lines, module);
    (join_lines(&lines, text), changed)
}

/// Re-joins lines using the line ending style of `original`, with a final newline.
fn join_lines(lines: &[String], original: &str) -> String {
    let eol = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut out = lines.join(eol);
    out.push_str(eol);
    out
}

/// The module name of a top-level `mod x;` declaration line, if it is one.
fn module_name(line: &str) -> Option<&str> {
    let rest = ["pub mod ", "pub(crate) mod ", "mod "]
        .iter()
        .find_map(|p| line.strip_prefix(p))?;
    let name = rest.strip_suffix(';')?.trim();
    (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')).then_some(name)
}

/// Inserts `pub mod <module>;`. Returns false if the module is already declared.
///
/// Placement keeps rustfmt happy: inside the first block of consecutive module
/// declarations, in alphabetical order. Without such a block, the line goes after
/// the `use` statements (or the file's leading `//!` docs).
fn declare_module(lines: &mut Vec<String>, module: &str) -> bool {
    if lines.iter().any(|l| module_name(l) == Some(module)) {
        return false;
    }
    let decl = format!("pub mod {module};");

    if let Some(first) = lines.iter().position(|l| module_name(l).is_some()) {
        let mut at = first;
        while at < lines.len() {
            match module_name(&lines[at]) {
                Some(name) if name < module => at += 1,
                _ => break,
            }
        }
        lines.insert(at, decl);
        return true;
    }

    // No declarations yet: after the last top-level `use`, else after leading `//!` docs.
    let mut insert_at = 0;
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        if line.starts_with("use ") || line.starts_with("pub use ") {
            let mut j = i;
            while j < lines.len() && !lines[j].trim_end().ends_with(';') {
                j += 1;
            }
            insert_at = (j + 1).min(lines.len());
            i = j + 1;
        } else {
            if insert_at == 0 && line.starts_with("//!") {
                // Remember the end of the leading doc block as a fallback.
                let mut k = i;
                while k < lines.len() && lines[k].starts_with("//!") {
                    k += 1;
                }
                insert_at = k;
                i = k;
                continue;
            }
            i += 1;
        }
    }

    let block = if insert_at == 0 {
        vec![decl, String::new()]
    } else {
        let mut v = vec![String::new(), decl];
        if lines.get(insert_at).is_some_and(|l| !l.trim().is_empty()) {
            v.push(String::new());
        }
        v
    };
    for (offset, line) in block.into_iter().enumerate() {
        lines.insert(insert_at + offset, line);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const API_MOD: &str = "\
//! HTTP routes.

use zapline::prelude::*;

pub mod health;
pub mod users;

/// Registers every route module on the app.
pub fn install(mut app: App) -> App {
    // zapline:routes:start
    app = app.plugin(health::mount);
    app = app.plugin(users::mount);
    // zapline:routes:end
    app
}
";

    fn register(text: &str, module: &str) -> Registered {
        register_route_module(text, module, &format!("app = app.plugin({module}::mount);"))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn inserts_module_alphabetically_and_mount_before_end_marker() {
        let r = register(API_MOD, "posts");
        assert!(r.declared && r.mounted);
        assert_eq!(
            r.text,
            "\
//! HTTP routes.

use zapline::prelude::*;

pub mod health;
pub mod posts;
pub mod users;

/// Registers every route module on the app.
pub fn install(mut app: App) -> App {
    // zapline:routes:start
    app = app.plugin(health::mount);
    app = app.plugin(users::mount);
    app = app.plugin(posts::mount);
    // zapline:routes:end
    app
}
"
        );
    }

    #[test]
    fn module_goes_last_when_alphabetically_last_and_first_when_first() {
        let r = register(API_MOD, "zebras");
        assert!(r.text.contains("pub mod users;\npub mod zebras;\n"));
        let r = register(API_MOD, "accounts");
        assert!(r.text.contains("pub mod accounts;\npub mod health;\n"));
    }

    #[test]
    fn registration_is_idempotent() {
        let once = register(API_MOD, "posts");
        let twice = register(&once.text, "posts");
        assert_eq!(twice.text, once.text);
        assert!(!twice.declared && !twice.mounted);
        // Already-present modules are not duplicated either.
        let again = register(API_MOD, "users");
        assert_eq!(again.text, API_MOD);
    }

    #[test]
    fn missing_markers_mean_manual_registration() {
        let text = "pub mod users;\n\npub fn install() {}\n";
        let r = register_route_module(text, "posts", "app = app.plugin(posts::mount);");
        assert_eq!(r.unwrap(), None);
    }

    #[test]
    fn malformed_markers_are_errors() {
        let only_start = "fn f() {\n    // zapline:routes:start\n}\n";
        assert!(register_route_module(only_start, "a", "x;").is_err());
        let only_end = "fn f() {\n    // zapline:routes:end\n}\n";
        assert!(register_route_module(only_end, "a", "x").is_err());
        let reversed = "// zapline:routes:end\n// zapline:routes:start\n";
        assert!(register_route_module(reversed, "a", "x").is_err());
    }

    #[test]
    fn works_on_an_empty_marker_region_without_modules() {
        let fresh = "\
//! Routes.

use zapline::prelude::*;

/// Registers every route module on the app.
pub fn install(mut app: App) -> App {
    // zapline:routes:start
    // zapline:routes:end
    app
}
";
        let r = register(fresh, "posts");
        assert_eq!(
            r.text,
            "\
//! Routes.

use zapline::prelude::*;

pub mod posts;

/// Registers every route module on the app.
pub fn install(mut app: App) -> App {
    // zapline:routes:start
    app = app.plugin(posts::mount);
    // zapline:routes:end
    app
}
"
        );
    }

    #[test]
    fn handles_files_without_use_statements() {
        let text = "//! Docs.\n\npub fn install() {\n    // zapline:routes:start\n    // zapline:routes:end\n}\n";
        let r = register(text, "posts");
        assert!(
            r.text
                .starts_with("//! Docs.\n\npub mod posts;\n\npub fn install()"),
            "{}",
            r.text
        );

        let bare =
            "pub fn install() {\n    // zapline:routes:start\n    // zapline:routes:end\n}\n";
        let r = register(bare, "posts");
        assert!(
            r.text.starts_with("pub mod posts;\n\npub fn install()"),
            "{}",
            r.text
        );
    }

    #[test]
    fn handles_multiline_use_statements() {
        let text = "use zapline::{\n    prelude::*,\n    App,\n};\n\npub fn install() {\n    // zapline:routes:start\n    // zapline:routes:end\n}\n";
        let r = register(text, "posts");
        assert!(
            r.text.contains("};\n\npub mod posts;\n\npub fn install()"),
            "{}",
            r.text
        );
    }

    #[test]
    fn preserves_crlf_line_endings() {
        let crlf = API_MOD.replace('\n', "\r\n");
        let r = register(&crlf, "posts");
        assert!(r.text.contains("pub mod posts;\r\n"));
        assert!(!r.text.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn mount_line_copies_the_end_markers_indentation() {
        let text = "fn f() {\n\t// zapline:routes:start\n\t// zapline:routes:end\n}\n";
        let r = register(text, "a");
        assert!(
            r.text
                .contains("\n\tapp = app.plugin(a::mount);\n\t// zapline:routes:end"),
            "{}",
            r.text
        );
    }

    #[test]
    fn declare_in_plain_module_files() {
        let (text, changed) = declare_in_module_file("//! Middleware.\n", "timing");
        assert!(changed);
        assert_eq!(text, "//! Middleware.\n\npub mod timing;\n");
    }

    #[test]
    fn declare_keeps_sorted_order_in_module_files() {
        let (text, _) = declare_in_module_file("pub mod b;\npub mod d;\n", "c");
        assert_eq!(text, "pub mod b;\npub mod c;\npub mod d;\n");
        let (same, changed) = declare_in_module_file(&text, "c");
        assert!(!changed);
        assert_eq!(same, text);
    }

    #[test]
    fn module_name_parsing() {
        assert_eq!(module_name("pub mod users;"), Some("users"));
        assert_eq!(module_name("mod users;"), Some("users"));
        assert_eq!(module_name("pub(crate) mod users;"), Some("users"));
        assert_eq!(module_name("pub mod users {"), None);
        assert_eq!(module_name("    pub mod users;"), None);
        assert_eq!(module_name("// pub mod users;"), None);
        assert_eq!(module_name("pub mod ;"), None);
    }
}
