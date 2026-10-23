//! `zapline client`: typed clients generated from the app's OpenAPI
//! document, and keeping them current from `zapline dev`.
//!
//! The generators live in `zapline-codegen` (shared with the framework's
//! `OpenApi::client_js()` route). A `.ts` target gets the TypeScript client
//! (an interface per schema, `createClient()` with one typed method per
//! operation, `ApiError` and a typed `ValidationError` for 422s); a `.js` or
//! `.mjs` target gets the same client as an ES module with JSDoc types, for
//! pages without a build step.
//!
//! Where clients go, first match wins:
//!
//! 1. `-o FILE` / `zapline dev --client FILE`;
//! 2. a `[client]` table in `zapline.toml`, or `[package.metadata.zapline.client]`
//!    in `Cargo.toml`: `ts = "web/src/api.ts"`, `js = "static/api.js"`,
//!    `enabled = false` to turn the default off;
//! 3. `frontend/src/api.ts` when the project has a `frontend/` directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use zapline_codegen::{Changes, Generated};

use crate::project::Project;
use crate::{openapi, ui};

/// Where the TypeScript client goes when the project has a `frontend/`.
pub const DEFAULT_FRONTEND_CLIENT: &str = "frontend/src/api.ts";

/// The language a client file is written in, from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// `.ts`, `.mts`, `.cts` (and anything unknown).
    TypeScript,
    /// `.js`, `.mjs`: an ES module with JSDoc types.
    JavaScript,
}

impl Lang {
    /// The language of `path`.
    pub fn of(path: &Path) -> Lang {
        match path.extension().and_then(|e| e.to_str()) {
            Some("js" | "mjs") => Lang::JavaScript,
            _ => Lang::TypeScript,
        }
    }
}

/// Generates the client for `path` from a spec.
pub fn generate_for(spec: &Value, path: &Path) -> Result<Generated> {
    let generated = match Lang::of(path) {
        Lang::TypeScript => zapline_codegen::typescript(spec),
        Lang::JavaScript => zapline_codegen::javascript(spec),
    };
    Ok(generated?)
}

/// Client files from the project's settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Configured {
    /// Absolute paths.
    pub paths: Vec<PathBuf>,
    /// Whether they were configured rather than found (`frontend/`).
    pub explicit: bool,
}

/// The client files configured for `project`, without a command-line
/// override. Empty when nothing is configured and there is no `frontend/`
/// directory.
pub fn configured(project: &Project) -> Result<Configured> {
    let root = &project.root;
    let table = config_table(root)?;
    let mut out = Vec::new();
    if let Some(t) = &table {
        if t.get("enabled").and_then(toml::Value::as_bool) == Some(false) {
            return Ok(Configured::default());
        }
        for key in ["ts", "js"] {
            match t.get(key) {
                None => {}
                Some(toml::Value::String(p)) => {
                    out.push(openapi::resolve_output(root, Path::new(p)))
                }
                Some(_) => bail!("`client.{key}` must be a path (a string)"),
            }
        }
        if let Some(unknown) = t
            .keys()
            .find(|k| !["ts", "js", "enabled"].contains(&k.as_str()))
        {
            bail!("unknown setting `client.{unknown}` (expected `ts`, `js` or `enabled`)");
        }
    }
    if !out.is_empty() {
        return Ok(Configured {
            paths: out,
            explicit: true,
        });
    }
    if root.join("frontend").is_dir() {
        out.push(root.join(DEFAULT_FRONTEND_CLIENT));
    }
    Ok(Configured {
        paths: out,
        explicit: false,
    })
}

/// The `[client]` table of `zapline.toml`, or else
/// `[package.metadata.zapline.client]` of `Cargo.toml`.
fn config_table(root: &Path) -> Result<Option<toml::Table>> {
    let zapline_toml = root.join("zapline.toml");
    if zapline_toml.is_file() {
        let text = std::fs::read_to_string(&zapline_toml)
            .with_context(|| format!("cannot read {}", zapline_toml.display()))?;
        let doc: toml::Table = toml::from_str(&text)
            .with_context(|| format!("cannot parse {}", zapline_toml.display()))?;
        if let Some(client) = doc.get("client") {
            return match client {
                toml::Value::Table(t) => Ok(Some(t.clone())),
                _ => bail!("`client` in zapline.toml must be a table"),
            };
        }
    }
    let manifest = root.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return Ok(None);
    };
    let doc: toml::Table = toml::from_str(&text).context("cannot parse Cargo.toml")?;
    let client = doc
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("zapline"))
        .and_then(|z| z.get("client"));
    match client {
        None => Ok(None),
        Some(toml::Value::Table(t)) => Ok(Some(t.clone())),
        Some(_) => bail!("`package.metadata.zapline.client` in Cargo.toml must be a table"),
    }
}

