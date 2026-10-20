//! Locating and describing the cargo project the CLI operates on.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::naming::crate_ident;

/// What the CLI needs to know about a cargo package.
#[derive(Debug, Clone)]
pub struct Project {
    /// Directory holding the package's `Cargo.toml`.
    pub root: PathBuf,
    /// `[package] name`.
    pub package_name: String,
    /// The library crate name as used in `use my_app::...`, if there is a lib.
    pub lib_name: Option<String>,
    /// Names of the binary targets.
    pub bins: Vec<String>,
    /// `[package] default-run`, if set.
    pub default_run: Option<String>,
    /// `[package] edition` (2021 when unspecified).
    pub edition: String,
    /// Names of crates listed in `[dependencies]` (package names for renamed ones too).
    pub dependencies: BTreeSet<String>,
    /// The same for `[dev-dependencies]`.
    pub dev_dependencies: BTreeSet<String>,
}

impl Project {
    /// Finds the nearest enclosing package by walking up from `start`.
    pub fn discover(start: &Path) -> Result<Project> {
        let start = if start.is_absolute() {
            start.to_path_buf()
        } else {
            std::env::current_dir()
                .context("cannot determine the current directory")?
                .join(start)
        };
        let mut dir = Some(start.as_path());
        while let Some(d) = dir {
            let manifest = d.join("Cargo.toml");
            if manifest.is_file() {
                // A virtual workspace manifest has no [package]; keep walking up.
                if let Some(project) = Project::from_manifest(&manifest)? {
                    return Ok(project);
                }
            }
            dir = d.parent();
        }
        bail!(
            "could not find a `Cargo.toml` with a `[package]` in `{}` or any parent directory\n\
             hint: run this inside a zapline project, or create one with `zapline new <name>`",
            start.display()
        )
    }

    /// Parses a manifest. Returns `Ok(None)` for manifests without `[package]`.
    pub fn from_manifest(manifest: &Path) -> Result<Option<Project>> {
        let text = std::fs::read_to_string(manifest)
            .with_context(|| format!("cannot read {}", manifest.display()))?;
        let root = manifest
            .parent()
            .ok_or_else(|| anyhow!("invalid manifest path"))?
            .to_path_buf();
        Project::parse(&root, &text)
            .with_context(|| format!("invalid manifest {}", manifest.display()))
    }

    /// Parses manifest text found in `root`; filesystem checks (for `src/main.rs`
    /// and friends) are relative to `root`.
    pub fn parse(root: &Path, text: &str) -> Result<Option<Project>> {
        let doc: toml::Table = toml::from_str(text).context("cannot parse Cargo.toml")?;
        let Some(package) = doc.get("package").and_then(|p| p.as_table()) else {
            return Ok(None);
        };
        let package_name = package
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow!("`[package]` has no string `name`"))?
            .to_string();
        let default_run = package
            .get("default-run")
            .and_then(|n| n.as_str())
            .map(str::to_string);

        let edition = package
            .get("edition")
            .and_then(|e| e.as_str())
            .unwrap_or("2021")
            .to_string();

        let lib_table = doc.get("lib").and_then(|l| l.as_table());
        let lib_path_exists = root.join("src/lib.rs").is_file()
            || lib_table
                .and_then(|l| l.get("path"))
                .and_then(|p| p.as_str())
                .is_some_and(|p| root.join(p).is_file());
        let lib_name = (lib_table.is_some() || lib_path_exists).then(|| {
            lib_table
                .and_then(|l| l.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| crate_ident(&package_name))
        });

        let mut bins: Vec<String> = Vec::new();
        if let Some(arr) = doc.get("bin").and_then(|b| b.as_array()) {
            bins.extend(
                arr.iter()
                    .filter_map(|b| b.get("name").and_then(|n| n.as_str()))
                    .map(str::to_string),
            );
        }
        if root.join("src/main.rs").is_file() && !bins.contains(&package_name) {
            bins.push(package_name.clone());
        }
        if let Ok(entries) = std::fs::read_dir(root.join("src/bin")) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = if path.is_file() && path.extension().is_some_and(|e| e == "rs") {
                    path.file_stem().map(|s| s.to_string_lossy().into_owned())
                } else if path.join("main.rs").is_file() {
                    path.file_name().map(|s| s.to_string_lossy().into_owned())
                } else {
                    None
                };
                if let Some(name) = name {
                    if !bins.contains(&name) {
                        bins.push(name);
                    }
                }
            }
        }
        bins.sort();

        let dependencies = dependency_names(doc.get("dependencies"));
        let dev_dependencies = dependency_names(doc.get("dev-dependencies"));

        Ok(Some(Project {
            root: root.to_path_buf(),
            package_name,
            lib_name,
            bins,
            default_run,
            edition,
            dependencies,
            dev_dependencies,
        }))
    }

    /// Whether `[dependencies]` mentions the crate.
    pub fn depends_on(&self, name: &str) -> bool {
        self.dependencies.contains(name)
    }

    /// Whether `[dev-dependencies]` (or `[dependencies]`) mentions the crate.
    pub fn dev_depends_on(&self, name: &str) -> bool {
        self.dev_dependencies.contains(name) || self.dependencies.contains(name)
    }

    /// Checks that `--bin <name>` refers to a real target, with a helpful error.
    pub fn check_bin(&self, bin: &str) -> Result<()> {
        if self.bins.iter().any(|b| b == bin) {
            return Ok(());
        }
        if self.bins.is_empty() {
            bail!(
                "package `{}` has no binary targets (add `src/main.rs`)",
                self.package_name
            );
        }
        bail!(
            "no binary named `{bin}` in package `{}`\navailable binaries: {}",
            self.package_name,
            self.bins.join(", ")
        )
    }

    /// The binary to run when `--bin` is not given: the only one, or `default-run`.
    pub fn default_bin(&self) -> Result<String> {
        if let Some(d) = &self.default_run {
            return Ok(d.clone());
        }
        match self.bins.as_slice() {
            [only] => Ok(only.clone()),
            [] => bail!(
                "package `{}` has no binary targets (add `src/main.rs`)",
                self.package_name
            ),
            many => bail!(
                "package `{}` has several binaries ({}); choose one with `--bin <name>`",
                self.package_name,
                many.join(", ")
            ),
        }
    }
}

