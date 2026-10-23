//! One-file apps: `zapline run app.rs`, `zapline dev app.rs`, `zapline new app.rs`.
//!
//! A one-file app is a single `.rs` file. Its dependencies, if it needs more
//! than zapline, serde, serde_json and tokio, are declared in a `cargo` block
//! inside the file's leading `//!` comment, in Cargo.toml syntax:
//!
//! ```text
//! #!/usr/bin/env -S zapline run
//! //! ```cargo
//! //! [dependencies]
//! //! uuid = { version = "1", features = ["v4"] }
//! //! ```
//! ```
//!
//! The CLI turns the file into a hidden cargo project under the user cache
//! directory and builds that:
//!
//! ```text
//! <cache>/scripts/<hash of the file's path>/Cargo.toml          [[bin]] path = the file itself
//! <cache>/scripts/<hash>/.cargo/config.toml                     build.target-dir = <cache>/target
//! <cache>/target/                                               shared by every script
//! ```
//!
//! The binary target points at the user's file, so compiler errors name the
//! real file and line, and `include_str!` paths are relative to it. All
//! scripts share one target directory, so dependencies compile once. A run of
//! an unchanged script skips cargo entirely: the executable is reused when the
//! generated manifest is the same and none of the files in its dep-info
//! (written by cargo next to the executable) changed since it was built.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::build::{self, BuildOutcome};
use crate::new::{absolute, resolve_zapline_path, toml_escape, zapline_version_req};
use crate::project::Project;
use crate::render::render;
use crate::ui;

/// The line, inside the leading `//!` comment, that opens the dependency block.
pub const FENCE: &str = "```cargo";

/// Top-level tables a header may contain.
pub const ALLOWED_TABLES: &[&str] = &[
    "package",
    "dependencies",
    "dev-dependencies",
    "target",
    "features",
    "profile",
    "patch",
    "lints",
];

/// Keys a header's `[package]` table may set. The name and the targets come
/// from the file itself.
pub const ALLOWED_PACKAGE_KEYS: &[&str] = &[
    "edition",
    "rust-version",
    "version",
    "description",
    "authors",
    "license",
];

/// The starter written by `zapline new <name>.rs`.
const STARTER: &str = include_str!("../templates/script/app.rs.tmpl");

// ---- the header ------------------------------------------------------------------------

/// A problem in a script's header, at a 1-based line (and column, when known).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderError {
    /// 1-based line in the script.
    pub line: usize,
    /// 1-based column, when the problem is at a precise spot.
    pub column: Option<usize>,
    /// What is wrong.
    pub message: String,
}

impl HeaderError {
    fn new(line: usize, column: Option<usize>, message: impl Into<String>) -> Self {
        HeaderError {
            line,
            column,
            message: message.into(),
        }
    }

    /// The error as `file:line:col: message`, the way compilers print them.
    ///
    /// ```
    /// let err = zapline_cli::script::parse_header("//! ```cargo\n//! [dependencies\n//! ```\n")
    ///     .unwrap_err();
    /// assert!(err.in_file("app.rs").to_string().starts_with("app.rs:2:"));
    /// ```
    pub fn in_file(&self, file: &str) -> anyhow::Error {
        match self.column {
            Some(col) => anyhow!("{file}:{}:{col}: {}", self.line, self.message),
            None => anyhow!("{file}:{}: {}", self.line, self.message),
        }
    }
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.column {
            Some(col) => write!(f, "line {}:{col}: {}", self.line, self.message),
            None => write!(f, "line {}: {}", self.line, self.message),
        }
    }
}

impl std::error::Error for HeaderError {}

/// The parsed `cargo` block of a script.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Header {
    /// The block's TOML (empty when the script has no block).
    pub table: toml::Table,
    /// 1-based line of the opening fence, if there is a block.
    pub fence_line: Option<usize>,
}

/// One line of the block: its 1-based line number, the byte offset where its
/// TOML starts (after `//! `), and the TOML text.
type BlockLine<'a> = (usize, usize, &'a str);