/// What writing one client did.
#[derive(Debug)]
pub struct Written {
    /// The file.
    pub path: PathBuf,
    /// The client.
    pub generated: Generated,
    /// Whether the file changed on disk.
    pub changed: bool,
}

/// Writes the client for each of `targets` from the spec at `spec_path`.
/// Files are only rewritten when they change, so watchers (vite,
/// `tsc --watch`) stay quiet.
fn write_all(spec_path: &Path, targets: &[PathBuf]) -> Result<Vec<Written>> {
    let text = std::fs::read_to_string(spec_path)
        .with_context(|| format!("cannot read {}", spec_path.display()))?;
    let spec: Value = serde_json::from_str(&text).context("the OpenAPI document is not JSON")?;
    let mut out = Vec::new();
    for target in targets {
        let generated = generate_for(&spec, target)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let changed =
            std::fs::read_to_string(target).ok().as_deref() != Some(generated.source.as_str());
        if changed {
            std::fs::write(target, &generated.source)
                .with_context(|| format!("cannot write {}", target.display()))?;
        }
        out.push(Written {
            path: target.clone(),
            generated,
            changed,
        });
    }
    Ok(out)
}

fn spec_path(project: &Project) -> PathBuf {
    project
        .root
        .join("target")
        .join("zapline-client-openapi.json")
}

fn shown<'p>(project: &Project, path: &'p Path) -> std::path::Display<'p> {
    path.strip_prefix(&project.root).unwrap_or(path).display()
}

/// `zapline client [-o FILE]`: export the spec and write the client(s).
pub fn run(project: &Project, bin: Option<&str>, output: Option<&Path>) -> Result<()> {
    let targets = match output {
        Some(o) => {
            let cwd = std::env::current_dir().context("cannot determine the current directory")?;
            vec![openapi::resolve_output(&cwd, o)]
        }
        None => match configured(project)?.paths {
            t if t.is_empty() => vec![project.root.join("client.ts")],
            t => t,
        },
    };
    let spec_path = spec_path(project);
    openapi::export(project, bin, &spec_path)?;
    for w in write_all(&spec_path, &targets)? {
        let g = &w.generated;
        ui::status(
            if w.changed { "Wrote" } else { "Unchanged" },
            format!(
                "{} ({} operations{})",
                shown(project, &w.path),
                g.operations,
                if g.skipped.is_empty() {
                    String::new()
                } else {
                    format!(", {} HTML routes skipped", g.skipped.len())
                }
            ),
        );
    }
    Ok(())
}

/// The clients `zapline dev` keeps current, and what they looked like after
/// the previous build (to report what changed).
#[derive(Debug, Default)]
pub struct Live {
    targets: Vec<PathBuf>,
    previous: BTreeMap<PathBuf, Generated>,
}

impl Live {
    /// Keeps `targets` up to date.
    pub fn new(targets: Vec<PathBuf>) -> Live {
        Live {
            targets,
            previous: BTreeMap::new(),
        }
    }

    /// Whether there is anything to keep up to date.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Regenerates every client from a freshly built executable (faster
    /// than `cargo run`) and reports each file that changed, with the
    /// methods that were added, removed or changed since the last build.
    pub fn refresh(
        &mut self,
        project: &Project,
        exe: &Path,
    ) -> Result<Vec<(PathBuf, Option<Changes>)>> {
        let spec_path = spec_path(project);
        let _ = std::fs::remove_file(&spec_path);
        let mut cmd = std::process::Command::new(exe);
        cmd.current_dir(&project.root)
            .env(openapi::ENV_EXPORT_OPENAPI, &spec_path);
        let captured = crate::build::run_captured(cmd, std::time::Duration::from_secs(30))?;
        if captured.status.code() == Some(openapi::EXIT_NO_SPEC) {
            bail!(
                "the app has no OpenAPI document\n\
                 hint: add `.openapi(OpenApi::new(\"My API\", \"1.0.0\"))` to the App"
            );
        }
        if !captured.status.success() {
            return Err(captured.failure("before exporting the spec"));
        }
        let mut report = Vec::new();
        for w in write_all(&spec_path, &self.targets)? {
            let changes = self
                .previous
                .get(&w.path)
                .map(|old| w.generated.changes_since(old));
            if w.changed {
                report.push((w.path.clone(), changes));
            }
            self.previous.insert(w.path, w.generated);
        }
        Ok(report)
    }

