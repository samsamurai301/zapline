//! `zapline generate auth`: accounts with sign-up, log-in, log-out,
//! password reset by email, Argon2id passwords, database-backed sessions
//! and a `CurrentUser` extractor, plus htmx pages when the project has
//! templates.

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
    // Which kind of section we are in: `// @@html` lines are kept for
    // projects with pages, `// @@api` lines for those without.
    let mut inside: Option<bool> = None;
    for line in template.split_inclusive('\n') {
        match line.trim() {
            "// @@html" => inside = Some(true),
            "// @@api" => inside = Some(false),
            "// html@@" | "// api@@" => inside = None,
            _ if inside.is_some_and(|for_html| for_html != html) => {}
            _ => out.push_str(line),
        }
    }
    out
}

/// Keeps (`keep`) or drops the `// @@<name>` ... `// <name>@@` sections of
/// a template.
pub fn named_sections(template: &str, name: &str, keep: bool) -> String {
    let (open, close) = (format!("// @@{name}"), format!("// {name}@@"));
    let mut out = String::with_capacity(template.len());
    let mut inside = false;
    for line in template.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed == open {
            inside = true;
        } else if trimmed == close {
            inside = false;
        } else if keep || !inside {
            out.push_str(line);
        }
    }
    out
}

/// Whether the app in `lib` (its `src/lib.rs`) already sets up sessions:
/// the fullstack template does, so its flash messages work before auth.
pub fn has_sessions(lib: &str) -> bool {
    lib.contains(".database_sessions(") || lib.contains("SessionLayer")
}