/// Finds and parses the `cargo` block in the leading `//!` comment of
/// `source`. A shebang line may come first; the comment ends at the first line
/// of code. A script without a block gets an empty header.
///
/// ```
/// use zapline_cli::script::parse_header;
///
/// let header = parse_header(
///     "#!/usr/bin/env -S zapline run\n\
///      //! A tiny app.\n\
///      //!\n\
///      //! ```cargo\n\
///      //! [dependencies]\n\
///      //! uuid = \"1\"\n\
///      //! ```\n\
///      fn main() {}\n",
/// )
/// .unwrap();
/// assert_eq!(header.fence_line, Some(4));
/// assert_eq!(header.table["dependencies"]["uuid"].as_str(), Some("1"));
///
/// assert!(parse_header("fn main() {}").unwrap().table.is_empty());
/// ```
pub fn parse_header(source: &str) -> Result<Header, HeaderError> {
    let mut open: Option<(usize, Vec<BlockLine<'_>>)> = None;
    let mut found: Option<(usize, Vec<BlockLine<'_>>)> = None;
    for (i, raw) in source.lines().enumerate() {
        let n = i + 1;
        if i == 0 && raw.starts_with("#!") && !raw.starts_with("#![") {
            continue; // shebang
        }
        let trimmed = raw.trim_start();
        let indent = raw.len() - trimmed.len();
        if let Some(rest) = trimmed.strip_prefix("//!") {
            let (text, offset) = match rest.strip_prefix(' ') {
                Some(t) => (t, indent + 4),
                None => (rest, indent + 3),
            };
            if let Some((_, lines)) = open.as_mut() {
                if text.trim_start().starts_with("```") {
                    found = open.take();
                } else {
                    lines.push((n, offset, text));
                }
            } else if text.trim() == FENCE {
                if let Some((first, _)) = &found {
                    return Err(HeaderError::new(
                        n,
                        None,
                        format!(
                            "only one ```cargo block is allowed (the first starts on line {first})"
                        ),
                    ));
                }
                open = Some((n, Vec::new()));
            }
            continue;
        }
        if let Some((start, _)) = &open {
            return Err(HeaderError::new(
                n,
                None,
                format!(
                    "every line of the ```cargo block (opened on line {start}) must start with \
                     `//!`; close the block with a `//! ```` line"
                ),
            ));
        }
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        break; // the first line of code ends the header
    }
    if let Some((start, _)) = open {
        return Err(HeaderError::new(
            start,
            None,
            "the ```cargo block is never closed; end it with a `//! ```` line",
        ));
    }
    let Some((fence, lines)) = found else {
        return Ok(Header::default());
    };
    let text: String = lines.iter().map(|(_, _, t)| format!("{t}\n")).collect();
    let table: toml::Table = toml::from_str(&text).map_err(|e| {
        let (line, column) = match e.span() {
            Some(span) => position(&lines, span.start),
            None => (fence, None),
        };
        HeaderError::new(
            line,
            column,
            format!("invalid ```cargo block: {}", e.message().trim_end()),
        )
    })?;
    validate(&table, &lines, fence)?;
    Ok(Header {
        table,
        fence_line: Some(fence),
    })
}

/// Maps a byte offset in the joined block text to a line and column of the file.
fn position(lines: &[BlockLine<'_>], offset: usize) -> (usize, Option<usize>) {
    let mut start = 0;
    for (n, col, text) in lines {
        let end = start + text.len() + 1;
        if offset < end {
            return (*n, Some(col + (offset - start) + 1));
        }
        start = end;
    }
    match lines.last() {
        Some((n, col, text)) => (*n, Some(col + text.len() + 1)),
        None => (1, None),
    }
}

/// The line that defines `key` (`[key]`, `[key.x]`, `[[key]]`, `key = ...`),
/// or the fence line.
fn line_of(lines: &[BlockLine<'_>], fence: usize, key: &str) -> usize {
    lines
        .iter()
        .find(|(_, _, text)| {
            let t = text.trim_start();
            let t = t.trim_start_matches('[').trim_start();
            t.strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with(['=', '.', ']']))
        })
        .map_or(fence, |(n, _, _)| *n)
}

fn validate(table: &toml::Table, lines: &[BlockLine<'_>], fence: usize) -> Result<(), HeaderError> {
    for (key, value) in table {
        if !ALLOWED_TABLES.contains(&key.as_str()) {
            let hint = match key.as_str() {
                "bin" | "lib" | "example" | "test" | "bench" | "workspace" => {
                    " (a one-file app is a single binary: the file itself)"
                }
                "build-dependencies" => " (a one-file app has no build script)",
                _ => "",
            };
            return Err(HeaderError::new(
                line_of(lines, fence, key),
                None,
                format!(
                    "`{key}` is not supported in a ```cargo block{hint}; use {}",
                    ALLOWED_TABLES
                        .iter()
                        .map(|t| format!("[{t}]"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        if !value.is_table() {
            return Err(HeaderError::new(
                line_of(lines, fence, key),
                None,
                format!("`{key}` must be a table: write `[{key}]` on its own line"),
            ));
        }
    }
    if let Some(package) = table.get("package").and_then(|p| p.as_table()) {
        for key in package.keys() {
            if !ALLOWED_PACKAGE_KEYS.contains(&key.as_str()) {
                let why = match key.as_str() {
                    "name" => "; the package is named after the file",
                    _ => "",
                };
                return Err(HeaderError::new(
                    line_of(lines, fence, key),
                    None,
                    format!(
                        "`package.{key}` can't be set in a one-file app{why}; allowed: {}",
                        ALLOWED_PACKAGE_KEYS.join(", ")
                    ),
                ));
            }
        }
    }
    Ok(())
}

// ---- the generated project ---------------------------------------------------------

/// Where scripts' generated projects and their shared target directory live:
/// `$XDG_CACHE_HOME/zapline`, else `~/.cache/zapline` (`%LOCALAPPDATA%\zapline`
/// on Windows). `env` reads an environment variable.
///
/// ```
/// use std::path::PathBuf;
/// use zapline_cli::script::cache_dir_from;
///
/// let env = |k: &str| (k == "HOME").then(|| "/home/ada".into());
/// assert_eq!(cache_dir_from(env).unwrap(), PathBuf::from("/home/ada/.cache/zapline"));
/// let env = |k: &str| (k == "XDG_CACHE_HOME").then(|| "/var/cache/ada".into());
/// assert_eq!(cache_dir_from(env).unwrap(), PathBuf::from("/var/cache/ada/zapline"));
/// ```
pub fn cache_dir_from(env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let var = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    // The XDG spec says relative values are invalid and must be ignored.
    if let Some(xdg) = var("XDG_CACHE_HOME").filter(|p| p.is_absolute()) {
        return Ok(xdg.join("zapline"));
    }
    if cfg!(windows) {
        if let Some(local) = var("LOCALAPPDATA") {
            return Ok(local.join("zapline"));
        }
    }
    match var("HOME").or_else(|| var("USERPROFILE")) {
        Some(home) => Ok(home.join(".cache").join("zapline")),
        None => bail!("cannot find a cache directory: set XDG_CACHE_HOME or HOME"),
    }
}

/// [`cache_dir_from`] with the process environment.
///
/// ```
/// assert!(zapline_cli::script::cache_dir().unwrap().ends_with("zapline"));
/// ```
pub fn cache_dir() -> Result<PathBuf> {
    cache_dir_from(|k| std::env::var_os(k))
}

/// A cargo package name from a file stem: `my app` → `my-app`, `2fa` → `app-2fa`.
///
/// ```
/// use zapline_cli::script::package_name;
///
/// assert_eq!(package_name("hello").unwrap(), "hello");
/// assert_eq!(package_name("My App!").unwrap(), "my-app");
/// assert_eq!(package_name("2fa").unwrap(), "app-2fa");
/// assert!(package_name("!!!").is_err());
/// ```
pub fn package_name(stem: &str) -> Result<String> {
    let mut out = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if !out.chars().any(|c| c.is_ascii_alphanumeric()) {
        bail!("`{stem}` can't name an app: use letters, digits, `-` or `_` in the file name");
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        return Ok(format!("app-{out}"));
    }
    Ok(out)
}

/// A one-file app and the hidden project it builds in.
#[derive(Debug, Clone)]
pub struct Script {
    /// Absolute path of the `.rs` file.
    pub path: PathBuf,
    /// The path as the user wrote it, for messages.
    pub shown: String,
    /// Package name, from the file name.
    pub package: String,
    /// Binary name: the package name plus a hash, since all scripts share a target dir.
    pub bin: String,
    /// The generated project's directory.
    pub dir: PathBuf,
    /// The shared target directory (unless `CARGO_TARGET_DIR` overrides it).
    pub target_dir: PathBuf,
}

/// A script turned into a cargo project.
#[derive(Debug, Clone)]
pub struct Materialized {
    /// The generated project.
    pub project: Project,
    /// Changes whenever the build inputs other than source files change.
    pub key: String,
    /// Inputs the dep-info does not list: the manifests of path dependencies.
    pub extra_inputs: Vec<PathBuf>,
}

impl Script {
    /// Describes the script at `path` with its project under `cache`.
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zapline_cli::script::{cache_dir, Script};
    ///
    /// let script = Script::locate(Path::new("app.rs"), &cache_dir()?)?;
    /// println!("{}", script.dir.display());
    /// # anyhow::Ok(())
    /// ```
    pub fn locate(path: &Path, cache: &Path) -> Result<Script> {
        let shown = path.display().to_string();
        if path.extension().is_none_or(|e| e != "rs") {
            bail!("`{shown}` is not a `.rs` file\nhint: a one-file app is a Rust source file, e.g. `zapline run app.rs`");
        }
        if !path.is_file() {
            bail!("`{shown}` does not exist\nhint: create one with `zapline new {shown}`");
        }
        let path = absolute(path)?;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let package = package_name(&stem)?;
        let digest = Sha256::digest(path.to_string_lossy().as_bytes());
        let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        Ok(Script {
            bin: format!("{package}-{}", &hash[..8]),
            dir: cache.join("scripts").join(&hash),
            target_dir: cache.join("target"),
            path,
            shown,
            package,
        })
    }

    /// The generated `Cargo.toml`.
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join("Cargo.toml")
    }

    /// The text of the generated `Cargo.toml` for `header`.
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use zapline_cli::script::{cache_dir, parse_header, Script};
    /// let script = Script::locate(Path::new("app.rs"), &cache_dir()?)?;
    /// let header = parse_header(&std::fs::read_to_string("app.rs")?)?;
    /// assert!(script.manifest(&header)?.contains("[[bin]]"));
    /// # anyhow::Ok(())
    /// ```
    pub fn manifest(&self, header: &Header) -> Result<String> {
        let mut user = header.table.clone();
        let base = self.path.parent().unwrap_or(Path::new("/"));
        absolutize_paths(&mut user, base);

        let mut package: Vec<(String, toml::Value)> = vec![
            ("name".into(), self.package.clone().into()),
            ("version".into(), "0.0.0".into()),
            ("edition".into(), "2021".into()),
            ("publish".into(), false.into()),
        ];
        // Only the script itself is a target.
        for key in [
            "autobins",
            "autoexamples",
            "autotests",
            "autobenches",
            "build",
        ] {
            package.push((key.into(), false.into()));
        }
        if let Some(toml::Value::Table(p)) = user.remove("package") {
            for (key, value) in p {
                match package.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = value,
                    None => package.push((key, value)),
                }
            }
        }

        let mut deps = match user.remove("dependencies") {
            Some(toml::Value::Table(d)) => d,
            _ => toml::Table::new(),
        };
        add_default_dependencies(&mut deps);

        let mut out = format!(
            "# Generated by zapline from {}.\n\
             # Do not edit: change the ```cargo block at the top of the script instead.\n",
            self.path.display()
        );
        write_section(
            &mut out,
            "[package]",
            package.iter().map(|(k, v)| (k.as_str(), v)),
        );
        let bin = [
            ("name", toml::Value::from(self.bin.clone())),
            ("path", self.path.to_string_lossy().into_owned().into()),
        ];
        write_section(&mut out, "[[bin]]", bin.iter().map(|(k, v)| (*k, v)));
        write_section(
            &mut out,
            "[dependencies]",
            deps.iter().map(|(k, v)| (k.as_str(), v)),
        );
        for (key, value) in &user {
            if let toml::Value::Table(t) = value {
                let header = format!("[{}]", toml_key(key));
                write_section(&mut out, &header, t.iter().map(|(k, v)| (k.as_str(), v)));
            }
        }
        // Its own workspace, so a parent directory's workspace never claims it.
        write_section(&mut out, "[workspace]", std::iter::empty());
        Ok(out)
    }

    /// Reads the script, parses its header and writes the generated project
    /// (files are only rewritten when their content changes).
    pub fn materialize(&self) -> Result<Materialized> {
        let source = fs::read_to_string(&self.path)
            .with_context(|| format!("cannot read {}", self.shown))?;
        let header = parse_header(&source).map_err(|e| e.in_file(&self.shown))?;
        let manifest = self.manifest(&header)?;
        let config = format!(
            "# Generated by zapline: every script shares one target directory.\n\
             [build]\ntarget-dir = \"{}\"\n",
            toml_escape(&self.target_dir.to_string_lossy())
        );
        fs::create_dir_all(self.dir.join(".cargo"))
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        write_if_changed(&self.manifest_path(), &manifest)?;
        write_if_changed(&self.dir.join(".cargo/config.toml"), &config)?;
        let project = Project::from_manifest(&self.manifest_path())?
            .ok_or_else(|| anyhow!("the generated manifest has no [package]"))?;
        let mut hasher = Sha256::new();
        hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
        hasher.update(manifest.as_bytes());
        hasher.update(config.as_bytes());
        if let Some(t) = std::env::var_os("CARGO_TARGET_DIR") {
            hasher.update(t.to_string_lossy().as_bytes());
        }
        let key = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut extra_inputs = Vec::new();
        if let Ok(generated) = toml::from_str::<toml::Table>(&manifest) {
            for (key, value) in &generated {
                if key != "bin" {
                    collect_path_manifests(value, &mut extra_inputs);
                }
            }
        }
        Ok(Materialized {
            project,
            key,
            extra_inputs,
        })
    }

    fn stamp_path(&self, release: bool) -> PathBuf {
        let profile = if release { "release" } else { "debug" };
        self.dir.join(format!("last-build-{profile}.json"))
    }

    /// The executable of the last build, if nothing that goes into it changed.
    pub fn fresh_executable(
        &self,
        key: &str,
        extra_inputs: &[PathBuf],
        release: bool,
    ) -> Option<PathBuf> {
        let text = fs::read_to_string(self.stamp_path(release)).ok()?;
        let stamp: Stamp = serde_json::from_str(&text).ok()?;
        (stamp.key == key && is_up_to_date(&stamp.exe, extra_inputs)).then_some(stamp.exe)
    }

    fn record_build(&self, key: &str, release: bool, exe: &Path) {
        let stamp = Stamp {
            key: key.to_string(),
            exe: exe.to_path_buf(),
        };
        if let Ok(json) = serde_json::to_string(&stamp) {
            let _ = fs::write(self.stamp_path(release), json);
        }
    }

    /// Builds the script, reusing the last executable when nothing changed.
    pub fn build(&self, m: &Materialized, release: bool) -> Result<BuildOutcome> {
        self.build_with(m, release, |_| {})
    }

    /// [`Script::build`], calling `on_spawn` with cargo's process id if a
    /// build runs (see [`build::build_with`]).
    pub fn build_with(
        &self,
        m: &Materialized,
        release: bool,
        on_spawn: impl FnOnce(u32),
    ) -> Result<BuildOutcome> {
        if let Some(exe) = self.fresh_executable(&m.key, &m.extra_inputs, release) {
            return Ok(BuildOutcome::Built(exe));
        }
        let outcome = build::build_with(&m.project, &self.bin, release, on_spawn)?;
        if let BuildOutcome::Built(exe) = &outcome {
            self.record_build(&m.key, release, exe);
        }
        Ok(outcome)
    }
}

/// What [`Script::fresh_executable`] remembers about the last build.
#[derive(Debug, Serialize, Deserialize)]
struct Stamp {
    key: String,
    exe: PathBuf,
}

/// Appends `header` and one `key = value` line per entry, values inline.
fn write_section<'a>(
    out: &mut String,
    header: &str,
    entries: impl Iterator<Item = (&'a str, &'a toml::Value)>,
) {
    out.push('\n');
    out.push_str(header);
    out.push('\n');
    for (key, value) in entries {
        out.push_str(&format!("{} = {value}\n", toml_key(key)));
    }
}

/// A TOML key: bare when it can be, quoted otherwise.
fn toml_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare {
        key.to_string()
    } else {
        format!("\"{}\"", toml_escape(key))
    }
}

fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    if fs::read_to_string(path).is_ok_and(|old| old == contents) {
        return Ok(());
    }
    fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))
}

/// Adds zapline, serde, serde_json and tokio unless the header lists them,
/// and gives a `zapline = { features = [...] }` without a source the version
/// of this CLI's release series.
fn add_default_dependencies(deps: &mut toml::Table) {
    let defaults: [(&str, toml::Value); 4] = [
        ("zapline", zapline_version_req().into()),
        ("serde", versioned("1", &["derive"])),
        ("serde_json", "1".into()),
        ("tokio", versioned("1", &["full"])),
    ];
    for (name, value) in defaults {
        let existing = deps.iter_mut().find(|(key, v)| {
            key.as_str() == name || v.get("package").and_then(|p| p.as_str()) == Some(name)
        });
        match existing {
            None => {
                deps.insert(name.into(), value);
            }
            Some((_, toml::Value::Table(t))) => {
                let has_source = ["version", "path", "git", "workspace", "registry"]
                    .iter()
                    .any(|k| t.contains_key(*k));
                if !has_source {
                    let version = match &value {
                        toml::Value::Table(d) => d.get("version").cloned(),
                        v => Some(v.clone()),
                    };
                    if let Some(v) = version {
                        t.insert("version".into(), v);
                    }
                }
            }
            Some(_) => {}
        }
    }
}

fn versioned(version: &str, features: &[&str]) -> toml::Value {
    let mut t = toml::Table::new();
    t.insert("version".into(), version.into());
    t.insert(
        "features".into(),
        toml::Value::Array(features.iter().map(|f| (*f).into()).collect()),
    );
    t.into()
}

/// Rewrites relative `path = "..."` dependencies (in every dependency table and
/// `[patch]`) to absolute ones, relative to the script's directory.
fn absolutize_paths(table: &mut toml::Table, base: &Path) {
    fn fix_deps(deps: &mut toml::Value, base: &Path) {
        let Some(deps) = deps.as_table_mut() else {
            return;
        };
        for (_, dep) in deps.iter_mut() {
            let Some(table) = dep.as_table_mut() else {
                continue;
            };
            let relative = match table.get("path").and_then(|p| p.as_str()) {
                Some(p) if Path::new(p).is_relative() => p.to_string(),
                _ => continue,
            };
            let absolute = base.join(relative).to_string_lossy().into_owned();
            table.insert("path".into(), absolute.into());
        }
    }
    for key in ["dependencies", "dev-dependencies"] {
        if let Some(d) = table.get_mut(key) {
            fix_deps(d, base);
        }
    }
    for section in ["target", "patch"] {
        let Some(t) = table.get_mut(section).and_then(|t| t.as_table_mut()) else {
            continue;
        };
        for (_, inner) in t.iter_mut() {
            if section == "patch" {
                fix_deps(inner, base);
            } else if let Some(inner) = inner.as_table_mut() {
                for key in ["dependencies", "dev-dependencies"] {
                    if let Some(d) = inner.get_mut(key) {
                        fix_deps(d, base);
                    }
                }
            }
        }
    }
}

// ---- freshness -------------------------------------------------------------------------

/// The files listed in a Makefile-style dep-info file (`target: dep dep ...`),
/// with `\ ` unescaped. The target itself is left out.
///
/// ```
/// use std::path::PathBuf;
/// use zapline_cli::script::dep_info_files;
///
/// assert_eq!(
///     dep_info_files("/t/app: /src/app.rs /src/my\\ data.txt\n"),
///     [PathBuf::from("/src/app.rs"), PathBuf::from("/src/my data.txt")]
/// );
/// ```
pub fn dep_info_files(text: &str) -> Vec<PathBuf> {
    // Only the first rule matters; a trailing `\` continues it on the next line.
    let logical = text.replace("\\\r\n", " ").replace("\\\n", " ");
    for line in logical.lines() {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&' ') => {
                    current.push(' ');
                    chars.next();
                }
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        // The first token ending in ':' is the target.
        if let Some(i) = tokens.iter().position(|t| t.ends_with(':')) {
            return tokens[i + 1..].iter().map(PathBuf::from).collect();
        }
    }
    Vec::new()
}

/// Adds `<path>/Cargo.toml` for every `path = "..."` dependency under `value`.
fn collect_path_manifests(value: &toml::Value, out: &mut Vec<PathBuf>) {
    let Some(table) = value.as_table() else {
        return;
    };
    if let Some(p) = table.get("path").and_then(|p| p.as_str()) {
        out.push(Path::new(p).join("Cargo.toml"));
        return;
    }
    for inner in table.values() {
        collect_path_manifests(inner, out);
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Whether `exe` exists and is newer than every file in its dep-info (`exe`
/// with a `.d` extension, which cargo writes next to it) and than `extra`.
pub fn is_up_to_date(exe: &Path, extra: &[PathBuf]) -> bool {
    let (Some(built), Ok(info)) = (modified(exe), fs::read_to_string(exe.with_extension("d")))
    else {
        return false;
    };
    let files = dep_info_files(&info);
    !files.is_empty()
        && files
            .iter()
            .chain(extra)
            .all(|f| f.is_absolute() && modified(f).is_some_and(|m| m <= built))
}

// ---- commands --------------------------------------------------------------------------

/// Settings for [`run`].
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// The script.
    pub file: PathBuf,
    /// Build with `--release`.
    pub release: bool,
    /// Arguments passed to the app.
    pub args: Vec<String>,
    /// Only write the generated project and print its manifest path.
    pub print_manifest: bool,
}

/// Opens the script at `file` with its project in the user cache directory.
pub fn open(file: &Path) -> Result<Script> {
    Script::locate(file, &cache_dir()?)
}

/// The generated project of the script at `file`, ready for any command that
/// works on a project (`zapline routes app.rs`, `zapline check app.rs`).
pub fn project(file: &Path) -> Result<Project> {
    Ok(open(file)?.materialize()?.project)
}

/// `zapline run app.rs [args]`: builds (or reuses) the script's executable and
/// runs it in the current directory, exiting with its exit code.
pub fn run(opts: &RunOptions) -> Result<()> {
    let script = open(&opts.file)?;
    let m = script.materialize()?;
    if opts.print_manifest {
        println!("{}", script.manifest_path().display());
        return Ok(());
    }
    let exe = match script.build(&m, opts.release)? {
        BuildOutcome::Built(exe) => exe,
        BuildOutcome::Failed(_) => bail!("could not compile `{}`", script.shown),
    };
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&opts.args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Replaces this process: signals, stdin and the exit code are the app's own.
        let err = cmd.exec();
        Err(anyhow!(err).context(format!("cannot start {}", exe.display())))
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .with_context(|| format!("cannot start {}", exe.display()))?;
        std::process::exit(status.code().unwrap_or(1))
    }
}

/// The starter `zapline new <name>.rs` writes. `zapline_path`, when given, is
/// the absolute path of a local `crates/zapline`.
///
/// ```
/// let source = zapline_cli::script::starter("hello.rs", None).unwrap();
/// assert!(source.starts_with("#!/usr/bin/env -S zapline run"));
/// assert!(zapline_cli::script::parse_header(&source).is_ok());
/// ```
pub fn starter(file_name: &str, zapline_path: Option<&Path>) -> Result<String> {
    let stem = file_name.strip_suffix(".rs").unwrap_or(file_name);
    let name = package_name(stem)?;
    let dependencies = match zapline_path {
        Some(p) => format!(
            "//! zapline = {{ path = \"{}\" }}",
            toml_escape(&p.to_string_lossy())
        ),
        None => "//! # More crates go here, e.g.: uuid = { version = \"1\", features = [\"v4\"] }"
            .to_string(),
    };
    render(
        STARTER,
        &[
            ("name", name.as_str()),
            ("file", file_name),
            ("dependencies", dependencies.as_str()),
        ],
    )
}

/// `zapline new <name>.rs`: writes a one-file starter app. Returns its path.
pub fn create(path: &Path, zapline_path: Option<&Path>) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let zapline = zapline_path.map(resolve_zapline_path).transpose()?;
    let source = starter(&file_name, zapline.as_deref())?;
    if path.exists() {
        bail!(
            "`{}` already exists\nhint: choose another name, or run it with `zapline dev {}`",
            path.display(),
            path.display()
        );
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    fs::write(path, source).with_context(|| format!("cannot write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `./hello.rs` runs it, through the shebang line.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    Ok(path.to_path_buf())
}

/// Runs `zapline new <name>.rs` and prints what to do next.
pub fn run_new(path: &Path, zapline_path: Option<&Path>) -> Result<()> {
    let created = create(path, zapline_path)?;
    let shown = created.display();
    ui::status("Created", format!("one-file app {shown}"));
    let color = ui::stderr_color();
    eprintln!();
    eprintln!("{}", ui::bold(color, "Next steps:"));
    eprintln!(
        "  zapline dev {shown}    {}",
        ui::dim(color, "# serve on :8080, restart on every save")
    );
    eprintln!(
        "  zapline test {shown}   {}",
        ui::dim(color, "# run the tests inside the file")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(source: &str) -> HeaderError {
        parse_header(source).unwrap_err()
    }

    #[test]
    fn a_script_without_a_block_has_an_empty_header() {
        for source in [
            "",
            "fn main() {}",
            "#!/usr/bin/env -S zapline run\nfn main() {}",
            "//! Just docs.\n//!\n//! ```\n//! let x = 1;\n//! ```\nfn main() {}",
        ] {
            assert_eq!(parse_header(source).unwrap(), Header::default(), "{source}");
        }
    }

    #[test]
    fn the_block_is_found_after_a_shebang_docs_and_comments() {
        let source = "#!/usr/bin/env -S zapline run\n\
                      // SPDX-License-Identifier: MIT\n\
                      \n\
                      //! My app.\n\
                      //!\n\
                      //! ```cargo\n\
                      //! [package]\n\
                      //! edition = \"2024\"\n\
                      //!\n\
                      //! [dependencies]\n\
                      //!uuid = { version = \"1\", features = [\"v4\"] }\n\
                      //! ```\n\
                      \n\
                      use zapline::prelude::*;\n";
        let h = parse_header(source).unwrap();
        assert_eq!(h.fence_line, Some(6));
        assert_eq!(h.table["package"]["edition"].as_str(), Some("2024"));
        assert_eq!(
            h.table["dependencies"]["uuid"]["version"].as_str(),
            Some("1")
        );
    }

    #[test]
    fn a_block_after_the_first_line_of_code_is_not_a_header() {
        let source = "use x;\n//! ```cargo\n//! [dependencies]\n//! a = \"1\"\n//! ```\n";
        assert!(parse_header(source).unwrap().table.is_empty());
    }

    #[test]
    fn crlf_line_endings_work() {
        let h = parse_header("//! ```cargo\r\n//! [dependencies]\r\n//! a = \"1\"\r\n//! ```\r\n")
            .unwrap();
        assert_eq!(h.table["dependencies"]["a"].as_str(), Some("1"));
    }

    #[test]
    fn toml_errors_point_at_the_line_and_column() {
        let e = err("//! app\n//! ```cargo\n//! [dependencies]\n//! uuid = \n//! ```\n");
        assert_eq!(e.line, 4, "{e}");
        assert!(e.column.is_some(), "{e}");
        assert!(e.message.contains("invalid ```cargo block"), "{e}");
        let e = err("//! ```cargo\n//! [dependencies]\n//!     serde = \"1\" junk\n//! ```\n");
        assert_eq!(e.line, 3, "{e}");
        // `//!     serde = "1" junk`: `junk` starts at byte 20, column 21.
        assert_eq!(e.column, Some(21), "{e}");
        let shown = e.in_file("app.rs").to_string();
        assert!(
            shown.starts_with("app.rs:3:21: invalid ```cargo block"),
            "{shown}"
        );
    }

    #[test]
    fn unclosed_and_broken_blocks_are_reported_where_they_start() {
        let e = err("//! ```cargo\n//! [dependencies]\n");
        assert_eq!(
            (e.line, e.message.contains("never closed")),
            (1, true),
            "{e}"
        );
        let e = err("//! ```cargo\n//! [dependencies]\nfn main() {}\n");
        assert_eq!(e.line, 3, "{e}");
        assert!(e.message.contains("opened on line 1"), "{e}");
        let e = err("//! ```cargo\n//! ```\n//! ```cargo\n//! ```\n");
        assert_eq!(e.line, 3, "{e}");
        assert!(e.message.contains("only one"), "{e}");
        assert_eq!(
            e.in_file("x.rs").to_string(),
            format!("x.rs:3: {}", e.message)
        );
    }

    #[test]
    fn unsupported_tables_and_package_keys_are_rejected_at_their_line() {
        let e = err("//! ```cargo\n//! [dependencies]\n//! a = \"1\"\n//! [[bin]]\n//! name = \"x\"\n//! ```\n");
        assert_eq!(e.line, 4, "{e}");
        assert!(e.message.contains("`bin` is not supported"), "{e}");
        assert!(e.message.contains("single binary"), "{e}");
        let e =
            err("//! ```cargo\n//! [package]\n//! edition = \"2024\"\n//! name = \"x\"\n//! ```\n");
        assert_eq!(e.line, 4, "{e}");
        assert!(e.message.contains("named after the file"), "{e}");
        let e = err("//! ```cargo\n//! dependencies = 3\n//! ```\n");
        assert_eq!(e.line, 2, "{e}");
        assert!(e.message.contains("must be a table"), "{e}");
    }

    #[test]
    fn cache_dir_follows_xdg_then_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| OsString::from(*v))
            }
        };
        assert_eq!(
            cache_dir_from(env(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")])).unwrap(),
            PathBuf::from("/x/zapline")
        );
        // Empty and relative XDG values are ignored.
        for xdg in ["", "relative/dir"] {
            let pairs: &'static [(&str, &str)] =
                Box::leak(vec![("XDG_CACHE_HOME", xdg), ("HOME", "/h")].into_boxed_slice());
            if !cfg!(windows) {
                assert_eq!(
                    cache_dir_from(env(pairs)).unwrap(),
                    PathBuf::from("/h/.cache/zapline")
                );
            }
        }
        assert!(cache_dir_from(env(&[])).is_err());
    }

    fn script_in(dir: &Path, name: &str, source: &str) -> Script {
        let file = dir.join(name);
        fs::write(&file, source).unwrap();
        Script::locate(&file, &dir.join("cache")).unwrap()
    }

    #[test]
    fn locate_names_the_project_after_the_file_and_hashes_its_path() {
        let tmp = tempfile::tempdir().unwrap();
        let a = script_in(tmp.path(), "My App.rs", "fn main() {}");
        assert_eq!(a.package, "my-app");
        assert!(a.bin.starts_with("my-app-") && a.bin.len() == "my-app-".len() + 8);
        assert!(a.dir.starts_with(tmp.path().join("cache/scripts")));
        assert_eq!(a.target_dir, tmp.path().join("cache/target"));
        // Same name elsewhere: another project and binary.
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let b = script_in(&tmp.path().join("sub"), "My App.rs", "fn main() {}");
        assert_ne!(a.dir, b.dir);
        assert_ne!(a.bin, b.bin);
        // Stable for the same file.
        let again = Script::locate(&a.path, &tmp.path().join("cache")).unwrap();
        assert_eq!(again.dir, a.dir);

        let e = Script::locate(&tmp.path().join("nope.rs"), tmp.path()).unwrap_err();
        assert!(e.to_string().contains("zapline new"), "{e}");
        fs::write(tmp.path().join("app.txt"), "").unwrap();
        let e = Script::locate(&tmp.path().join("app.txt"), tmp.path()).unwrap_err();
        assert!(e.to_string().contains("not a `.rs` file"), "{e}");
    }

    fn manifest_of(source: &str) -> (tempfile::TempDir, Script, toml::Table) {
        let tmp = tempfile::tempdir().unwrap();
        let script = script_in(tmp.path(), "app.rs", source);
        let header = parse_header(source).unwrap();
        let text = script.manifest(&header).unwrap();
        let table = toml::from_str(&text).unwrap();
        (tmp, script, table)
    }

    #[test]
    fn the_manifest_has_one_bin_at_the_script_and_default_dependencies() {
        let (_tmp, script, m) = manifest_of("fn main() {}");
        assert_eq!(m["package"]["name"].as_str(), Some("app"));
        assert_eq!(m["package"]["edition"].as_str(), Some("2021"));
        assert_eq!(m["package"]["autobins"].as_bool(), Some(false));
        assert_eq!(m["package"]["build"].as_bool(), Some(false));
        let bins = m["bin"].as_array().unwrap();
        assert_eq!(bins.len(), 1);
        assert_eq!(bins[0]["name"].as_str(), Some(script.bin.as_str()));
        assert_eq!(
            bins[0]["path"].as_str(),
            Some(script.path.to_string_lossy().as_ref())
        );
        let deps = m["dependencies"].as_table().unwrap();
        assert_eq!(
            deps["zapline"].as_str(),
            Some(zapline_version_req().as_str())
        );
        assert_eq!(deps["serde"]["features"][0].as_str(), Some("derive"));
        assert_eq!(deps["serde_json"].as_str(), Some("1"));
        assert_eq!(deps["tokio"]["features"][0].as_str(), Some("full"));
        assert!(m["workspace"].as_table().unwrap().is_empty());
    }

    #[test]
    fn header_dependencies_override_and_extend_the_defaults() {
        let (_tmp, _, m) = manifest_of(
            "//! ```cargo\n\
             //! [package]\n\
             //! edition = \"2024\"\n\
             //! [dependencies]\n\
             //! zapline = { features = [\"ws\"] }\n\
             //! tokio = \"1.40\"\n\
             //! json = { package = \"serde_json\", version = \"1.0.100\" }\n\
             //! uuid = \"1\"\n\
             //! [dev-dependencies]\n\
             //! pretty_assertions = \"1\"\n\
             //! [profile.release]\n\
             //! lto = true\n\
             //! ```\n",
        );
        assert_eq!(m["package"]["edition"].as_str(), Some("2024"));
        let deps = m["dependencies"].as_table().unwrap();
        assert_eq!(
            deps["zapline"]["version"].as_str(),
            Some(zapline_version_req().as_str())
        );
        assert_eq!(deps["zapline"]["features"][0].as_str(), Some("ws"));
        assert_eq!(deps["tokio"].as_str(), Some("1.40"));
        // A renamed serde_json counts as serde_json.
        assert!(!deps.contains_key("serde_json"));
        assert_eq!(deps["uuid"].as_str(), Some("1"));
        assert!(deps.contains_key("serde"));
        assert_eq!(
            m["dev-dependencies"]["pretty_assertions"].as_str(),
            Some("1")
        );
        assert_eq!(m["profile"]["release"]["lto"].as_bool(), Some(true));
    }

    #[test]
    fn relative_path_dependencies_are_relative_to_the_script() {
        let (_tmp, script, m) = manifest_of(
            "//! ```cargo\n\
             //! [dependencies]\n\
             //! zapline = { path = \"../zapline/crates/zapline\" }\n\
             //! abs = { path = \"/opt/abs\" }\n\
             //! [target.'cfg(unix)'.dependencies]\n\
             //! unixy = { path = \"vendor/unixy\" }\n\
             //! [patch.crates-io]\n\
             //! uuid = { path = \"vendor/uuid\" }\n\
             //! ```\n",
        );
        let dir = script.path.parent().unwrap();
        let deps = &m["dependencies"];
        assert_eq!(
            deps["zapline"]["path"].as_str(),
            Some(
                dir.join("../zapline/crates/zapline")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        // A path source means no version is added.
        assert!(deps["zapline"].get("version").is_none());
        assert_eq!(deps["abs"]["path"].as_str(), Some("/opt/abs"));
        assert_eq!(
            m["target"]["cfg(unix)"]["dependencies"]["unixy"]["path"].as_str(),
            Some(dir.join("vendor/unixy").to_string_lossy().as_ref())
        );
        assert_eq!(
            m["patch"]["crates-io"]["uuid"]["path"].as_str(),
            Some(dir.join("vendor/uuid").to_string_lossy().as_ref())
        );
    }

    #[test]
    fn materialize_writes_the_project_once_and_reports_header_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let script = script_in(tmp.path(), "svc.rs", "fn main() {}\n");
        let m = script.materialize().unwrap();
        assert_eq!(m.project.root, script.dir);
        assert_eq!(m.project.bins, std::slice::from_ref(&script.bin));
        assert_eq!(m.project.default_bin().unwrap(), script.bin);
        assert!(m.project.depends_on("zapline"));
        let config = fs::read_to_string(script.dir.join(".cargo/config.toml")).unwrap();
        let config: toml::Table = toml::from_str(&config).unwrap();
        assert_eq!(
            config["build"]["target-dir"].as_str(),
            Some(script.target_dir.to_string_lossy().as_ref())
        );

        // Unchanged: same key, file untouched.
        let manifest = script.manifest_path();
        let before = modified(&manifest).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let again = script.materialize().unwrap();
        assert_eq!(again.key, m.key);
        assert_eq!(modified(&manifest).unwrap(), before);

        // A new dependency changes the manifest and the key.
        fs::write(
            &script.path,
            "//! ```cargo\n//! [dependencies]\n//! uuid = \"1\"\n//! local = { path = \"local\" }\n//! ```\nfn main() {}\n",
        )
        .unwrap();
        let changed = script.materialize().unwrap();
        assert_ne!(changed.key, m.key);
        assert!(changed.project.depends_on("uuid"));
        // A path dependency's manifest is an input the dep-info does not list.
        assert!(m.extra_inputs.is_empty());
        assert_eq!(
            changed.extra_inputs,
            [script.path.parent().unwrap().join("local/Cargo.toml")]
        );

        // A broken header names the file and line.
        fs::write(&script.path, "//! ```cargo\n//! [dependencies\n//! ```\n").unwrap();
        let e = script.materialize().unwrap_err().to_string();
        assert!(e.starts_with(&format!("{}:2:", script.shown)), "{e}");
    }

    #[test]
    fn dep_info_parsing_handles_escapes_and_continuations() {
        assert_eq!(
            dep_info_files("/t/a: /a.rs \\\n /b\\ c.rs\n\n/a.rs:\n"),
            [PathBuf::from("/a.rs"), PathBuf::from("/b c.rs")]
        );
        assert!(dep_info_files("").is_empty());
        assert!(dep_info_files("garbage").is_empty());
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn an_executable_is_fresh_until_one_of_its_inputs_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let script = script_in(tmp.path(), "app.rs", "fn main() {}\n");
        let data = tmp.path().join("data.txt");
        fs::write(&data, "x").unwrap();
        let exe = tmp.path().join("app-bin");
        fs::write(&exe, "binary").unwrap();
        let t0 = SystemTime::now() - std::time::Duration::from_secs(100);
        set_mtime(&script.path, t0);
        set_mtime(&data, t0);
        set_mtime(&exe, t0 + std::time::Duration::from_secs(10));

        // No dep-info yet.
        assert!(!is_up_to_date(&exe, &[]));
        let escaped = |p: &Path| p.to_string_lossy().replace(' ', "\\ ");
        fs::write(
            exe.with_extension("d"),
            format!(
                "{}: {} {}\n",
                escaped(&exe),
                escaped(&script.path),
                escaped(&data)
            ),
        )
        .unwrap();
        assert!(is_up_to_date(&exe, &[]));

        fs::create_dir_all(&script.dir).unwrap();
        script.record_build("k1", false, &exe);
        assert_eq!(script.fresh_executable("k1", &[], false), Some(exe.clone()));
        // Another manifest or profile: not fresh.
        assert_eq!(script.fresh_executable("k2", &[], false), None);
        assert_eq!(script.fresh_executable("k1", &[], true), None);

        // Editing an included file makes it stale.
        set_mtime(&data, t0 + std::time::Duration::from_secs(20));
        assert!(!is_up_to_date(&exe, &[]));
        assert_eq!(script.fresh_executable("k1", &[], false), None);
        set_mtime(&data, t0);
        // So does deleting one, or the executable.
        fs::remove_file(&data).unwrap();
        assert!(!is_up_to_date(&exe, &[]));
        fs::write(&data, "x").unwrap();
        set_mtime(&data, t0);
        assert!(is_up_to_date(&exe, &[]));
        // An extra input (a path dependency's manifest) counts too.
        let dep_manifest = tmp.path().join("Cargo.toml");
        fs::write(&dep_manifest, "").unwrap();
        set_mtime(&dep_manifest, t0);
        assert!(is_up_to_date(&exe, std::slice::from_ref(&dep_manifest)));
        set_mtime(&dep_manifest, t0 + std::time::Duration::from_secs(20));
        assert!(!is_up_to_date(&exe, &[dep_manifest]));
        fs::remove_file(&exe).unwrap();
        assert!(!is_up_to_date(&exe, &[]));
    }

    #[test]
    fn the_starter_has_a_header_tests_and_the_local_zapline_when_asked() {
        let source = starter("hello-world.rs", None).unwrap();
        assert!(source.contains("Hello from hello-world!"));
        assert!(source.contains("zapline dev hello-world.rs"));
        assert!(source.contains("#[cfg(test)]"));
        let h = parse_header(&source).unwrap();
        assert!(h.table["dependencies"].as_table().unwrap().is_empty());

        let source = starter("hello.rs", Some(Path::new("/src/zapline/crates/zapline"))).unwrap();
        let h = parse_header(&source).unwrap();
        assert_eq!(
            h.table["dependencies"]["zapline"]["path"].as_str(),
            Some("/src/zapline/crates/zapline")
        );
        assert!(starter("!!.rs", None).is_err());
    }

    #[test]
    fn the_starter_is_rustfmt_clean() {
        use crate::test_util::{assert_rustfmt_clean, rustfmt_available};
        if !rustfmt_available() {
            return;
        }
        assert_rustfmt_clean("hello.rs", &starter("hello.rs", None).unwrap());
        let local = starter("hello.rs", Some(Path::new("/src/zapline/crates/zapline"))).unwrap();
        assert_rustfmt_clean("hello.rs (local zapline)", &local);
    }

    #[test]
    fn create_writes_an_executable_file_and_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("apps/hello.rs");
        create(&path, None).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        assert!(source.starts_with("#!/usr/bin/env -S zapline run\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "{mode:o}");
        }
        let e = create(&path, None).unwrap_err().to_string();
        assert!(e.contains("already exists"), "{e}");
        let e = create(&tmp.path().join("x.rs"), Some(&tmp.path().join("missing")))
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a directory"), "{e}");
        assert!(!tmp.path().join("x.rs").exists());
    }
}