    /// Runs [`refresh`](Self::refresh) and prints the outcome. A failure
    /// turns live clients off when the app has no OpenAPI document and they
    /// were not asked for explicitly.
    pub fn refresh_and_report(&mut self, project: &Project, exe: &Path, explicit: bool) {
        match self.refresh(project, exe) {
            Ok(report) => {
                for (path, changes) in report {
                    let what = match changes {
                        Some(c) if !c.is_empty() => format!(" ({c})"),
                        _ => String::new(),
                    };
                    ui::status("Client", format!("updated {}{what}", shown(project, &path)));
                }
            }
            Err(e) => {
                let text = format!("{e:#}");
                if !explicit && text.contains("no OpenAPI document") {
                    ui::status_warn(
                        "Client",
                        "frontend/ found but the app has no OpenAPI document; add \
                         `.openapi(OpenApi::new(..))` for a typed client (off for this session)",
                    );
                    self.targets.clear();
                } else {
                    ui::status_warn("Client", format!("not updated: {text}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(dir: &Path, cargo_extra: &str) -> Project {
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"demo\"\nversion = \"0.1.0\"\n{cargo_extra}"),
        )
        .unwrap();
        Project::from_manifest(&dir.join("Cargo.toml"))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn language_from_extension() {
        assert_eq!(Lang::of(Path::new("a/api.ts")), Lang::TypeScript);
        assert_eq!(Lang::of(Path::new("a/api.mjs")), Lang::JavaScript);
        assert_eq!(Lang::of(Path::new("static/api.js")), Lang::JavaScript);
        assert_eq!(Lang::of(Path::new("client")), Lang::TypeScript);
    }

    #[test]
    fn nothing_configured_and_no_frontend() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path(), "");
        assert!(configured(&p).unwrap().paths.is_empty());
    }

    #[test]
    fn frontend_dir_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("frontend")).unwrap();
        let p = project(dir.path(), "");
        assert_eq!(
            configured(&p).unwrap(),
            Configured {
                paths: vec![p.root.join("frontend/src/api.ts")],
                explicit: false
            }
        );
    }

    #[test]
    fn zapline_toml_wins_over_cargo_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("frontend")).unwrap();
        let p = project(
            dir.path(),
            "[package.metadata.zapline.client]\nts = \"meta/api.ts\"\n",
        );
        assert_eq!(
            configured(&p).unwrap().paths,
            vec![p.root.join("meta/api.ts")]
        );
        assert!(configured(&p).unwrap().explicit);
        std::fs::write(
            dir.path().join("zapline.toml"),
            "[client]\nts = \"web/src/api.ts\"\njs = \"static/api.js\"\n",
        )
        .unwrap();
        assert_eq!(
            configured(&p).unwrap().paths,
            vec![p.root.join("web/src/api.ts"), p.root.join("static/api.js")]
        );
    }

    #[test]
    fn disabled_and_invalid_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("frontend")).unwrap();
        let p = project(dir.path(), "");
        std::fs::write(
            dir.path().join("zapline.toml"),
            "[client]\nenabled = false\n",
        )
        .unwrap();
        assert!(configured(&p).unwrap().paths.is_empty());
        std::fs::write(
            dir.path().join("zapline.toml"),
            "[client]\noutput = \"x.ts\"\n",
        )
        .unwrap();
        let err = configured(&p).unwrap_err().to_string();
        assert!(err.contains("unknown setting `client.output`"), "{err}");
        std::fs::write(dir.path().join("zapline.toml"), "[client]\nts = 1\n").unwrap();
        assert!(configured(&p).is_err());
    }

    #[test]
    fn writes_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("spec.json");
        std::fs::write(
            &spec,
            r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{"/a":{"get":{"operationId":"a","responses":{"204":{"description":""}}}}}}"#,
        )
        .unwrap();
        let targets = vec![
            dir.path().join("web/api.ts"),
            dir.path().join("static/api.js"),
        ];
        let first = write_all(&spec, &targets).unwrap();
        assert!(first.iter().all(|w| w.changed));
        let ts = std::fs::read_to_string(&targets[0]).unwrap();
        assert!(ts.contains("a: (options?: CallOptions) =>"), "{ts}");
        let js = std::fs::read_to_string(&targets[1]).unwrap();
        assert!(js.contains("// @ts-check"), "{js}");
        let second = write_all(&spec, &targets).unwrap();
        assert!(second.iter().all(|w| !w.changed));
    }
}
