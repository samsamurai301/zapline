//! `zapline generate job <name>`: a durable job in `src/jobs/<name>.rs`,
//! registered in `src/jobs/mod.rs`, wired into `app()` and tested.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use super::{apply, crate_name_of, format_files, warn_if_not_zapline, ModEdit, Report};
use crate::naming::{pascal_case, snake_case, validate_identifier};
use crate::project::Project;
use crate::render::{render, OutputFile};
use crate::templates;

const MODULES: &str = "// zapline:jobs:modules";
const REGISTER: &str = "// zapline:jobs:register";

/// Adds `pub mod <module>;` and `.register::<module::Type>()` to a jobs
/// `mod.rs`. `None` when the markers are missing.
pub fn register_job(mod_rs: &str, module: &str, ty: &str) -> Option<String> {
    let decl = format!("pub mod {module};");
    let call = format!(".register::<{module}::{ty}>()");
    let mut out = String::with_capacity(mod_rs.len() + 64);
    let (mut saw_modules, mut saw_register) = (false, false);
    let has_decl = mod_rs.lines().any(|l| l.trim() == decl);
    let has_call = mod_rs.lines().any(|l| l.trim() == call);
    for line in mod_rs.split_inclusive('\n') {
        let indent = &line[..line.len() - line.trim_start().len()];
        match line.trim() {
            MODULES => {
                saw_modules = true;
                if !has_decl {
                    out.push_str(&format!("{indent}{decl}\n"));
                }
            }
            REGISTER => {
                saw_register = true;
                if !has_call {
                    out.push_str(&format!("{indent}    {call}\n"));
                }
            }
            _ => {}
        }
        out.push_str(line);
    }
    (saw_modules && saw_register).then_some(out)
}

/// Declares `jobs` and registers the queue in a fullstack-style `lib.rs`
/// (`.database(db)` inside `pub fn app(db: Database)`). `None` if the file
/// has another shape; unchanged text if it is already wired.
pub fn wire_lib(lib: &str) -> Option<String> {
    if lib.contains(".jobs(jobs::register)") {
        return Some(lib.to_string());
    }
    if !lib.contains("pub fn app(db: Database)") {
        return None;
    }
    let mut out = String::with_capacity(lib.len() + 48);
    let (mut declared, mut registered) = (lib.lines().any(|l| l.trim() == "pub mod jobs;"), false);
    for line in lib.split_inclusive('\n') {
        let trimmed = line.trim();
        // Before `routes`, where `cargo fmt` would sort it.
        if !declared && trimmed == "pub mod routes;" {
            out.push_str("pub mod jobs;\n");
            declared = true;
        }
        out.push_str(line);
        if !registered && trimmed == ".database(db)" {
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push_str(&format!("{indent}.jobs(jobs::register)\n"));
            registered = true;
        }
    }
    (declared && registered).then_some(out)
}

/// What `generate job` writes, before touching the disk.
pub fn files(krate: &str, module: &str, with_test: bool) -> Result<Vec<OutputFile>> {
    let pascal = pascal_case(module);
    let vars = [
        ("name", module),
        ("Pascal", pascal.as_str()),
        ("crate_name", krate),
    ];
    let mut files = vec![OutputFile {
        path: PathBuf::from(format!("src/jobs/{module}.rs")),
        contents: render(templates::JOB, &vars)?,
    }];
    if with_test {
        files.push(OutputFile {
            path: PathBuf::from(format!("tests/{module}_job.rs")),
            contents: render(templates::JOB_TEST, &vars)?,
        });
    }
    Ok(files)
}

/// `zapline generate job <name>`.
pub fn generate(project: &Project, name: &str, force: bool) -> Result<Report> {
    let module = snake_case(name);
    validate_identifier(&module, name)?;
    if !project.root.join(crate::db::MIGRATIONS_DIR).is_dir() {
        bail!(
            "jobs are stored in the database: this project has no migrations/ directory\n\
             hint: start from `zapline new <name> --template fullstack`, or register \
             `App::database(..)` and add one with `zapline db new init`"
        );
    }
    let ty = pascal_case(&module);
    let lib_path = project.root.join("src/lib.rs");
    let lib = fs::read_to_string(&lib_path).unwrap_or_default();
    let manifest = fs::read_to_string(project.root.join("Cargo.toml")).unwrap_or_default();
    let sqlite = manifest.contains("\"sqlite\"");
    let wired = wire_lib(&lib);
    let with_test = project.lib_name.is_some() && wired.is_some() && sqlite;

    let rel = PathBuf::from("src/jobs/mod.rs");
    let mod_path = project.root.join(&rel);
    let (current, is_new) = match fs::read_to_string(&mod_path) {
        Ok(text) => (text, false),
        Err(_) => (templates::JOBS_MOD.to_string(), true),
    };
    let mut report = Report::default();
    let edit = match register_job(&current, &module, &ty) {
        Some(text) if !is_new && text == current => ModEdit::Unchanged,
        Some(text) => ModEdit::Write {
            path: rel.clone(),
            text,
            is_new,
        },
        None => ModEdit::Manual(format!(
            "src/jobs/mod.rs has no `{MODULES}` / `{REGISTER}` markers; register the job yourself:\n\
             \x20 pub mod {module};\n\
             \x20 // ...and on the queue: .register::<{module}::{ty}>()"
        )),
    };
    let files = files(&crate_name_of(project), &module, with_test)?;
    apply(project, files, edit, force, &mut report)?;
    // rustfmt keeps the module list sorted, as `cargo fmt` would.
    format_files(project, &[rel]);

    match wired {
        Some(text) if text != lib => {
            fs::write(&lib_path, text)
                .with_context(|| format!("cannot write {}", lib_path.display()))?;
            report.modified.push(PathBuf::from("src/lib.rs"));
        }
        Some(_) => {}
        None => report.notes.push(
            "register the queue on your app, after `.database(..)`:\n\
             \x20 pub mod jobs;   // in src/lib.rs or src/main.rs\n\
             \x20 app.jobs(jobs::register)"
                .to_string(),
        ),
    }
    report.notes.push(format!(
        "enqueue it from a handler that takes `jobs: zapline::jobs::Jobs`:\n\
         \x20 jobs.enqueue(crate::jobs::{module}::{ty} {{}}).await?;\n\
         the job runs in the background, is retried with backoff when it fails, and survives restarts"
    ));
    warn_if_not_zapline(project, &mut report);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_once_between_the_markers() {
        let once = register_job(templates::JOBS_MOD, "send_report", "SendReport").unwrap();
        assert!(once.contains("pub mod send_report;\n// zapline:jobs:modules"));
        assert!(once.contains(
            "    queue\n        .register::<send_report::SendReport>()\n    // zapline:jobs:register"
        ));
        assert_eq!(
            register_job(&once, "send_report", "SendReport").unwrap(),
            once
        );
        let twice = register_job(&once, "purge", "Purge").unwrap();
        assert!(twice.contains("pub mod send_report;\npub mod purge;\n"));
        assert_eq!(register_job("pub mod x;\n", "a", "A"), None);
    }

    #[test]
    fn wires_the_queue_into_app() {
        let lib = "pub mod routes;\n\npub fn app(db: Database) -> App {\n    App::new()\n        .database(db)\n        .views(\"templates\")\n}\n";
        let wired = wire_lib(lib).unwrap();
        assert!(wired.contains("pub mod jobs;\npub mod routes;\n"));
        assert!(wired.contains("        .database(db)\n        .jobs(jobs::register)\n"));
        assert_eq!(wire_lib(&wired).unwrap(), wired);
        assert_eq!(wire_lib("fn main() {}\n"), None);
    }
}
