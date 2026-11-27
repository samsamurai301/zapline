//! Copies the guide's chapters into OUT_DIR with every ```` ```rust ```` block
//! marked `no_run`: the check compiles each example but never runs it (many
//! start a server and would wait forever). Blocks already marked `ignore`,
//! `no_run`, `compile_fail` or `should_panic` keep their marks.
//!
//! It also writes `chapters.rs`, one doctest item per chapter with Rust in
//! it, so a new chapter is checked without anyone listing it.

use std::fmt::Write as _;
use std::{env, fs, path::Path};

fn main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../src");
    let out_dir = env::var("OUT_DIR").unwrap();
    let out = Path::new(&out_dir).join("guide");
    fs::create_dir_all(&out).unwrap();
    println!("cargo:rerun-if-changed={}", src.display());

    let mut paths: Vec<_> = fs::read_dir(&src)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    paths.sort();

    let mut chapters = String::new();
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).unwrap();
        let mut converted = String::with_capacity(text.len());
        let mut has_rust = false;
        for line in text.split_inclusive('\n') {
            let fence = line.trim_end();
            let marked = ["ignore", "no_run", "compile_fail", "should_panic"]
                .iter()
                .any(|m| fence.contains(m));
            if fence == "```rust" || fence.starts_with("```rust,") {
                has_rust = true;
            }
            if fence == "```rust" || (fence.starts_with("```rust,") && !marked) {
                converted.push_str(&line.replacen("```rust", "```rust,no_run", 1));
            } else {
                converted.push_str(line);
            }
        }
        let copy = out.join(&name);
        fs::write(&copy, converted).unwrap();
        if has_rust {
            // `admin.md` → `Admin`, `one-file.md` → `OneFile`.
            let ident: String = name
                .trim_end_matches(".md")
                .split(['-', '_', '.'])
                .map(|w| {
                    let mut c = w.chars();
                    c.next()
                        .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                        .unwrap_or_default()
                })
                .collect();
            writeln!(
                chapters,
                "#[doc = include_str!({:?})]\n#[cfg(doctest)]\npub struct {ident};",
                copy.display().to_string()
            )
            .unwrap();
        }
    }
    fs::write(Path::new(&out_dir).join("chapters.rs"), chapters).unwrap();
}
