//! Tailwind CSS without Node: the official standalone `tailwindcss` binary,
//! pinned to one version, downloaded on first use and checked against its
//! SHA-256 before it ever runs.
//!
//! A project uses Tailwind when `styles/app.css` exists (`zapline g tailwind`
//! creates it). `zapline css` compiles it to `static/app.css`, and
//! `zapline dev` keeps that file up to date while you edit templates.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::generate::Report;
use crate::project::Project;
use crate::ui;

/// The Tailwind release zapline downloads.
pub const VERSION: &str = "v4.3.3";
/// The stylesheet Tailwind reads, relative to the project root.
pub const INPUT: &str = "styles/app.css";
/// The stylesheet it writes, served by `App::assets("/static", "static")`.
pub const OUTPUT: &str = "static/app.css";
/// Use this binary instead of downloading one.
pub const BINARY_ENV: &str = "ZAPLINE_TAILWIND";

/// SHA-256 of each release asset of [`VERSION`] (from its `sha256sums.txt`).
const CHECKSUMS: &[(&str, &str)] = &[
    (
        "tailwindcss-linux-arm64",
        "55fd0b241214eff3de1e8ee4f22796662f2d2e7a49bcfca7477cfd0bac398195",
    ),
    (
        "tailwindcss-linux-arm64-musl",
        "71ea4be79c9de9827545682df3e040053fb535d37c71ed2cfdedf9385a0868e0",
    ),
    (
        "tailwindcss-linux-x64",
        "dc61b3ac6b8c9ca874c0cc4c57b2409791a64c5540404ca5f5367360babc313a",
    ),
    (
        "tailwindcss-linux-x64-musl",
        "a04d34ceacc8f52cbe8920ad846cdeb61d3d0021dba32db0d1f77c9d9fad7a6c",
    ),
    (
        "tailwindcss-macos-arm64",
        "cdf646702987a743464dff4d9c60fd4480d1c1e73dd819a9a67f1078815dce9d",
    ),
    (
        "tailwindcss-macos-x64",
        "7922e0953f2110c05976e3bf58f14e643d90427575e766b7d433f5f80cbee7e1",
    ),
    (
        "tailwindcss-windows-x64.exe",
        "e0e260ce048014e9268f6237ff18f8ccf02cef521cbd0ae04e82c2cdf7aa3955",
    ),
];

/// Whether the project at `root` compiles its CSS with Tailwind.
pub fn is_used(root: &Path) -> bool {
    root.join(INPUT).is_file()
}

/// The release asset for an OS (`std::env::consts::OS`), architecture and
/// libc, if Tailwind publishes one.
pub fn asset_for(os: &str, arch: &str, musl: bool) -> Option<&'static str> {
    let name = match (os, arch, musl) {
        ("linux", "x86_64", false) => "tailwindcss-linux-x64",
        ("linux", "x86_64", true) => "tailwindcss-linux-x64-musl",
        ("linux", "aarch64", false) => "tailwindcss-linux-arm64",
        ("linux", "aarch64", true) => "tailwindcss-linux-arm64-musl",
        ("macos", "x86_64", _) => "tailwindcss-macos-x64",
        ("macos", "aarch64", _) => "tailwindcss-macos-arm64",
        ("windows", "x86_64", _) => "tailwindcss-windows-x64.exe",
        _ => return None,
    };
    Some(name)
}

fn checksum(asset: &str) -> Option<&'static str> {
    CHECKSUMS
        .iter()
        .find(|(name, _)| *name == asset)
        .map(|(_, sum)| *sum)
}

/// Where downloaded tools are kept: the platform's per-user cache directory.
fn cache_dir() -> Option<PathBuf> {
    let var = |name| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")?
    } else if cfg!(target_os = "macos") {
        var("HOME")?.join("Library/Caches")
    } else {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|h| h.join(".cache")))?
    };
    Some(base.join("zapline"))
}

/// Hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The `tailwindcss` binary: `$ZAPLINE_TAILWIND`, or the pinned release,
/// downloaded into the user cache the first time.
pub fn binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(BINARY_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let musl = cfg!(target_env = "musl");
    let Some(asset) = asset_for(std::env::consts::OS, std::env::consts::ARCH, musl) else {
        bail!(
            "Tailwind publishes no standalone binary for {}-{}; install `tailwindcss` and set {BINARY_ENV} to its path",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    let dir = cache_dir()
        .with_context(|| format!("no cache directory (HOME is not set); set {BINARY_ENV}"))?;
    let exe = if cfg!(windows) { ".exe" } else { "" };
    let path = dir.join(format!("tailwindcss-{VERSION}{exe}"));
    if path.is_file() {
        return Ok(path);
    }
    download(asset, &path)?;
    Ok(path)
}

fn download(asset: &str, dest: &Path) -> Result<()> {
    let expected = checksum(asset).context("no checksum for this Tailwind build")?;
    let dir = dest.parent().context("cache path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let url =
        format!("https://github.com/tailwindlabs/tailwindcss/releases/download/{VERSION}/{asset}");
    let partial = dest.with_extension("download");
    ui::status(
        "Downloading",
        format!("Tailwind CSS {VERSION} (once, into {})", dir.display()),
    );
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "2", "-o"])
        .arg(&partial)
        .arg(&url)
        .stdin(Stdio::null())
        .status();
    let ok = match status {
        Ok(s) => s.success(),
        Err(e) => bail!(
            "cannot run curl to download Tailwind ({e}); download {url} yourself and set {BINARY_ENV} to its path"
        ),
    };
    if !ok {
        let _ = fs::remove_file(&partial);
        bail!("downloading {url} failed; check the network, or download it yourself and set {BINARY_ENV} to its path");
    }
    let actual = sha256_file(&partial)?;
    if actual != expected {
        let _ = fs::remove_file(&partial);
        bail!("{url} has SHA-256 {actual}, expected {expected}; refusing to run it");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&partial, dest)
        .with_context(|| format!("cannot move Tailwind to {}", dest.display()))?;
    Ok(())
}

fn command(root: &Path, minify: bool) -> Result<Command> {
    if !is_used(root) {
        bail!("no {INPUT} in this project; run `zapline g tailwind` to set up Tailwind");
    }
    let mut cmd = Command::new(binary()?);
    cmd.args(["--input", INPUT, "--output", OUTPUT])
        .current_dir(root);
    if minify {
        cmd.arg("--minify");
    }
    Ok(cmd)
}

/// `zapline css`: compiles `styles/app.css` to `static/app.css` once.
pub fn build(root: &Path, minify: bool) -> Result<()> {
    let mut cmd = command(root, minify)?;
    let status = cmd
        .stdin(Stdio::null())
        .status()
        .context("cannot run tailwindcss")?;
    if !status.success() {
        bail!("tailwindcss exited with {status}");
    }
    ui::status("Built", OUTPUT);
    Ok(())
}

/// A `tailwindcss --watch` for `zapline dev`. It stops by itself when the
/// returned stdin is dropped (Tailwind's watcher exits when stdin closes),
/// so it never outlives the dev server.
pub fn watch(root: &Path) -> Result<(Child, ChildStdin)> {
    let mut child = command(root, false)?
        .arg("--watch")
        .stdin(Stdio::piped())
        .spawn()
        .context("cannot start tailwindcss")?;
    let stdin = child.stdin.take().context("tailwindcss has no stdin")?;
    Ok((child, stdin))
}

/// External stylesheets (`<link rel="stylesheet" href="https://...">`) in a
/// layout, one per line. `g tailwind` moves them into `styles/app.css`
/// (layer `base`), so Tailwind's utilities win over them.
pub fn external_stylesheets(layout: &str) -> Vec<(String, String)> {
    layout
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("<link") && t.contains("stylesheet") && t.ends_with('>')
        })
        .filter_map(|l| {
            let href = l.split("href=\"").nth(1)?.split('"').next()?;
            (href.starts_with("https://") || href.starts_with("http://"))
                .then(|| (l.to_string(), href.to_string()))
        })
        .collect()
}

/// The `styles/app.css` that `zapline g tailwind` writes.
///
/// With no other stylesheets it is plain `@import "tailwindcss"`. With
/// `external` ones (such as Pico in the fullstack template) they keep
/// styling plain elements in place of Tailwind's reset. The CSS that was in
/// `static/app.css` moves into layer `components`. Unlayered CSS would beat
/// every utility class, which is why both go in layers.
pub fn input_css(existing: Option<&str>, external: &[String]) -> String {
    let mut css = String::new();
    if external.is_empty() {
        css.push_str("@import \"tailwindcss\";\n");
    } else {
        css.push_str(
            "/* The stylesheets your layout linked style plain elements in place of\n   \
             Tailwind's reset, and utility classes override them. For Tailwind alone,\n   \
             replace everything up to @source with: @import \"tailwindcss\"; */\n\
             @layer theme, base, components, utilities;\n",
        );
        for url in external {
            css.push_str(&format!("@import url(\"{url}\") layer(base);\n"));
        }
        css.push_str(
            "@import \"tailwindcss/theme.css\" layer(theme);\n\
             @import \"tailwindcss/utilities.css\" layer(utilities);\n",
        );
    }
    css.push_str(
        "/* Classes used in templates/ end up in static/app.css. */\n@source \"../templates\";\n",
    );
    match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(old) => {
            css.push_str(
                "\n/* Your own CSS (moved from static/app.css). */\n@layer components {\n",
            );
            for line in old.lines() {
                if line.trim().is_empty() {
                    css.push('\n');
                } else {
                    css.push_str(&format!("  {line}\n"));
                }
            }
            css.push_str("}\n");
        }
        None => css.push_str("\n@layer components {\n  /* Your own CSS goes here. */\n}\n"),
    }
    css
}