/// What `generate auth` writes, before touching the disk. `own_sessions`:
/// the auth module sets up the cookie key and sessions itself (the app
/// doesn't yet).
pub fn files(
    package: &str,
    krate: &str,
    dialect: Dialect,
    html: bool,
    with_test: bool,
    own_sessions: bool,
    migration: &str,
) -> Result<Vec<OutputFile>> {
    let vars = [("crate_name", krate), ("package", package)];
    let module = named_sections(
        &html_sections(templates::AUTH, html),
        "own_sessions",
        own_sessions,
    );
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
            contents: render(&module, &vars)?,
        },
    ];
    if html {
        for (name, tmpl) in [
            ("signup", templates::AUTH_SIGNUP_PAGE),
            ("login", templates::AUTH_LOGIN_PAGE),
            ("forgot", templates::AUTH_FORGOT_PAGE),
            ("reset", templates::AUTH_RESET_PAGE),
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
    let mut out = super::add_zapline_features(manifest, &["cookies", "mail", "oauth", "password"])
        .unwrap_or_else(|| manifest.to_string());
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

/// The variables auth reads, appended to `.env.example` when missing.
const ENV_VARS: [(&str, &str); 7] = [
    (
        "SESSION_SECRET",
        "# Signs session cookies; required in release builds. `openssl rand -base64 48`\nSESSION_SECRET=\n",
    ),
    (
        "SMTP_URL",
        "# Sends email (password resets). Unset in debug builds: mail shows up at /_dev/mail.\n\
         # smtps://user:pass@smtp.example.com, or smtp://localhost:1025 for Mailpit\nSMTP_URL=\n",
    ),
    ("MAIL_FROM", "MAIL_FROM=no-reply@example.com\n"),
    (
        "APP_URL",
        "# The public address, for links in emails.\nAPP_URL=http://localhost:8080\n",
    ),
    (
        "GITHUB_CLIENT_ID",
        "# \"Continue with GitHub\": an OAuth app with callback {APP_URL}/auth/github/callback\n\
         GITHUB_CLIENT_ID=\nGITHUB_CLIENT_SECRET=\n",
    ),
    (
        "GOOGLE_CLIENT_ID",
        "# \"Continue with Google\": an OAuth client with redirect URI {APP_URL}/auth/google/callback\n\
         GOOGLE_CLIENT_ID=\nGOOGLE_CLIENT_SECRET=\n",
    ),
    (
        "ADMIN_EMAILS",
        "# Accounts allowed into the admin panel at /admin, comma-separated. Each\n\
         # owner proves the address once: \"Forgot your password?\" or GitHub/Google login.\n\
         ADMIN_EMAILS=\n",
    ),
];

/// `.env.example` with the auth variables it lacks, or `None`.
pub fn patch_env_example(env: &str) -> Option<String> {
    let mut out = env.to_string();
    for (name, block) in ENV_VARS {
        if !out
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("{name}=")))
        {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(block);
        }
    }
    (out != env).then_some(out)
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
    let dialect = super::dialect_of(&manifest);
    let html = project.root.join("templates/base.html").is_file();
    let lib = fs::read_to_string(project.root.join("src/lib.rs")).unwrap_or_default();
    // Generated tests use an in-memory SQLite database.
    let with_test = project.lib_name.is_some()
        && lib.contains("pub fn app(db: Database)")
        && dialect == Dialect::Sqlite;
    // Regenerating (--force) rewrites the existing migration instead of adding a second one.
    let migration = match existing_migration(project, "_create_users.sql") {
        Some(name) => name,
        None => crate::db::new_migration_name(&project.root, "create_users")?,
    };
    let krate = crate_name_of(project);
    let files = files(
        &project.package_name,
        &krate,
        dialect,
        html,
        with_test,
        !has_sessions(&lib),
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
        if let Some(patched) = patch_env_example(&env) {
            fs::write(&env_example, patched)?;
            report.modified.push(PathBuf::from(".env.example"));
        }
    }
    report.notes.push(format!(
        "accounts are ready: {}POST /api/auth/signup, /api/auth/login, /api/auth/logout and GET /api/auth/me\n\
         protect a handler by adding a `CurrentUser` argument (use crate::routes::auth::CurrentUser)\n\
         password reset: POST /api/auth/forgot and /api/auth/reset{}; while developing, reset emails show up at /_dev/mail\n\
         production needs SESSION_SECRET (32+ random bytes, `openssl rand -base64 48`), SMTP_URL, MAIL_FROM and APP_URL",
        if html { "pages at /signup and /login; " } else { "" },
        if html { " (pages at /forgot and /reset)" } else { "" }
    ));
    warn_if_not_zapline(project, &mut report);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_sections_are_kept_or_dropped() {
        let t = "a\n// @@html\nb\n// html@@\nc\n// @@api\nd\n// api@@\n";
        assert_eq!(html_sections(t, true), "a\nb\nc\n");
        assert_eq!(html_sections(t, false), "a\nc\nd\n");
    }

    #[test]
    fn manifest_gets_features_and_a_fast_argon2() {
        let m =
            "[dependencies]\nzapline = { version = \"0.1\", features = [\"sqlite\", \"views\"] }\n";
        let p = patch_manifest(m).unwrap();
        assert!(
            p.contains("features = [\"cookies\", \"mail\", \"oauth\", \"password\", \"sqlite\", \"views\"]"),
            "{p}"
        );
        assert!(p.contains("[profile.dev.package.argon2]\nopt-level = 3"));
        assert_eq!(patch_manifest(&p), None);
        let bare = patch_manifest("[dependencies]\nzapline = { version = \"0.1\" }\n").unwrap();
        assert!(
            bare.contains(
                "zapline = { version = \"0.1\", features = [\"cookies\", \"mail\", \"oauth\", \"password\"] }"
            ),
            "{bare}"
        );
    }

    #[test]
    fn env_example_gets_the_missing_variables_once() {
        let env = "ADDR=0.0.0.0:8080\nMAIL_FROM=me@x.test";
        let patched = patch_env_example(env).unwrap();
        assert!(patched.starts_with("ADDR=0.0.0.0:8080\nMAIL_FROM=me@x.test\n# Signs"));
        assert_eq!(patched.matches("MAIL_FROM=").count(), 1);
        for var in [
            "SESSION_SECRET=",
            "SMTP_URL=",
            "APP_URL=http://localhost:8080",
        ] {
            assert!(patched.contains(var), "{var}");
        }
        assert_eq!(patch_env_example(&patched), None);
    }

    #[test]
    fn files_for_each_dialect_and_layout() {
        let all = files(
            "my-app",
            "my_app",
            Dialect::Sqlite,
            true,
            true,
            false,
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
                "templates/auth/forgot.html",
                "templates/auth/reset.html",
                "tests/auth.rs"
            ]
        );
        assert!(all[0].contents.contains("AUTOINCREMENT"));
        assert!(all[1].contents.contains("signup_form"));
        assert!(all[6].contents.contains("use my_app::app;"));
        assert!(all[0].contents.contains("CREATE TABLE password_resets"));
        // The fullstack app already has sessions: auth doesn't add more.
        assert!(!all[1].contents.contains("database_sessions"));
        assert!(!all[1].contents.contains("key_from_env"));
        assert!(all[1].contents.contains("ValidForm<Credentials>"));

        let api = files(
            "my-app",
            "my_app",
            Dialect::Postgres,
            false,
            false,
            true,
            "1_create_users.sql",
        )
        .unwrap();
        assert_eq!(api.len(), 2);
        assert!(api[0].contents.contains("BIGSERIAL"));
        assert!(!api[1].contents.contains("Views"));
        assert!(!api[1].contents.contains("Flash"));
        assert!(api[1].contents.contains(".database_sessions("));
        assert!(api[1].contents.contains(".cookie_key(key)"));
        assert!(!api[1].contents.contains("@@"));
    }

    #[test]
    fn named_sections_and_session_detection() {
        let t = "a\n// @@own_sessions\nb\n// own_sessions@@\nc\n";
        assert_eq!(named_sections(t, "own_sessions", true), "a\nb\nc\n");
        assert_eq!(named_sections(t, "own_sessions", false), "a\nc\n");
        assert!(has_sessions(
            "App::new().database_sessions(|l| l.secure(true))"
        ));
        assert!(has_sessions(".wrap(SessionLayer::new(MemoryStore::new()))"));
        assert!(!has_sessions("App::new().database(db)"));
    }
}
