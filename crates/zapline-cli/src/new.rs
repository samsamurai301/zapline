//! `zapline new`: scaffold a project from an embedded template.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::naming::{crate_ident, validate_crate_name};
use crate::project::Project;
use crate::render::{render, write_files, OutputFile};
use crate::templates::Template;
use crate::ui;

/// Where the generated project gets the `zapline` crate from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZaplineSource {
    /// crates.io, pinned to this CLI's release series (`"0.1"`).
    Registry,
    /// A local checkout of `crates/zapline`.
    Path(PathBuf),
}

/// Everything `zapline new` needs.
#[derive(Debug, Clone)]
pub struct NewOptions {
    /// Package name, e.g. `my-app`.
    pub name: String,
    /// Which layout to generate.
    pub template: Template,
    /// Directory the project directory is created in.
    pub parent: PathBuf,
    /// Use a local checkout of zapline instead of crates.io.
    pub zapline_path: Option<PathBuf>,
    /// Run `git init` (skipped inside an existing repository).
    pub git: bool,
}

/// What happened to version control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitOutcome {
    /// `--no-git` was given.
    Disabled,
    /// A new repository was created.
    Initialized,
    /// The target is already inside a repository.
    InsideRepository,
    /// `git` could not be run.
    Unavailable(String),
}

/// The result of scaffolding a project.
#[derive(Debug, Clone)]
pub struct Created {
    /// Project directory.
    pub root: PathBuf,
    /// Files written, relative to `root`.
    pub files: Vec<String>,
    /// Version-control outcome.
    pub git: GitOutcome,
}