/// Crate names in a dependency table, including the real names of renamed ones.
fn dependency_names(table: Option<&toml::Value>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Some(table) = table.and_then(|d| d.as_table()) {
        for (key, value) in table {
            names.insert(key.clone());
            if let Some(real) = value.get("package").and_then(|p| p.as_str()) {
                names.insert(real.to_string());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(root: &Path, text: &str) -> Project {
        Project::parse(root, text).unwrap().unwrap()
    }

    #[test]
    fn reads_package_and_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        let p = parse(
            dir.path(),
            r#"
            [package]
            name = "my-app"
            version = "0.1.0"

            [dependencies]
            zapline = "0.1"
            serde = { version = "1", features = ["derive"] }
            sj = { package = "serde_json", version = "1" }

            [dev-dependencies]
            tokio = "1"
            "#,
        );
        assert_eq!(p.package_name, "my-app");
        assert_eq!(p.lib_name.as_deref(), Some("my_app"));
        assert_eq!(p.bins, ["my-app"]);
        assert!(p.depends_on("zapline"));
        assert!(p.depends_on("serde"));
        assert!(p.depends_on("serde_json"));
        assert!(p.depends_on("sj"));
        assert!(!p.depends_on("schemars"));
        assert!(!p.depends_on("tokio"));
        assert!(p.dev_depends_on("tokio"));
        assert!(p.dev_depends_on("serde"));
        assert_eq!(p.default_bin().unwrap(), "my-app");
    }

    #[test]
    fn bin_only_package_has_no_lib() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        let p = parse(dir.path(), "[package]\nname = \"x\"\nversion = \"0.1.0\"\n");
        assert!(p.lib_name.is_none());
    }

    #[test]
    fn custom_lib_name_and_bin_targets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/bin")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/bin/worker.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(dir.path().join("src/bin/admin")).unwrap();
        std::fs::write(dir.path().join("src/bin/admin/main.rs"), "fn main() {}").unwrap();
        let p = parse(
            dir.path(),
            r#"
            [package]
            name = "svc"
            version = "0.1.0"
            default-run = "worker"

            [lib]
            name = "svc_core"

            [[bin]]
            name = "server"
            path = "src/server.rs"
            "#,
        );
        assert_eq!(p.lib_name.as_deref(), Some("svc_core"));
        assert_eq!(p.bins, ["admin", "server", "worker"]);
        assert_eq!(p.default_bin().unwrap(), "worker");
        assert!(p.check_bin("server").is_ok());
        let err = p.check_bin("nope").unwrap_err().to_string();
        assert!(err.contains("no binary named `nope`"), "{err}");
        assert!(err.contains("admin, server, worker"), "{err}");
    }

    #[test]
    fn several_bins_without_default_need_a_choice() {
        let dir = tempfile::tempdir().unwrap();
        let p = parse(
            dir.path(),
            "[package]\nname = \"s\"\nversion = \"0.1.0\"\n[[bin]]\nname = \"a\"\n[[bin]]\nname = \"b\"\n",
        );
        let err = p.default_bin().unwrap_err().to_string();
        assert!(err.contains("--bin"), "{err}");
    }

    #[test]
    fn virtual_workspace_manifest_is_not_a_package() {
        let r = Project::parse(Path::new("."), "[workspace]\nmembers = [\"a\"]\n").unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn invalid_manifest_is_an_error() {
        assert!(Project::parse(Path::new("."), "this is [not toml").is_err());
        assert!(Project::parse(Path::new("."), "[package]\nversion = \"1\"\n").is_err());
    }

    #[test]
    fn discover_walks_up_and_skips_virtual_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("a/Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        let p = Project::discover(&nested).unwrap();
        assert_eq!(p.package_name, "outer");
        assert_eq!(p.root, root);
    }

    #[test]
    fn discover_fails_outside_a_project() {
        let dir = tempfile::tempdir().unwrap();
        // The temp dir normally has no Cargo.toml in any parent.
        if Project::discover(dir.path()).is_ok() {
            return; // running inside an unusual layout; nothing to assert
        }
        let err = Project::discover(dir.path()).unwrap_err().to_string();
        assert!(err.contains("Cargo.toml"), "{err}");
        assert!(err.contains("zapline new"), "{err}");
    }
}
