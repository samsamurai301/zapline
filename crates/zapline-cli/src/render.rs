//! A tiny placeholder renderer for the embedded templates, plus safe file output.
//!
//! Placeholders look like `@@name@@`. That delimiter does not occur in Rust,
//! TOML, YAML (`${{ ... }}` is fine) or Dockerfiles, so templates can contain
//! real code without escaping. Unknown placeholders are an error, so typos in
//! templates are caught by the unit tests instead of shipping to users.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

/// Substitutes every `@@key@@` in `template` using `vars`.
pub fn render(template: &str, vars: &[(&str, &str)]) -> Result<String> {
    let mut out = String::with_capacity(template.len() + 128);
    let mut rest = template;
    while let Some(start) = rest.find("@@") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let key_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let key = &after[..key_len];
        if key_len > 0 && after[key_len..].starts_with("@@") {
            let value = vars
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| *v)
                .ok_or_else(|| anyhow!("unknown template placeholder `@@{key}@@`"))?;
            out.push_str(value);
            rest = &after[key_len + 2..];
        } else {
            // A lone `@@` that is not a placeholder: keep it verbatim.
            out.push_str("@@");
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// A file to be written, relative to the output root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    /// Destination relative to the output root.
    pub path: PathBuf,
    /// File contents.
    pub contents: String,
}

/// Rejects absolute paths and `..` components so templates can never write
/// outside the target directory.
pub fn ensure_relative(path: &Path) -> Result<()> {
    if path.is_absolute() {
        bail!("refusing to write absolute path {}", path.display());
    }
    for c in path.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("refusing to write outside the target: {}", path.display()),
        }
    }
    Ok(())
}

/// Writes `files` below `root`, creating parent directories. With
/// `overwrite == false`, an existing file is an error (checked for all files
/// before the first byte is written).
pub fn write_files(root: &Path, files: &[OutputFile], overwrite: bool) -> Result<()> {
    for f in files {
        ensure_relative(&f.path)?;
        let target = root.join(&f.path);
        if !overwrite && target.exists() {
            bail!(
                "{} already exists (use --force to overwrite)",
                f.path.display()
            );
        }
    }
    for f in files {
        let target = root.join(&f.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create directory {}", parent.display()))?;
        }
        fs::write(&target, &f.contents)
            .with_context(|| format!("cannot write {}", target.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_placeholders() {
        let out = render(
            "name=@@name@@ lib=@@lib_name@@!",
            &[("name", "a-b"), ("lib_name", "a_b")],
        )
        .unwrap();
        assert_eq!(out, "name=a-b lib=a_b!");
    }

    #[test]
    fn repeated_and_adjacent_placeholders() {
        let out = render("@@a@@@@b@@@@a@@", &[("a", "1"), ("b", "2")]).unwrap();
        assert_eq!(out, "121");
    }

    #[test]
    fn unknown_placeholder_is_an_error() {
        let err = render("x @@nope@@ y", &[("name", "a")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("@@nope@@"), "{err}");
    }

    #[test]
    fn stray_delimiters_are_kept() {
        assert_eq!(render("a @@ b", &[]).unwrap(), "a @@ b");
        assert_eq!(render("a @@ b @@ c", &[]).unwrap(), "a @@ b @@ c");
        assert_eq!(render("trailing @@", &[]).unwrap(), "trailing @@");
        assert_eq!(render("@@@@", &[]).unwrap(), "@@@@");
    }

    #[test]
    fn github_actions_expressions_pass_through() {
        let s = "run: echo ${{ matrix.os }} @@name@@";
        assert_eq!(
            render(s, &[("name", "x")]).unwrap(),
            "run: echo ${{ matrix.os }} x"
        );
    }

    #[test]
    fn values_are_not_rescanned() {
        assert_eq!(render("@@a@@", &[("a", "@@b@@")]).unwrap(), "@@b@@");
    }

    #[test]
    fn rejects_escaping_paths() {
        assert!(ensure_relative(Path::new("src/main.rs")).is_ok());
        assert!(ensure_relative(Path::new("./src/main.rs")).is_ok());
        assert!(ensure_relative(Path::new("../evil")).is_err());
        assert!(ensure_relative(Path::new("src/../../evil")).is_err());
        assert!(ensure_relative(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn write_files_creates_parents_and_respects_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![OutputFile {
            path: PathBuf::from("a/b/c.txt"),
            contents: "one".into(),
        }];
        write_files(dir.path(), &files, false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "one"
        );

        let again = vec![OutputFile {
            path: PathBuf::from("a/b/c.txt"),
            contents: "two".into(),
        }];
        let err = write_files(dir.path(), &again, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "one"
        );

        write_files(dir.path(), &again, true).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "two"
        );
    }

    #[test]
    fn conflicts_are_detected_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.txt"), "keep").unwrap();
        let files = vec![
            OutputFile {
                path: "a.txt".into(),
                contents: "a".into(),
            },
            OutputFile {
                path: "b.txt".into(),
                contents: "b".into(),
            },
        ];
        assert!(write_files(dir.path(), &files, false).is_err());
        assert!(!dir.path().join("a.txt").exists());
    }
}
