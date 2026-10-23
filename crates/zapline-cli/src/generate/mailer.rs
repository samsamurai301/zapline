//! `zapline generate mailer <name>`: an email in `src/mail/<name>.rs`, its
//! templates in `templates/mail/<name>.html` and `.txt`, declared in
//! `src/mail/mod.rs` and tested against the outbox mailer.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::register::declare_in_module_file;
use super::{
    add_zapline_features, apply, crate_name_of, format_files, no_lib_note, warn_if_not_zapline,
    ModEdit, Report,
};
use crate::naming::{snake_case, validate_identifier};
use crate::project::Project;
use crate::render::{render, OutputFile};
use crate::templates;

/// The default subject for an email named `module`: `password_reset` ->
/// `Password reset`.
pub fn title(module: &str) -> String {
    let words = module.replace('_', " ");
    let mut chars = words.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// What `generate mailer` writes, before touching the disk.
pub fn files(krate: &str, module: &str, with_test: bool) -> Result<Vec<OutputFile>> {
    let title = title(module);
    let vars = [
        ("name", module),
        ("Title", title.as_str()),
        ("crate_name", krate),
    ];
    let mut files = vec![
        OutputFile {
            path: PathBuf::from(format!("src/mail/{module}.rs")),
            contents: render(templates::MAILER, &vars)?,
        },
        OutputFile {
            path: PathBuf::from(format!("templates/mail/{module}.html")),
            contents: render(templates::MAILER_HTML, &vars)?,
        },
        OutputFile {
            path: PathBuf::from(format!("templates/mail/{module}.txt")),
            contents: render(templates::MAILER_TEXT, &vars)?,
        },
    ];
    if with_test {
        files.push(OutputFile {
            path: PathBuf::from(format!("tests/{module}_email.rs")),
            contents: render(templates::MAILER_TEST, &vars)?,
        });
    }
    Ok(files)
}

/// `zapline generate mailer <name>`.
pub fn generate(project: &Project, name: &str, force: bool) -> Result<Report> {
    let module = snake_case(name);
    validate_identifier(&module, name)?;
    let with_test = project.lib_name.is_some();

    let rel = PathBuf::from("src/mail/mod.rs");
    let mod_path = project.root.join(&rel);
    let (current, is_new) = match fs::read_to_string(&mod_path) {
        Ok(text) => (text, false),
        Err(_) => (templates::MAIL_MOD.to_string(), true),
    };
    let (text, changed) = declare_in_module_file(&current, &module);
    let edit = if changed || is_new {
        ModEdit::Write {
            path: rel.clone(),
            text,
            is_new,
        }
    } else {
        ModEdit::Unchanged
    };
    let mut report = Report::default();
    let files = files(&crate_name_of(project), &module, with_test)?;
    apply(project, files, edit, force, &mut report)?;
    format_files(project, &[rel]);

    // `pub mod mail;` in the crate root.
    let root_rel = if project.lib_name.is_some() {
        PathBuf::from("src/lib.rs")
    } else {
        PathBuf::from("src/main.rs")
    };
    let root_path = project.root.join(&root_rel);
    let root = fs::read_to_string(&root_path).unwrap_or_default();
    let (declared, changed) = declare_in_module_file(&root, "mail");
    if changed && !root.is_empty() {
        fs::write(&root_path, declared)
            .with_context(|| format!("cannot write {}", root_path.display()))?;
        report.modified.push(root_rel);
    }

    let manifest_path = project.root.join("Cargo.toml");
    if let Ok(manifest) = fs::read_to_string(&manifest_path) {
        if let Some(patched) = add_zapline_features(&manifest, &["mail", "views"]) {
            fs::write(&manifest_path, patched)
                .with_context(|| format!("cannot write {}", manifest_path.display()))?;
            report.modified.push(PathBuf::from("Cargo.toml"));
        }
    }

    if !with_test {
        report.notes.push(no_lib_note("email"));
    }
    let mut setup = Vec::new();
    if !root.contains(".views(") {
        setup.push("  .views(\"templates\")");
    }
    if !root.contains(".mailer(") && !generated_auth(project) {
        setup.push(
            "  .mailer(Mailer::from_env().expect(\"SMTP_URL\"))   // use zapline::mail::Mailer;",
        );
    }
    if !setup.is_empty() {
        report.notes.push(format!(
            "register on your app what the email needs:\n{}",
            setup.join("\n")
        ));
    }
    report.notes.push(format!(
        "send it from a handler that takes `views: Views, mail: Mailer`:\n\
         \x20 mail.send(crate::mail::{module}::{module}(&views, &user.email)?).await?;\n\
         while developing it shows up at /_dev/mail; in production set SMTP_URL and MAIL_FROM"
    ));
    warn_if_not_zapline(project, &mut report);
    Ok(report)
}

/// `zapline g auth` registers a mailer in its own plugin.
fn generated_auth(project: &Project) -> bool {
    fs::read_to_string(project.root.join("src/routes/auth.rs"))
        .is_ok_and(|auth| auth.contains(".mailer("))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_read_like_subjects() {
        assert_eq!(title("welcome"), "Welcome");
        assert_eq!(title("password_reset"), "Password reset");
    }

    #[test]
    fn files_and_their_contents() {
        let all = files("my_app", "order_shipped", true).unwrap();
        let paths: Vec<_> = all
            .iter()
            .map(|f| f.path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            paths,
            [
                "src/mail/order_shipped.rs",
                "templates/mail/order_shipped.html",
                "templates/mail/order_shipped.txt",
                "tests/order_shipped_email.rs"
            ]
        );
        assert!(all[0]
            .contents
            .contains("pub fn order_shipped(views: &Views, to: &str) -> Result<Email>"));
        assert!(all[0].contents.contains("\"mail/order_shipped\""));
        assert!(all[1]
            .contents
            .contains("<title>{% block subject %}Order shipped{% endblock %}</title>"));
        assert!(all[2].contents.contains("Hello {{ to }},"));
        assert!(all[3]
            .contents
            .contains("use my_app::mail::order_shipped::order_shipped;"));
        assert!(all[3]
            .contents
            .contains("assert_eq!(sent[0].subject, \"Order shipped\");"));
        assert!(all.iter().all(|f| !f.contents.contains("@@")));
        assert_eq!(files("my_app", "x", false).unwrap().len(), 3);
    }
}