/// The `zapline` version requirement for generated manifests: `0.1` for the
/// 0.1.x series, `1` for 1.x.
pub fn zapline_version_req() -> String {
    let mut parts = env!("CARGO_PKG_VERSION").split('.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0");
    if major == "0" {
        format!("0.{minor}")
    } else {
        major.to_string()
    }
}

/// Escapes `s` for use inside a TOML basic string.
pub fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The right-hand side of `zapline = ...` in `[dependencies]`.
pub fn zapline_dependency(source: &ZaplineSource, features: &[&str]) -> String {
    let feature_list = features
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ");
    match (source, features.is_empty()) {
        (ZaplineSource::Registry, true) => format!("\"{}\"", zapline_version_req()),
        (ZaplineSource::Registry, false) => format!(
            "{{ version = \"{}\", features = [{feature_list}] }}",
            zapline_version_req()
        ),
        (ZaplineSource::Path(p), true) => {
            format!("{{ path = \"{}\" }}", toml_escape(&p.to_string_lossy()))
        }
        (ZaplineSource::Path(p), false) => format!(
            "{{ path = \"{}\", features = [{feature_list}] }}",
            toml_escape(&p.to_string_lossy())
        ),
    }
}

/// An absolute, symlink-free path where possible (no `\\?\` prefixes on Windows).
fn absolute(path: &Path) -> Result<PathBuf> {
    if cfg!(windows) {
        std::path::absolute(path).with_context(|| format!("cannot resolve {}", path.display()))
    } else {
        path.canonicalize()
            .with_context(|| format!("cannot resolve {}", path.display()))
    }
}

/// Validates `--zapline-path` and returns the absolute path of `crates/zapline`.
/// Accepts the repository root too, since that is an easy thing to pass.
pub fn resolve_zapline_path(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        bail!("--zapline-path `{}` is not a directory", path.display());
    }
    let dir = absolute(path)?;
    for candidate in [dir.clone(), dir.join("crates").join("zapline")] {
        let manifest = candidate.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        if let Some(project) = Project::from_manifest(&manifest)? {
            if project.package_name == "zapline" {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "--zapline-path `{}` does not contain the `zapline` crate\n\
         hint: pass the directory with zapline's Cargo.toml (e.g. `<checkout>/crates/zapline`)",
        path.display()
    )
}

/// Renders every file of `template` for a project called `name`.
/// `zapline_dep` is the right-hand side of the `zapline = ...` dependency line.
pub fn render_template(
    template: Template,
    name: &str,
    zapline_dep: &str,
) -> Result<Vec<OutputFile>> {
    let crate_name = crate_ident(name);
    let vars = [
        ("name", name),
        ("crate_name", crate_name.as_str()),
        ("zapline", zapline_dep),
    ];
    template
        .files()
        .iter()
        .map(|f| {
            Ok(OutputFile {
                path: PathBuf::from(f.dest),
                contents: render(f.source, &vars)
                    .with_context(|| format!("while rendering template file {}", f.dest))?,
            })
        })
        .collect()
}

/// Scaffolds the project. Nothing is written if validation or rendering fails.
pub fn create(opts: &NewOptions) -> Result<Created> {
    validate_crate_name(&opts.name)?;

    let source = match &opts.zapline_path {
        Some(p) => ZaplineSource::Path(resolve_zapline_path(p)?),
        None => ZaplineSource::Registry,
    };
    let dep = zapline_dependency(&source, opts.template.zapline_features());
    let files = render_template(opts.template, &opts.name, &dep)?;

    let root = opts.parent.join(&opts.name);
    let created_dir = prepare_destination(&root)?;
    if let Err(err) = write_files(&root, &files, false) {
        if created_dir {
            let _ = fs::remove_dir_all(&root);
        }
        return Err(err);
    }

    let git = if opts.git {
        init_git(&root)
    } else {
        GitOutcome::Disabled
    };
    Ok(Created {
        root,
        files: files
            .iter()
            .map(|f| f.path.to_string_lossy().replace('\\', "/"))
            .collect(),
        git,
    })
}

/// Ensures `root` can receive a new project. Returns whether it had to be created.
fn prepare_destination(root: &Path) -> Result<bool> {
    if root.exists() {
        if !root.is_dir() {
            bail!("`{}` already exists and is not a directory", root.display());
        }
        let non_empty = fs::read_dir(root)
            .with_context(|| format!("cannot read {}", root.display()))?
            .next()
            .is_some();
        if non_empty {
            bail!(
                "destination `{}` already exists and is not empty\n\
                 hint: choose another name, or remove the directory first",
                root.display()
            );
        }
        return Ok(false);
    }
    fs::create_dir_all(root).with_context(|| format!("cannot create {}", root.display()))?;
    Ok(true)
}

fn init_git(root: &Path) -> GitOutcome {
    let parent = root.parent().unwrap_or(root);
    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(parent)
        .stderr(Stdio::null())
        .output();
    match inside {
        Err(e) => return GitOutcome::Unavailable(e.to_string()),
        Ok(out)
            if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true" =>
        {
            return GitOutcome::InsideRepository;
        }
        Ok(_) => {}
    }
    match Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
    {
        Ok(status) if status.success() => GitOutcome::Initialized,
        Ok(status) => GitOutcome::Unavailable(format!("`git init` exited with {status}")),
        Err(e) => GitOutcome::Unavailable(e.to_string()),
    }
}

/// Runs `zapline new` and prints the result.
pub fn run(opts: &NewOptions) -> Result<()> {
    let created = create(opts)?;
    ui::status(
        "Created",
        format!(
            "{} project `{}` in {} ({} files)",
            opts.template.name(),
            opts.name,
            created.root.display(),
            created.files.len()
        ),
    );
    match &created.git {
        GitOutcome::Initialized => ui::status("Initialized", "git repository"),
        GitOutcome::InsideRepository => {
            ui::status("Skipped", "git init (already inside a repository)")
        }
        GitOutcome::Unavailable(why) => ui::warn(format!("could not initialize git: {why}")),
        GitOutcome::Disabled => {}
    }
    if let Some(p) = &opts.zapline_path {
        ui::status("Using", format!("zapline from {}", p.display()));
    }

    let color = ui::stderr_color();
    let cd = if opts.parent == Path::new(".") || opts.parent.as_os_str().is_empty() {
        opts.name.clone()
    } else {
        created.root.display().to_string()
    };
    eprintln!();
    eprintln!("{}", ui::bold(color, "Next steps:"));
    eprintln!("  cd {cd}");
    eprintln!(
        "  zapline dev       {}",
        ui::dim(color, "# rebuild and restart on every change")
    );
    eprintln!("  cargo test        {}", ui::dim(color, "# run the tests"));
    if opts.template != Template::Minimal {
        eprintln!();
        eprintln!("Then open http://localhost:8080/docs for the interactive API docs.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{assert_rustfmt_clean, rustfmt_available};

    #[test]
    fn version_requirement_follows_the_release_series() {
        let req = zapline_version_req();
        assert!(!req.is_empty());
        assert!(env!("CARGO_PKG_VERSION").starts_with(&req), "{req}");
    }

    #[test]
    fn dependency_lines() {
        let req = zapline_version_req();
        assert_eq!(
            zapline_dependency(&ZaplineSource::Registry, &[]),
            format!("\"{req}\"")
        );
        assert_eq!(
            zapline_dependency(&ZaplineSource::Registry, &["compression", "ws"]),
            format!("{{ version = \"{req}\", features = [\"compression\", \"ws\"] }}")
        );
        assert_eq!(
            zapline_dependency(&ZaplineSource::Path("/x/zapline".into()), &[]),
            "{ path = \"/x/zapline\" }"
        );
        assert_eq!(
            zapline_dependency(&ZaplineSource::Path("/x/zapline".into()), &["compression"]),
            "{ path = \"/x/zapline\", features = [\"compression\"] }"
        );
    }

    #[test]
    fn toml_strings_are_escaped() {
        assert_eq!(toml_escape(r"C:\dev\zapline"), r"C:\\dev\\zapline");
        assert_eq!(toml_escape("a\"b"), "a\\\"b");
        assert_eq!(toml_escape("tab\there"), "tab\\there");
        assert_eq!(toml_escape("bell\u{7}"), "bell\\u0007");
        // Round-trips through a real TOML parser.
        let nasty = "C:\\dir with \"quotes\"\\x";
        let doc: toml::Table = toml::from_str(&format!("p = \"{}\"", toml_escape(nasty))).unwrap();
        assert_eq!(doc["p"].as_str(), Some(nasty));
    }

    const ALL: [Template; 3] = [Template::Minimal, Template::Api, Template::Full];

    fn rendered(template: Template, name: &str) -> Vec<OutputFile> {
        let dep = zapline_dependency(
            &ZaplineSource::Path("/opt/zapline/crates/zapline".into()),
            template.zapline_features(),
        );
        render_template(template, name, &dep).unwrap()
    }

    fn file<'a>(files: &'a [OutputFile], path: &str) -> &'a str {
        &files
            .iter()
            .find(|f| f.path == Path::new(path))
            .unwrap_or_else(|| panic!("missing {path}"))
            .contents
    }

    #[test]
    fn templates_render_without_leftover_placeholders() {
        for t in ALL {
            for f in rendered(t, "demo-app") {
                assert!(
                    !f.contents.contains("@@"),
                    "{}: {} still has a placeholder",
                    t.name(),
                    f.path.display()
                );
                assert!(
                    !f.contents.trim().is_empty(),
                    "{} is empty",
                    f.path.display()
                );
                assert!(
                    f.contents.ends_with('\n'),
                    "{} lacks a final newline",
                    f.path.display()
                );
            }
        }
    }

    #[test]
    fn manifests_are_valid_toml_with_the_right_dependencies() {
        for t in ALL {
            let files = rendered(t, "demo-app");
            let doc: toml::Table = toml::from_str(file(&files, "Cargo.toml")).unwrap();
            let package = doc["package"].as_table().unwrap();
            assert_eq!(package["name"].as_str(), Some("demo-app"));
            assert_eq!(package["edition"].as_str(), Some("2021"));
            let deps = doc["dependencies"].as_table().unwrap();
            let zapline = deps["zapline"].as_table().unwrap();
            assert_eq!(
                zapline["path"].as_str(),
                Some("/opt/zapline/crates/zapline")
            );
            if t == Template::Full {
                assert_eq!(
                    zapline["features"].as_array().unwrap()[0].as_str(),
                    Some("compression")
                );
            }
            let dev = doc["dev-dependencies"].as_table().unwrap();
            assert!(dev.contains_key("tokio"));
            if t != Template::Minimal {
                assert!(deps.contains_key("serde"));
                assert!(deps.contains_key("schemars"));
                assert!(deps.contains_key("validator"));
                assert!(dev.contains_key("serde_json"));
            }
        }
    }

    #[test]
    fn expected_files_exist() {
        let expect: [(Template, &[&str]); 3] = [
            (
                Template::Minimal,
                &[
                    "AGENTS.md",
                    "Cargo.toml",
                    "src/main.rs",
                    "src/lib.rs",
                    ".gitignore",
                    "README.md",
                    "tests/api.rs",
                ],
            ),
            (
                Template::Api,
                &[
                    "AGENTS.md",
                    "Cargo.toml",
                    "src/main.rs",
                    "src/lib.rs",
                    "src/models.rs",
                    "src/routes/mod.rs",
                    "src/routes/health.rs",
                    "src/routes/users.rs",
                    "tests/health.rs",
                    "tests/users.rs",
                    "Dockerfile",
                    ".dockerignore",
                    ".github/workflows/ci.yml",
                    ".env.example",
                    ".gitignore",
                    "README.md",
                ],
            ),
            (
                Template::Full,
                &[
                    "AGENTS.md",
                    "Cargo.toml",
                    "src/main.rs",
                    "src/lib.rs",
                    "src/config.rs",
                    "src/models.rs",
                    "src/repository.rs",
                    "src/routes/mod.rs",
                    "src/routes/users.rs",
                    "config/default.toml",
                    "config/development.toml",
                    "config/production.toml",
                    "tests/ops.rs",
                    "tests/users.rs",
                    "Dockerfile",
                    ".dockerignore",
                    ".github/workflows/ci.yml",
                    ".env.example",
                    ".gitignore",
                    "README.md",
                ],
            ),
        ];
        for (t, paths) in expect {
            let files = rendered(t, "demo-app");
            let got: Vec<String> = files
                .iter()
                .map(|f| f.path.to_string_lossy().into_owned())
                .collect();
            for p in paths {
                assert!(got.iter().any(|g| g == p), "{}: missing {p}", t.name());
            }
            assert_eq!(
                got.len(),
                paths.len(),
                "{}: unexpected extra files: {got:?}",
                t.name()
            );
        }
    }

    #[test]
    fn route_registration_markers_are_present() {
        for t in [Template::Api, Template::Full] {
            let files = rendered(t, "demo-app");
            let mod_rs = file(&files, "src/routes/mod.rs");
            assert!(mod_rs.contains("// zapline:routes:start"));
            assert!(mod_rs.contains("// zapline:routes:end"));
            assert!(mod_rs.find("routes:start").unwrap() < mod_rs.find("routes:end").unwrap());
        }
    }

    #[test]
    fn project_name_appears_where_expected() {
        let files = rendered(Template::Api, "demo-app");
        assert!(file(&files, "src/main.rs").contains("demo_app::app()"));
        assert!(file(&files, "tests/users.rs").contains("use demo_app::app;"));
        assert!(file(&files, "Dockerfile").contains("/app/target/release/demo-app"));
        let files = rendered(Template::Full, "demo-app");
        assert!(file(&files, "config/default.toml").contains("name = \"demo-app\""));
        assert!(file(&files, "tests/ops.rs").contains("\"demo-app\""));
    }

    #[test]
    fn ci_workflow_keeps_github_expressions_intact() {
        let files = rendered(Template::Api, "demo-app");
        let ci = file(&files, ".github/workflows/ci.yml");
        assert!(ci.contains("cargo fmt --all -- --check"));
        assert!(ci.contains("cargo clippy --all-targets -- -D warnings"));
        assert!(ci.contains("cargo test"));
    }

    #[test]
    fn dockerfile_is_multistage_and_non_root() {
        for t in [Template::Api, Template::Full] {
            let files = rendered(t, "demo-app");
            let d = file(&files, "Dockerfile");
            assert_eq!(
                d.matches("\nFROM ").count() + d.starts_with("FROM ") as usize,
                2
            );
            assert!(d.contains("AS builder"));
            assert!(d.contains("debian:bookworm-slim"));
            assert!(d.contains("USER app"));
        }
    }

    /// Generated Rust must already be rustfmt-clean.
    #[test]
    fn generated_rust_is_formatted() {
        if !rustfmt_available() {
            eprintln!("rustfmt not installed; skipping");
            return;
        }
        for t in ALL {
            for f in rendered(t, "demo-app") {
                if f.path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                assert_rustfmt_clean(&f.path.display().to_string(), &f.contents);
            }
        }
    }

    #[test]
    fn create_writes_the_project_and_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = NewOptions {
            name: "demo-app".into(),
            template: Template::Minimal,
            parent: tmp.path().to_path_buf(),
            zapline_path: None,
            git: false,
        };
        let created = create(&opts).unwrap();
        assert_eq!(created.root, tmp.path().join("demo-app"));
        assert_eq!(created.git, GitOutcome::Disabled);
        assert!(created.root.join("src/lib.rs").is_file());
        assert!(created.files.contains(&"tests/api.rs".to_string()));
        let manifest = fs::read_to_string(created.root.join("Cargo.toml")).unwrap();
        assert!(manifest.contains(&format!("zapline = \"{}\"", zapline_version_req())));

        let err = create(&opts).unwrap_err().to_string();
        assert!(err.contains("not empty"), "{err}");
    }

    #[test]
    fn an_empty_existing_directory_is_fine() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("demo")).unwrap();
        let opts = NewOptions {
            name: "demo".into(),
            template: Template::Minimal,
            parent: tmp.path().to_path_buf(),
            zapline_path: None,
            git: false,
        };
        create(&opts).unwrap();
        assert!(tmp.path().join("demo/Cargo.toml").is_file());
    }

    #[test]
    fn invalid_names_write_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["", "1abc", "has space", "../escape", "a/b", "zapline"] {
            let opts = NewOptions {
                name: bad.into(),
                template: Template::Minimal,
                parent: tmp.path().to_path_buf(),
                zapline_path: None,
                git: false,
            };
            assert!(create(&opts).is_err(), "{bad:?}");
        }
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_bad_zapline_path_is_reported_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = NewOptions {
            name: "demo".into(),
            template: Template::Minimal,
            parent: tmp.path().to_path_buf(),
            zapline_path: Some(tmp.path().join("nope")),
            git: false,
        };
        let err = create(&opts).unwrap_err().to_string();
        assert!(err.contains("not a directory"), "{err}");
        assert!(!tmp.path().join("demo").exists());

        // A directory without the zapline crate.
        fs::create_dir(tmp.path().join("other")).unwrap();
        fs::write(
            tmp.path().join("other/Cargo.toml"),
            "[package]\nname = \"other\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let opts = NewOptions {
            zapline_path: Some(tmp.path().join("other")),
            ..opts
        };
        let err = create(&opts).unwrap_err().to_string();
        assert!(
            err.contains("does not contain the `zapline` crate"),
            "{err}"
        );
    }

    #[test]
    fn zapline_path_accepts_the_repository_root() {
        let tmp = tempfile::tempdir().unwrap();
        let krate = tmp.path().join("crates/zapline");
        fs::create_dir_all(&krate).unwrap();
        fs::write(
            krate.join("Cargo.toml"),
            "[package]\nname = \"zapline\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/zapline\"]\n",
        )
        .unwrap();
        let resolved = resolve_zapline_path(tmp.path()).unwrap();
        assert!(resolved.ends_with("crates/zapline"), "{resolved:?}");
        assert!(resolve_zapline_path(&krate)
            .unwrap()
            .ends_with("crates/zapline"));
    }

    #[test]
    fn git_is_initialized_when_requested() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // Make sure the temp dir is not inside a repository.
        let inside = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(tmp.path())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let opts = NewOptions {
            name: "demo".into(),
            template: Template::Minimal,
            parent: tmp.path().to_path_buf(),
            zapline_path: None,
            git: true,
        };
        let created = create(&opts).unwrap();
        if inside {
            assert_eq!(created.git, GitOutcome::InsideRepository);
        } else {
            assert_eq!(created.git, GitOutcome::Initialized);
            assert!(created.root.join(".git").exists());
        }
    }
}
