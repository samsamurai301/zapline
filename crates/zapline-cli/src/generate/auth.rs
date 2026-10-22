//! `zapline generate auth`: accounts with sign-up, log-in, log-out,
//! Argon2id passwords, database-backed sessions and a `CurrentUser`
//! extractor, plus htmx pages when the project has templates.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Result};

use super::db_resource::Dialect;
use super::{
    apply, crate_name_of, existing_migration, plan_route_registration, warn_if_not_zapline, Report,
};
use crate::project::Project;
use crate::render::{render, OutputFile};
use crate::templates;

/// Keeps or drops the `// @@html` ... `// html@@` sections of a template.
pub fn html_sections(template: &str, html: bool) -> String {
    let mut out = String::with_capacity(template.len());
    let mut inside = false;
    for line in template.split_inclusive('\n') {
        match line.trim() {
            "// @@html" => inside = true,
            "// html@@" => inside = false,
            _ if inside && !html => {}
            _ => out.push_str(line),
        }
    }
    out
}

/// What `generate auth` writes, before touching the disk.
pub fn files(
    package: &str,
    krate: &str,
    dialect: Dialect,
    html: bool,
    with_test: bool,
    migration: &str,
) -> Result<Vec<OutputFile>> {
    let vars = [("crate_name", krate), ("package", package)];
    let mut files = vec![
        OutputFile {
            path: PathBuf::from(format!("migrations/{migration}")),
            contents: match dialect {
                Dialect::Sqlite => templates::AUTH_MIGRATION_SQLITE,
                Dialect::Postgres => templates::AUTH_MIGRATION_POSTGRES,
            }
            .to_string(),
        },
        OutputFile {
            path: PathBuf::from("src/routes/auth.rs"),
            contents: render(&html_sections(templates::AUTH, html), &vars)?,
        },
    ];
    if html {
        for (name, tmpl) in [
            ("signup", templates::AUTH_SIGNUP_PAGE),
            ("login", templates::AUTH_LOGIN_PAGE),
        ] {
            files.push(OutputFile {
                path: PathBuf::from(format!("templates/auth/{name}.html")),
                contents: render(tmpl, &vars)?,
            });
        }
    }
    if with_test {
        files.push(OutputFile {
            path: PathBuf::from("tests/auth.rs"),
            contents: render(&html_sections(templates::AUTH_TEST, html), &vars)?,
        });
    }
    Ok(files)
}

/// Adds the zapline features auth needs and compiles Argon2 optimized in
/// debug builds (unoptimized, one hash takes most of a second). Returns the
/// new manifest, or `None` if it already had everything.
pub fn patch_manifest(manifest: &str) -> Option<String> {
    let mut out = manifest.to_string();
    // The `zapline = { ... features = [..] }` line.
    let line_start = out
        .lines()
        .find(|l| l.trim_start().starts_with("zapline ") || l.trim_start().starts_with("zapline="))
        .map(str::to_string);
    if let Some(line) = line_start {
        let missing: Vec<String> = ["cookies", "password"]
            .iter()
            .filter(|f| !line.contains(&format!("\"{f}\"")))
            .map(|f| format!("\"{f}\""))
            .collect();
        let mut new_line = line.clone();
        let added = missing.join(", ");
        if missing.is_empty() {
            // Nothing to add.
        } else if let Some(pos) = new_line.find("features = [") {
            let at = pos + "features = [".len();
            let sep = if new_line[at..].trim_start().starts_with(']') {
                ""
            } else {
                ", "
            };
            new_line.insert_str(at, &format!("{added}{sep}"));
        } else if let Some(pos) = new_line.rfind('}') {
            let before = new_line[..pos].trim_end().len();
            new_line.replace_range(before..pos, &format!(", features = [{added}] "));
        }
        out = out.replacen(&line, &new_line, 1);
    }
    if !out.contains("[profile.dev.package.argon2]") {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(
            "\n# Password hashing is deliberately slow; unoptimized it takes most of a second.\n\
             [profile.dev.package.argon2]\nopt-level = 3\n",
        );
    }
    (out != manifest).then_some(out)
}

