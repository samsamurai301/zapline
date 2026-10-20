//! `zapline openapi`: export the app's OpenAPI document to a file.
//!
//! The app is run with `ZAPLINE_EXPORT_OPENAPI=<path>`; the OpenAPI plugin then
//! writes the document and `App::run` exits. The CLI has the app write to a
//! temporary file next to the target and renames it into place only after
//! checking that it is valid JSON, so a failed export never clobbers a good file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::build;
use crate::project::Project;
use crate::ui;

/// Environment variable that makes the OpenAPI plugin write the spec and exit.
pub const ENV_EXPORT_OPENAPI: &str = "ZAPLINE_EXPORT_OPENAPI";

/// How long the app may take to export once built.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// A short description of an OpenAPI document for the success message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecSummary {
    /// `info.title`, if present.
    pub title: Option<String>,
    /// `info.version`, if present.
    pub version: Option<String>,
    /// Number of path items.
    pub paths: usize,
    /// Number of operations (method + path pairs).
    pub operations: usize,
}

const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Validates `text` as an OpenAPI document and summarises it.
pub fn summarize(text: &str) -> Result<SpecSummary> {
    let doc: serde_json::Value =
        serde_json::from_str(text).context("the exported document is not valid JSON")?;
    if !doc.is_object() {
        bail!("the exported document is not a JSON object");
    }
    if doc.get("openapi").is_none() && doc.get("swagger").is_none() {
        bail!("the exported document has no `openapi` version field");
    }
    let info = doc.get("info");
    let text_field = |key: &str| {
        info.and_then(|i| i.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let paths = doc.get("paths").and_then(|p| p.as_object());
    let operations = paths
        .map(|p| {
            p.values()
                .filter_map(|item| item.as_object())
                .map(|item| {
                    item.keys()
                        .filter(|k| HTTP_METHODS.contains(&k.as_str()))
                        .count()
                })
                .sum()
        })
        .unwrap_or(0);
    Ok(SpecSummary {
        title: text_field("title"),
        version: text_field("version"),
        paths: paths.map_or(0, |p| p.len()),
        operations,
    })
}

/// The temporary file the app writes to before it is renamed to `target`.
pub fn temp_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "openapi.json".into());
    target.with_file_name(format!(".{name}.zapline-tmp"))
}

/// Resolves the output path against `base` and makes it absolute.
pub fn resolve_output(base: &Path, output: &Path) -> PathBuf {
    if output.is_absolute() {
        output.to_path_buf()
    } else {
        base.join(output)
    }
}

/// Runs the app and writes the OpenAPI document to `output`.
pub fn run(project: &Project, bin: Option<&str>, output: &Path) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot determine the current directory")?;
    let target = resolve_output(&cwd, output);
    export(project, bin, &target)
}

/// Like [`run`] with an already-resolved absolute target path.
pub fn export(project: &Project, bin: Option<&str>, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let tmp = temp_path_for(target);
    let _ = std::fs::remove_file(&tmp);

    let result = (|| -> Result<SpecSummary> {
        let captured = build::run_app(
            project,
            bin,
            &[(ENV_EXPORT_OPENAPI, tmp.as_os_str())],
            RUN_TIMEOUT,
        )?;
        if !captured.status.success() {
            bail!(
                "the app exited with {} before exporting the spec",
                captured.status
            );
        }
        let text = std::fs::read_to_string(&tmp).context(
            "the app exited without writing the OpenAPI document\n\
             hint: enable it with `App::openapi(OpenApi::new(...))` and make sure `main` calls `App::run`",
        )?;
        summarize(&text)
    })();

    let summary = match result {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, target)
        .with_context(|| format!("cannot move the spec to {}", target.display()))?;

    let size = std::fs::metadata(target).map(|m| m.len()).unwrap_or(0);
    ui::status(
        "Wrote",
        format!(
            "{} ({} bytes, {} paths, {} operations)",
            target.display(),
            size,
            summary.paths,
            summary.operations
        ),
    );
    if let (Some(title), Some(version)) = (&summary.title, &summary.version) {
        ui::status("API", format!("{title} v{version}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_a_document() {
        let doc = r#"{
            "openapi": "3.1.0",
            "info": {"title": "demo", "version": "0.2.0"},
            "paths": {
                "/users": {"get": {}, "post": {}, "parameters": []},
                "/users/{id}": {"get": {}, "delete": {}}
            }
        }"#;
        assert_eq!(
            summarize(doc).unwrap(),
            SpecSummary {
                title: Some("demo".into()),
                version: Some("0.2.0".into()),
                paths: 2,
                operations: 4,
            }
        );
    }

    #[test]
    fn documents_without_paths_are_fine() {
        let s = summarize(r#"{"openapi":"3.1.0"}"#).unwrap();
        assert_eq!((s.paths, s.operations), (0, 0));
        assert_eq!(s.title, None);
    }

    #[test]
    fn rejects_things_that_are_not_openapi() {
        assert!(summarize("not json").is_err());
        assert!(summarize("[1]").is_err());
        assert!(summarize(r#"{"info":{}}"#).is_err());
        assert!(summarize("").is_err());
    }

    #[test]
    fn temp_file_sits_next_to_the_target() {
        assert_eq!(
            temp_path_for(Path::new("/a/b/openapi.json")),
            PathBuf::from("/a/b/.openapi.json.zapline-tmp")
        );
        assert_eq!(
            temp_path_for(Path::new("spec.json")),
            PathBuf::from(".spec.json.zapline-tmp")
        );
    }

    #[test]
    fn output_paths_resolve_against_the_base() {
        assert_eq!(
            resolve_output(Path::new("/work"), Path::new("openapi.json")),
            PathBuf::from("/work/openapi.json")
        );
        assert_eq!(
            resolve_output(Path::new("/work"), Path::new("/abs/spec.json")),
            PathBuf::from("/abs/spec.json")
        );
    }
}