/// `zapline g tailwind`: writes `styles/app.css` and builds `static/app.css`.
pub fn generate(project: &Project, force: bool) -> Result<Report> {
    let root = &project.root;
    let input = root.join(INPUT);
    if input.exists() && !force {
        bail!("{INPUT} already exists (use --force to overwrite)");
    }
    let read = |path: &Path| match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    // On --force, the old output is Tailwind's own; don't copy it back in.
    let existing = if input.exists() {
        None
    } else {
        read(&root.join(OUTPUT))?
    };
    let layout_path = root.join("templates/base.html");
    let layout = read(&layout_path)?;
    let links = layout
        .as_deref()
        .map(external_stylesheets)
        .unwrap_or_default();
    let urls: Vec<String> = links.iter().map(|(_, url)| url.clone()).collect();

    fs::create_dir_all(root.join("styles"))?;
    fs::write(&input, input_css(existing.as_deref(), &urls))
        .with_context(|| format!("cannot write {}", input.display()))?;
    let mut report = Report {
        created: vec![PathBuf::from(INPUT)],
        ..Report::default()
    };
    if let (Some(layout), false) = (&layout, links.is_empty()) {
        let kept: String = layout
            .split_inclusive('\n')
            .filter(|l| {
                !links
                    .iter()
                    .any(|(line, _)| l.trim_end_matches(['\r', '\n']) == line)
            })
            .collect();
        fs::write(&layout_path, kept)
            .with_context(|| format!("cannot write {}", layout_path.display()))?;
        report.modified.push(PathBuf::from("templates/base.html"));
    }
    let link = if layout
        .as_deref()
        .is_some_and(|l| l.contains("asset('app.css')"))
    {
        "templates/base.html already links it with `{{ asset('app.css') }}`".to_string()
    } else {
        "link it from your layout with `<link rel=\"stylesheet\" href=\"{{ asset('app.css') }}\">` \
         and serve it with `.assets(\"/static\", \"static\")`"
            .to_string()
    };
    match build(root, false) {
        Ok(()) => report.modified.push(PathBuf::from(OUTPUT)),
        Err(e) => report.notes.push(format!(
            "{OUTPUT} was not built yet: {e:#}\n\
             fix that, then run `zapline css`"
        )),
    }
    report.notes.push(format!(
        "Tailwind is set up: use its classes in templates/ and `zapline dev` rebuilds {OUTPUT} as you save\n\
         {link}\n\
         commit {OUTPUT} (it is what gets deployed); `zapline css --minify` makes it smaller"
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_platform_has_a_checksum() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("windows", "x86_64"),
        ] {
            for musl in [false, true] {
                let asset = asset_for(os, arch, musl).unwrap();
                let sum = checksum(asset).unwrap();
                assert_eq!(sum.len(), 64);
                assert!(sum.bytes().all(|b| b.is_ascii_hexdigit()));
            }
        }
        assert_eq!(asset_for("freebsd", "x86_64", false), None);
        assert_eq!(asset_for("windows", "aarch64", false), None);
    }

    #[test]
    fn input_keeps_existing_css_in_a_layer() {
        let css = input_css(Some("body { color: red }\n\na { b: c }\n"), &[]);
        assert!(css.starts_with("@import \"tailwindcss\";\n"));
        assert!(css.contains("@source \"../templates\";"));
        assert!(css.ends_with("@layer components {\n  body { color: red }\n\n  a { b: c }\n}\n"));
        assert!(input_css(None, &[]).contains("Your own CSS goes here"));
        assert_eq!(input_css(Some("  \n"), &[]), input_css(None, &[]));
    }

    #[test]
    fn external_stylesheets_go_in_the_base_layer() {
        let layout =
            "<head>\n  <link rel=\"stylesheet\" href=\"https://cdn.example/pico.css\">\n  \
                      <link rel=\"stylesheet\" href=\"{{ asset('app.css') }}\">\n  \
                      <link rel=\"icon\" href=\"https://x/icon.png\">\n</head>\n";
        let links = external_stylesheets(layout);
        assert_eq!(
            links,
            vec![(
                "  <link rel=\"stylesheet\" href=\"https://cdn.example/pico.css\">".to_string(),
                "https://cdn.example/pico.css".to_string()
            )]
        );
        let css = input_css(None, &["https://cdn.example/pico.css".into()]);
        assert!(css.contains("@layer theme, base, components, utilities;\n"));
        assert!(css.contains("@import url(\"https://cdn.example/pico.css\") layer(base);\n"));
        assert!(css.contains("@import \"tailwindcss/utilities.css\" layer(utilities);\n"));
        assert!(!css.contains("@import \"tailwindcss\";\n"));
    }

    #[test]
    fn hashes_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        fs::write(&path, "abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn css_needs_the_input() {
        let dir = tempfile::tempdir().unwrap();
        let err = build(dir.path(), false).unwrap_err().to_string();
        assert!(err.contains("zapline g tailwind"), "{err}");
    }
}