/// `zapline generate auth`.
pub fn generate(project: &Project, force: bool) -> Result<Report> {
    if !project.root.join(crate::db::MIGRATIONS_DIR).is_dir() {
        bail!(
            "auth needs a database: this project has no migrations/ directory\n\
             hint: start from `zapline new <name> --template fullstack`, or add one with \
             `zapline db new init` and register `App::database(..)`"
        );
    }
    let manifest_path = project.root.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)?;
    let dialect = if manifest.contains("\"postgres\"") && !manifest.contains("\"sqlite\"") {
        Dialect::Postgres
    } else {
        Dialect::Sqlite
    };
    let html = project.root.join("templates/base.html").is_file();
    let lib = fs::read_to_string(project.root.join("src/lib.rs")).unwrap_or_default();
    // Generated tests use an in-memory SQLite database.
    let with_test = project.lib_name.is_some()
        && lib.contains("pub fn app(db: Database)")
        && dialect == Dialect::Sqlite;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Regenerating (--force) rewrites the existing migration instead of adding a second one.
    let migration = match existing_migration(project, "_create_users.sql") {
        Some(name) => name,
        None => crate::db::migration_file_name("create_users", now)?,
    };
    let krate = crate_name_of(project);
    let files = files(
        &project.package_name,
        &krate,
        dialect,
        html,
        with_test,
        &migration,
    )?;

    let mut report = Report::default();
    let edit = plan_route_registration(project, "auth")?;
    apply(project, files, edit, force, &mut report)?;
    if let Some(patched) = patch_manifest(&manifest) {
        fs::write(&manifest_path, patched)?;
        report.modified.push(PathBuf::from("Cargo.toml"));
    }
    let env_example = project.root.join(".env.example");
    if let Ok(env) = fs::read_to_string(&env_example) {
        if !env.contains("SESSION_SECRET") {
            let sep = if env.ends_with('\n') || env.is_empty() {
                ""
            } else {
                "\n"
            };
            fs::write(
                &env_example,
                format!(
                    "{env}{sep}# Signs session cookies; required in release builds. `openssl rand -base64 48`\n\
                     SESSION_SECRET=\n"
                ),
            )?;
            report.modified.push(PathBuf::from(".env.example"));
        }
    }
    report.notes.push(format!(
        "accounts are ready: {}POST /api/auth/signup, /api/auth/login, /api/auth/logout and GET /api/auth/me\n\
         protect a handler by adding a `CurrentUser` argument (use crate::routes::auth::CurrentUser)\n\
         production needs SESSION_SECRET set to a random string of 32+ bytes (`openssl rand -base64 48`)",
        if html { "pages at /signup and /login; " } else { "" }
    ));
    warn_if_not_zapline(project, &mut report);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_sections_are_kept_or_dropped() {
        let t = "a\n// @@html\nb\n// html@@\nc\n";
        assert_eq!(html_sections(t, true), "a\nb\nc\n");
        assert_eq!(html_sections(t, false), "a\nc\n");
    }

    #[test]
    fn manifest_gets_features_and_a_fast_argon2() {
        let m =
            "[dependencies]\nzapline = { version = \"0.1\", features = [\"sqlite\", \"views\"] }\n";
        let p = patch_manifest(m).unwrap();
        assert!(
            p.contains("features = [\"cookies\", \"password\", \"sqlite\", \"views\"]"),
            "{p}"
        );
        assert!(p.contains("[profile.dev.package.argon2]\nopt-level = 3"));
        assert_eq!(patch_manifest(&p), None);
        let bare = patch_manifest("[dependencies]\nzapline = { version = \"0.1\" }\n").unwrap();
        assert!(
            bare.contains(
                "zapline = { version = \"0.1\", features = [\"cookies\", \"password\"] }"
            ),
            "{bare}"
        );
    }

    #[test]
    fn files_for_each_dialect_and_layout() {
        let all = files(
            "my-app",
            "my_app",
            Dialect::Sqlite,
            true,
            true,
            "1_create_users.sql",
        )
        .unwrap();
        let paths: Vec<_> = all
            .iter()
            .map(|f| f.path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            paths,
            [
                "migrations/1_create_users.sql",
                "src/routes/auth.rs",
                "templates/auth/signup.html",
                "templates/auth/login.html",
                "tests/auth.rs"
            ]
        );
        assert!(all[0].contents.contains("AUTOINCREMENT"));
        assert!(all[1].contents.contains("signup_form"));
        assert!(all[4].contents.contains("use my_app::app;"));

        let api = files(
            "my-app",
            "my_app",
            Dialect::Postgres,
            false,
            false,
            "1_create_users.sql",
        )
        .unwrap();
        assert_eq!(api.len(), 2);
        assert!(api[0].contents.contains("BIGSERIAL"));
        assert!(!api[1].contents.contains("Views"));
        assert!(!api[1].contents.contains("@@"));
    }
}
