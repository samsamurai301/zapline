use std::fs;
use std::path::Path;

use super::*;
use crate::new::{create, NewOptions};
use crate::templates::Template;
use crate::test_util::{assert_rustfmt_clean, rustfmt_available};

/// Scaffolds a project of `template` in `dir` and returns its root.
fn scaffold(dir: &Path, template: Template) -> PathBuf {
    let created = create(&NewOptions {
        name: "demo-app".into(),
        template,
        parent: dir.to_path_buf(),
        zapline_path: None,
        git: false,
    })
    .unwrap();
    created.root
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn names(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| display(p)).collect()
}

#[test]
fn resource_creates_files_and_registers_the_module() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);

    let report = resource(&root, "blog post", None, false).unwrap();
    assert_eq!(
        names(&report.created),
        ["src/routes/blog_posts.rs", "tests/blog_posts.rs"]
    );
    assert_eq!(names(&report.modified), ["src/routes/mod.rs"]);
    assert!(report.notes.is_empty(), "{:?}", report.notes);

    let module = read(&root, "src/routes/blog_posts.rs");
    for needle in [
        "pub struct BlogPost {",
        "pub struct NewBlogPost {",
        "pub struct ListBlogPostsQuery {",
        "pub struct BlogPostStore(",
        "#[get(\"/blog-posts\")]",
        "#[get(\"/blog-posts/{id}\")]",
        "#[post(\"/blog-posts\", status = 201)]",
        "#[put(\"/blog-posts/{id}\")]",
        "#[delete(\"/blog-posts/{id}\", status = 204)]",
        "async fn list_blog_posts(",
        "async fn get_blog_post(",
        "async fn create_blog_post(",
        "async fn replace_blog_post(",
        "async fn delete_blog_post(",
        "pub fn mount(app: App) -> App",
    ] {
        assert!(module.contains(needle), "missing `{needle}` in:\n{module}");
    }
    assert!(!module.contains("@@"));

    let test = read(&root, "tests/blog_posts.rs");
    assert!(test.contains("use demo_app::routes::blog_posts;"), "{test}");
    assert!(test.contains("App::new().plugin(blog_posts::mount).test()"));
    assert!(test.contains("\"/blog-posts\""));

    let mod_rs = read(&root, "src/routes/mod.rs");
    assert!(
        mod_rs.contains("pub mod blog_posts;\npub mod health;\npub mod users;"),
        "{mod_rs}"
    );
    assert!(
        mod_rs.contains("    app = app.plugin(users::mount);\n    app = app.plugin(blog_posts::mount);\n    // zapline:routes:end"),
        "{mod_rs}"
    );
}

#[test]
fn resource_names_may_be_plural_or_any_case() {
    for input in ["Post", "posts", "post", "Posts"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = scaffold(tmp.path(), Template::Api);
        let report = resource(&root, input, None, false).unwrap();
        assert_eq!(
            names(&report.created),
            ["src/routes/posts.rs", "tests/posts.rs"],
            "{input}"
        );
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    let report = resource(&root, "person", Some("people"), false).unwrap();
    assert_eq!(
        names(&report.created),
        ["src/routes/people.rs", "tests/people.rs"]
    );
    assert!(read(&root, "src/routes/people.rs").contains("#[get(\"/people\")]"));
}

#[test]
fn generating_twice_requires_force_and_never_duplicates_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    resource(&root, "post", None, false).unwrap();
    let mod_after_first = read(&root, "src/routes/mod.rs");
    let custom = "// my edits\n";
    fs::write(root.join("src/routes/posts.rs"), custom).unwrap();

    let err = resource(&root, "post", None, false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "{err}");
    assert!(err.contains("--force"), "{err}");
    // A refused run changes nothing.
    assert_eq!(read(&root, "src/routes/posts.rs"), custom);
    assert_eq!(read(&root, "src/routes/mod.rs"), mod_after_first);

    let report = resource(&root, "post", None, true).unwrap();
    assert!(report.modified.is_empty(), "registration is idempotent");
    assert_ne!(read(&root, "src/routes/posts.rs"), custom);
    assert_eq!(read(&root, "src/routes/mod.rs"), mod_after_first);
    assert_eq!(mod_after_first.matches("pub mod posts;").count(), 1);
    assert_eq!(
        mod_after_first.matches("app.plugin(posts::mount)").count(),
        1
    );
}

#[test]
fn full_template_projects_work_too() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Full);
    let report = resource(&root, "order", None, false).unwrap();
    assert_eq!(names(&report.modified), ["src/routes/mod.rs"]);
    let mod_rs = read(&root, "src/routes/mod.rs");
    assert!(
        mod_rs.contains("pub mod orders;\npub mod users;"),
        "{mod_rs}"
    );
    assert!(mod_rs.contains("app = app.plugin(orders::mount);"));
}

#[test]
fn missing_markers_produce_instructions_and_leave_mod_rs_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    let plain = "pub mod health;\npub mod users;\n";
    fs::write(root.join("src/routes/mod.rs"), plain).unwrap();

    let report = resource(&root, "post", None, false).unwrap();
    assert_eq!(read(&root, "src/routes/mod.rs"), plain);
    assert!(report.modified.is_empty());
    let note = report.notes.join("\n");
    assert!(note.contains("zapline:routes:start"), "{note}");
    assert!(note.contains("pub mod posts;"), "{note}");
    assert!(note.contains("app.plugin(posts::mount);"), "{note}");
    assert!(root.join("src/routes/posts.rs").is_file());
}

#[test]
fn malformed_markers_abort_before_writing_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    fs::write(root.join("src/routes/mod.rs"), "// zapline:routes:start\n").unwrap();
    let err = resource(&root, "post", None, false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("zapline:routes:end"), "{err}");
    assert!(!root.join("src/routes/posts.rs").exists());
    assert!(!root.join("tests/posts.rs").exists());
}

#[test]
fn minimal_projects_get_a_routes_module_and_dependency_hints() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Minimal);
    assert!(!root.join("src/routes").exists());

    let report = resource(&root, "post", None, false).unwrap();
    assert_eq!(
        names(&report.created),
        ["src/routes/posts.rs", "tests/posts.rs", "src/routes/mod.rs"]
    );
    let mod_rs = read(&root, "src/routes/mod.rs");
    assert!(mod_rs.contains("pub mod posts;"), "{mod_rs}");
    assert!(
        mod_rs.contains("app = app.plugin(posts::mount);"),
        "{mod_rs}"
    );
    assert!(mod_rs.contains("// zapline:routes:start"));

    let notes = report.notes.join("\n");
    assert!(notes.contains("pub mod routes;"), "{notes}");
    assert!(notes.contains("src/lib.rs"), "{notes}");
    assert!(notes.contains("routes::install"), "{notes}");
    assert!(!notes.contains("schemars"), "{notes}");
    assert!(notes.contains("cargo add --dev serde_json"), "{notes}");
    // tokio is already a dev-dependency of the minimal template.
    assert!(!notes.contains("cargo add --dev tokio"), "{notes}");

    // A second resource extends the freshly created module.
    resource(&root, "comment", None, false).unwrap();
    let mod_rs = read(&root, "src/routes/mod.rs");
    assert!(
        mod_rs.contains("pub mod comments;\npub mod posts;"),
        "{mod_rs}"
    );
}

#[test]
fn api_projects_need_no_dependency_hints() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    let report = resource(&root, "post", None, false).unwrap();
    assert!(report.notes.is_empty(), "{:?}", report.notes);
}

#[test]
fn bin_only_projects_skip_the_test_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bin-only");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"bin-only\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

    let report = resource(&root, "post", None, false).unwrap();
    assert!(!root.join("tests/posts.rs").exists());
    assert!(root.join("src/routes/posts.rs").is_file());
    let notes = report.notes.join("\n");
    assert!(notes.contains("no src/lib.rs"), "{notes}");
    assert!(
        notes.contains("add `pub mod routes;` to src/main.rs"),
        "{notes}"
    );
    assert!(notes.contains("cargo add zapline"), "{notes}");
}

#[test]
fn file_style_routes_module_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Minimal);
    fs::write(root.join("src/routes.rs"), "// my routes\n").unwrap();
    let report = resource(&root, "post", None, false).unwrap();
    assert!(!root.join("src/routes/mod.rs").exists());
    assert!(report.notes.join("\n").contains("src/routes.rs"));
}

#[test]
fn invalid_names_and_locations_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    for bad in ["", "type", "9lives", "a/b", "../x"] {
        assert!(resource(&root, bad, None, false).is_err(), "{bad:?}");
    }
    // Not inside a project.
    let empty = tempfile::tempdir().unwrap();
    let err = resource(empty.path(), "post", None, false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("Cargo.toml"), "{err}");
}

#[test]
fn works_from_a_subdirectory_of_the_project() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    let report = resource(&root.join("src/routes"), "post", None, false).unwrap();
    assert_eq!(names(&report.created)[0], "src/routes/posts.rs");
    assert!(root.join("src/routes/posts.rs").is_file());
}

// ---- handlers -----------------------------------------------------------------------

#[test]
fn handler_defaults_to_get_and_a_kebab_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    let report = handler(&root, "send_email", HandlerMethod::Get, None, false).unwrap();
    assert_eq!(
        names(&report.created),
        ["src/routes/send_email.rs", "tests/send_email.rs"]
    );
    let src = read(&root, "src/routes/send_email.rs");
    assert!(src.contains("#[get(\"/send-email\")]"), "{src}");
    assert!(
        src.contains("async fn send_email() -> &'static str {\n    \"ok\"\n}"),
        "{src}"
    );
    assert!(src.contains("app.mount(send_email)"));
    let test = read(&root, "tests/send_email.rs");
    assert!(test.contains("use demo_app::routes::send_email;"), "{test}");
    assert!(test.contains("client.get(\"/send-email\")"), "{test}");
    assert!(test.contains("assert_eq!(res.status(), 200);"), "{test}");
    assert!(read(&root, "src/routes/mod.rs").contains("app = app.plugin(send_email::mount);"));
}

#[test]
fn handler_methods_shape_the_response() {
    for (method, ret, status) in [
        (HandlerMethod::Post, "(StatusCode, &'static str)", "201"),
        (HandlerMethod::Put, "&'static str", "200"),
        (HandlerMethod::Patch, "&'static str", "200"),
        (HandlerMethod::Delete, "StatusCode", "204"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = scaffold(tmp.path(), Template::Api);
        handler(&root, "thing", method, Some("/things"), false).unwrap();
        let src = read(&root, "src/routes/thing.rs");
        let route_status = match status {
            "200" => String::new(),
            code => format!(", status = {code}"),
        };
        assert!(
            src.contains(&format!("#[{}(\"/things\"{route_status})]", method.lower())),
            "{src}"
        );
        assert!(
            src.contains(&format!("async fn thing() -> {ret} {{")),
            "{src}"
        );
        let test = read(&root, "tests/thing.rs");
        assert!(
            test.contains(&format!("client.{}(\"/things\")", method.lower())),
            "{test}"
        );
        assert!(
            test.contains(&format!("assert_eq!(res.status(), {status});")),
            "{test}"
        );
    }
}

#[test]
fn handler_path_parameters_become_extractors() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    handler(
        &root,
        "get_item",
        HandlerMethod::Get,
        Some("/items/{id}"),
        false,
    )
    .unwrap();
    let src = read(&root, "src/routes/get_item.rs");
    assert!(
        src.contains("async fn get_item(Path(id): Path<String>) -> String {\n    id\n}"),
        "{src}"
    );
    assert!(read(&root, "tests/get_item.rs").contains("client.get(\"/items/1\")"));

    handler(
        &root,
        "move_item",
        HandlerMethod::Post,
        Some("/boards/{board}/items/{id}"),
        false,
    )
    .unwrap();
    let src = read(&root, "src/routes/move_item.rs");
    assert!(
        src.contains("Path((board, id)): Path<(String, String)>"),
        "{src}"
    );
    assert!(src.contains("-> (StatusCode, String)"), "{src}");
    assert!(
        src.contains("(StatusCode::CREATED, format!(\"{board}/{id}\"))"),
        "{src}"
    );
    assert!(read(&root, "tests/move_item.rs").contains("\"/boards/1/items/1\""));
}

#[test]
fn route_path_validation() {
    assert_eq!(route_params("/").unwrap(), Vec::<String>::new());
    assert_eq!(route_params("/a/b").unwrap(), Vec::<String>::new());
    assert_eq!(route_params("/a/").unwrap(), Vec::<String>::new());
    assert_eq!(route_params("/a/{id}/b/{other}").unwrap(), ["id", "other"]);
    assert_eq!(route_params("/files/{*rest}").unwrap(), ["rest"]);
    for bad in [
        "",
        "no-slash",
        "/a b",
        "/a\"b",
        "/a\\b",
        "/a?x=1",
        "/a#frag",
        "/a//b",
        "/{id",
        "/id}",
        "/pre{id}",
        "/{id}post",
        "/{}",
        "/{9}",
        "/{type}",
        "/{id}/{id}",
        "/{*rest}/more",
        "/{a-b}",
    ] {
        assert!(route_params(bad).is_err(), "{bad:?} should be rejected");
    }
}

#[test]
fn handler_rejects_bad_input_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    assert!(handler(&root, "ok", HandlerMethod::Get, Some("nope"), false).is_err());
    assert!(handler(&root, "type", HandlerMethod::Get, None, false).is_err());
    assert!(!root.join("src/routes/ok.rs").exists());
    let err = handler(&root, "health", HandlerMethod::Get, None, false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "{err}");
}

// ---- middleware ---------------------------------------------------------------------

#[test]
fn middleware_creates_its_module_and_a_test() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    let report = middleware(&root, "RequestAudit", false).unwrap();
    assert_eq!(
        names(&report.created),
        [
            "src/middleware/request_audit.rs",
            "tests/request_audit_middleware.rs",
            "src/middleware/mod.rs"
        ]
    );
    let src = read(&root, "src/middleware/request_audit.rs");
    assert!(
        src.contains("pub async fn request_audit(req: Request, next: Next) -> Response"),
        "{src}"
    );
    assert!(
        src.contains("HeaderName::from_static(\"x-request-audit\")"),
        "{src}"
    );
    assert!(read(&root, "src/middleware/mod.rs").contains("pub mod request_audit;"));
    let test = read(&root, "tests/request_audit_middleware.rs");
    assert!(
        test.contains("use demo_app::middleware::request_audit;"),
        "{test}"
    );
    assert!(
        test.contains(".wrap(request_audit::request_audit)"),
        "{test}"
    );

    let notes = report.notes.join("\n");
    assert!(
        notes.contains("add `pub mod middleware;` to src/lib.rs"),
        "{notes}"
    );
    assert!(
        notes.contains("app.wrap(middleware::request_audit::request_audit)"),
        "{notes}"
    );
}

#[test]
fn second_middleware_extends_mod_rs_alphabetically() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    middleware(&root, "timing", false).unwrap();
    let report = middleware(&root, "audit", false).unwrap();
    assert_eq!(names(&report.modified), ["src/middleware/mod.rs"]);
    let mod_rs = read(&root, "src/middleware/mod.rs");
    assert!(
        mod_rs.contains("pub mod audit;\npub mod timing;"),
        "{mod_rs}"
    );
}

#[test]
fn no_module_note_once_the_project_declares_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    let lib = read(&root, "src/lib.rs")
        .replace("pub mod models;", "pub mod middleware;\npub mod models;");
    fs::write(root.join("src/lib.rs"), lib).unwrap();
    let report = middleware(&root, "audit", false).unwrap();
    assert!(
        !report.notes.join("\n").contains("declare the module"),
        "{:?}",
        report.notes
    );
}

#[test]
fn middleware_refuses_to_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    middleware(&root, "audit", false).unwrap();
    assert!(middleware(&root, "audit", false).is_err());
    assert!(middleware(&root, "audit", true).is_ok());
    assert!(middleware(&root, "fn", false).is_err());
}

// ---- generated code quality ---------------------------------------------------------------

#[test]
fn generated_code_is_rustfmt_clean() {
    if !rustfmt_available() {
        eprintln!("rustfmt not installed; skipping");
        return;
    }
    for template in [Template::Minimal, Template::Api, Template::Full] {
        let tmp = tempfile::tempdir().unwrap();
        let root = scaffold(tmp.path(), template);
        resource(&root, "blog post", None, false).unwrap();
        resource(&root, "category", None, false).unwrap();
        for (name, method, path) in [
            ("ping", HandlerMethod::Get, None),
            ("make_thing", HandlerMethod::Post, Some("/things")),
            ("update_item", HandlerMethod::Put, Some("/items/{id}")),
            (
                "patch_item",
                HandlerMethod::Patch,
                Some("/items/{id}/parts/{part}"),
            ),
            ("remove_item", HandlerMethod::Delete, Some("/items/{id}")),
            ("show_file", HandlerMethod::Get, Some("/files/{*rest}")),
        ] {
            handler(&root, name, method, path, false).unwrap();
        }
        middleware(&root, "request_audit", false).unwrap();
        middleware(&root, "timing", false).unwrap();

        let mut checked = 0;
        for dir in ["src/routes", "src/middleware", "tests"] {
            for entry in fs::read_dir(root.join(dir)).unwrap().flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "rs") {
                    let label = format!(
                        "{}:{}",
                        template.name(),
                        path.strip_prefix(&root).unwrap().display()
                    );
                    assert_rustfmt_clean(&label, &fs::read_to_string(&path).unwrap());
                    checked += 1;
                }
            }
        }
        assert!(checked >= 20, "only {checked} files checked");
    }
}

/// The templates themselves (before the rustfmt pass that generation runs) are
/// kept formatted for a mid-length reference name, so output is already tidy
/// when rustfmt is not installed. Other name lengths may reflow a few lines.
#[test]
fn templates_are_rustfmt_clean_for_a_reference_name() {
    if !rustfmt_available() {
        eprintln!("rustfmt not installed; skipping");
        return;
    }
    for input in ["blog_post"] {
        let names = ResourceNames::parse(input, None).unwrap();
        for f in resource_files("demo_app", &names, true).unwrap() {
            assert_rustfmt_clean(
                &format!("resource {input}: {}", f.path.display()),
                &f.contents,
            );
        }
    }
    for (name, method, path) in [
        ("ping", HandlerMethod::Get, "/ping"),
        ("make_thing", HandlerMethod::Post, "/things"),
        ("update_item", HandlerMethod::Put, "/items/{id}"),
        (
            "patch_item",
            HandlerMethod::Patch,
            "/items/{id}/parts/{part}",
        ),
        ("remove_item", HandlerMethod::Delete, "/items/{id}"),
        ("show_file", HandlerMethod::Get, "/files/{*rest}"),
    ] {
        for f in handler_files("demo_app", name, method, path, true).unwrap() {
            assert_rustfmt_clean(
                &format!("handler {name}: {}", f.path.display()),
                &f.contents,
            );
        }
    }
    for name in ["audit", "request_timer"] {
        for f in middleware_files("demo_app", name, true).unwrap() {
            assert_rustfmt_clean(
                &format!("middleware {name}: {}", f.path.display()),
                &f.contents,
            );
        }
    }
}

/// Even very long names end up formatted, because generation runs rustfmt.
#[test]
fn generated_files_are_formatted_for_long_names_too() {
    if !rustfmt_available() {
        eprintln!("rustfmt not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    resource(&root, "international_shipping_label", None, false).unwrap();
    handler(
        &root,
        "recalculate_everything_now",
        HandlerMethod::Patch,
        Some("/a/{some_long_id}/b/{another_long_id}"),
        false,
    )
    .unwrap();
    for rel in [
        "src/routes/international_shipping_labels.rs",
        "tests/international_shipping_labels.rs",
        "src/routes/recalculate_everything_now.rs",
        "tests/recalculate_everything_now.rs",
    ] {
        assert_rustfmt_clean(rel, &read(&root, rel));
    }
}

// ---- database-backed resources and migrations ------------------------------------

/// The one migration whose name ends with `suffix`.
fn migration_text(root: &Path, suffix: &str) -> String {
    let dir = root.join("migrations");
    let name = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.ends_with(suffix))
        .unwrap_or_else(|| panic!("no migration ending in {suffix}"));
    fs::read_to_string(dir.join(name)).unwrap()
}

#[test]
fn sql_resource_writes_pages_timestamps_and_enables_chrono() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Fullstack);
    let report = resource_with_fields(
        &root,
        "event",
        None,
        &["title".into(), "day:date".into(), "token:uuid".into()],
        false,
        false,
    )
    .unwrap();
    let created = names(&report.created);
    assert!(
        created.iter().any(|p| p.ends_with("_create_events.up.sql")),
        "{created:?}"
    );
    for rel in [
        "src/routes/events.rs",
        "templates/events/index.html",
        "templates/events/_row.html",
        "templates/events/show.html",
        "templates/events/edit.html",
        "tests/events.rs",
    ] {
        assert!(created.contains(&rel.to_string()), "{rel} in {created:?}");
    }
    assert!(names(&report.modified).contains(&"Cargo.toml".to_string()));
    let manifest = read(&root, "Cargo.toml");
    assert!(
        manifest.contains("\"chrono\"") && manifest.contains("\"uuid\""),
        "{manifest}"
    );
    let sql = migration_text(&root, "_create_events.up.sql");
    assert!(sql.contains("day TEXT NOT NULL"), "{sql}");
    assert!(sql.contains("token BLOB NOT NULL"), "{sql}");
    assert!(
        sql.contains("updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"),
        "{sql}"
    );

    // Generating another resource leaves the features alone.
    let report = resource_with_fields(&root, "tag", None, &["name".into()], false, false).unwrap();
    assert!(!names(&report.modified).contains(&"Cargo.toml".to_string()));
}

#[test]
fn references_use_the_parent_label_and_note_a_missing_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Fullstack);

    // No `authors` table yet.
    let report = resource_with_fields(
        &root,
        "post",
        None,
        &["title".into(), "author:references".into()],
        false,
        false,
    )
    .unwrap();
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("no migration creates the `authors` table")),
        "{:?}",
        report.notes
    );
    let module = read(&root, "src/routes/posts.rs");
    assert!(
        module.contains("CAST(id AS TEXT) AS label FROM authors"),
        "{module}"
    );

    // With the parent generated first, forms show its name and rows link to it.
    resource_with_fields(&root, "author", None, &["name".into()], false, false).unwrap();
    let report = resource_with_fields(
        &root,
        "post",
        None,
        &["title".into(), "author:references".into()],
        false,
        true,
    )
    .unwrap();
    assert!(!report
        .notes
        .iter()
        .any(|n| n.contains("no migration creates")));
    let module = read(&root, "src/routes/posts.rs");
    assert!(module.contains("CAST(name AS TEXT) AS label FROM authors ORDER BY name"));
    assert!(module.contains("#[get(\"/api/authors/{id}/posts\", tag = \"posts\")]"));
    assert!(read(&root, "templates/posts/_row.html")
        .contains("<a href=\"/authors/{{ item.author_id }}\">"));
    // --force rewrote the same migration rather than adding a second one.
    let count = fs::read_dir(root.join("migrations"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with("_create_posts.up.sql")
        })
        .count();
    assert_eq!(count, 1);

    let err = resource_with_fields(
        &root,
        "author",
        None,
        &["author:references".into()],
        false,
        true,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("refer to itself"), "{err}");
}

#[test]
fn migration_adds_columns_to_an_existing_table() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Fullstack);
    resource_with_fields(&root, "post", None, &["title".into()], false, false).unwrap();
    resource_with_fields(&root, "category", None, &["name".into()], false, false).unwrap();

    let report = migration(
        &root,
        "add_published_at_to_posts",
        &[
            "published_at:datetime?".into(),
            "views:int".into(),
            "category:references?".into(),
        ],
    )
    .unwrap();
    assert_eq!(report.created.len(), 2);
    assert!(display(&report.created[0]).ends_with("_add_published_at_to_posts.up.sql"));
    assert!(display(&report.created[1]).ends_with("_add_published_at_to_posts.down.sql"));
    let sql = migration_text(&root, "_add_published_at_to_posts.up.sql");
    assert_eq!(
        sql,
        "-- Created by `zapline generate migration add_published_at_to_posts`.\n\
         ALTER TABLE posts ADD COLUMN published_at TEXT;\n\
         ALTER TABLE posts ADD COLUMN views BIGINT NOT NULL DEFAULT 0;\n\
         ALTER TABLE posts ADD COLUMN category_id BIGINT REFERENCES categories(id) ON DELETE SET NULL;\n\
         CREATE INDEX posts_category_id_idx ON posts (category_id);\n"
    );
    // The down file undoes it, last column first.
    assert_eq!(
        migration_text(&root, "_add_published_at_to_posts.down.sql"),
        "-- Reverts add_published_at_to_posts.\n\
         DROP INDEX posts_category_id_idx;\n\
         ALTER TABLE posts DROP COLUMN category_id;\n\
         ALTER TABLE posts DROP COLUMN views;\n\
         ALTER TABLE posts DROP COLUMN published_at;\n"
    );
    // The model doesn't change by itself: the note says what to add.
    let note = report
        .notes
        .iter()
        .find(|n| n.contains("src/routes/posts.rs"))
        .expect("a note about the model");
    assert!(
        note.contains("pub published_at: Option<DateTime<Utc>>,"),
        "{note}"
    );
    assert!(note.contains("pub category_id: Option<i64>,"), "{note}");

    // The column can come from the name alone.
    migration(&root, "add_subtitle_to_posts", &[]).unwrap();
    assert!(migration_text(&root, "_add_subtitle_to_posts.up.sql")
        .contains("ALTER TABLE posts ADD COLUMN subtitle TEXT NOT NULL DEFAULT '';"));

    // A required date has no value for existing rows.
    let err = migration(&root, "add_due_to_posts", &["due:date".into()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("`due:date?`"), "{err}");

    // An unknown table gets a note.
    let report = migration(&root, "add_x_to_nopes", &[]).unwrap();
    assert!(report
        .notes
        .iter()
        .any(|n| n.contains("no migration creates the `nopes` table")));
}

#[test]
fn migration_creates_and_drops() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Fullstack);
    let report = migration(
        &root,
        "CreateTags",
        &["name".into(), "color:string?".into()],
    )
    .unwrap();
    let sql = migration_text(&root, "_create_tags.up.sql");
    assert!(
        sql.starts_with(
            "-- Created by `zapline generate migration create_tags`.\nCREATE TABLE tags (\n    \
             id INTEGER PRIMARY KEY AUTOINCREMENT,\n    name TEXT NOT NULL,\n    color TEXT,\n"
        ),
        "{sql}"
    );
    assert!(sql.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"));
    assert!(migration_text(&root, "_create_tags.down.sql").ends_with("DROP TABLE tags;\n"));
    assert!(report.modified.is_empty());

    migration(&root, "remove_color_from_tags", &[]).unwrap();
    assert!(migration_text(&root, "_remove_color_from_tags.up.sql")
        .ends_with("ALTER TABLE tags DROP COLUMN color;\n"));
    // Rolling back brings the column back, empty.
    assert!(migration_text(&root, "_remove_color_from_tags.down.sql")
        .ends_with("ALTER TABLE tags ADD COLUMN color TEXT NOT NULL DEFAULT '';\n"));

    // Any other name: a table if there are fields, an empty file if not.
    migration(&root, "audit_log", &["event".into(), "at:datetime".into()]).unwrap();
    assert!(migration_text(&root, "_audit_log.up.sql").contains("CREATE TABLE audit_log ("));
    assert!(read(&root, "Cargo.toml").contains("\"chrono\""));
    migration(&root, "backfill_slugs", &[]).unwrap();
    assert!(migration_text(&root, "_backfill_slugs.up.sql").ends_with("-- Write your SQL here.\n"));
    assert!(migration_text(&root, "_backfill_slugs.down.sql").contains("Reverts backfill_slugs"));
}

#[test]
fn migration_follows_the_project_dialect() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Fullstack);
    let manifest = read(&root, "Cargo.toml").replace("\"sqlite\"", "\"postgres\"");
    fs::write(root.join("Cargo.toml"), manifest).unwrap();
    migration(
        &root,
        "create_events",
        &["day:date".into(), "at:datetime".into(), "token:uuid".into()],
    )
    .unwrap();
    let sql = migration_text(&root, "_create_events.up.sql");
    for col in [
        "id BIGSERIAL PRIMARY KEY",
        "day DATE NOT NULL",
        "at TIMESTAMPTZ NOT NULL",
        "token UUID NOT NULL",
        "created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP",
    ] {
        assert!(sql.contains(col), "{col} in {sql}");
    }
    let manifest = read(&root, "Cargo.toml");
    assert!(
        manifest.contains("\"uuid\"") && manifest.contains("\"chrono\""),
        "{manifest}"
    );

    let api = scaffold(&tmp.path().join("api"), Template::Api);
    let err = migration(&api, "create_x", &[]).unwrap_err().to_string();
    assert!(err.contains("no migrations/ directory"), "{err}");
}

#[test]
fn zapline_features_are_added_once_in_every_manifest_style() {
    let patch = |m: &str| add_zapline_features(m, &["chrono", "uuid"]);
    assert_eq!(
        patch("[dependencies]\nzapline = { version = \"0.1\", features = [\"sqlite\"] }\n")
            .unwrap(),
        "[dependencies]\nzapline = { version = \"0.1\", features = [\"chrono\", \"uuid\", \"sqlite\"] }\n"
    );
    assert_eq!(
        patch("zapline = { path = \"../z\" }\n").unwrap(),
        "zapline = { path = \"../z\", features = [\"chrono\", \"uuid\"] }\n"
    );
    assert_eq!(
        patch("zapline = \"0.1\"\n").unwrap(),
        "zapline = { version = \"0.1\", features = [\"chrono\", \"uuid\"] }\n"
    );
    assert_eq!(
        patch("zapline = { version = \"0.1\", features = [] }\n").unwrap(),
        "zapline = { version = \"0.1\", features = [\"chrono\", \"uuid\"] }\n"
    );
    assert_eq!(
        patch("zapline = { version = \"0.1\", features = [\"uuid\", \"chrono\"] }\n"),
        None
    );
    assert_eq!(patch("[dependencies]\nserde = \"1\"\n"), None);
}

#[test]
fn generated_sql_resources_are_rustfmt_clean() {
    if !rustfmt_available() {
        eprintln!("rustfmt not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Fullstack);
    resource_with_fields(&root, "author", None, &["name".into()], false, false).unwrap();
    resource_with_fields(
        &root,
        "post",
        None,
        &[
            "title".into(),
            "body:text".into(),
            "published_on:date".into(),
            "starts_at:datetime?".into(),
            "token:uuid?".into(),
            "author:references".into(),
        ],
        false,
        false,
    )
    .unwrap();
    for rel in ["src/routes/posts.rs", "tests/posts.rs"] {
        assert_rustfmt_clean(rel, &read(&root, rel));
    }
}

// ---- mailer ---------------------------------------------------------------------------------

#[test]
fn mailer_writes_the_email_its_templates_and_a_test() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Fullstack);
    let project = Project::discover(&root).unwrap();
    let report = mailer::generate(&project, "order-shipped", false).unwrap();
    assert_eq!(
        names(&report.created),
        [
            "src/mail/order_shipped.rs",
            "templates/mail/order_shipped.html",
            "templates/mail/order_shipped.txt",
            "tests/order_shipped_email.rs",
            "src/mail/mod.rs"
        ]
    );
    assert_eq!(names(&report.modified), ["src/lib.rs", "Cargo.toml"]);
    let lib = read(&root, "src/lib.rs");
    assert!(lib.contains("pub mod mail;\npub mod routes;\n"), "{lib}");
    let manifest = read(&root, "Cargo.toml");
    assert!(
        manifest.contains("\"mail\", \"sqlite\", \"views\""),
        "{manifest}"
    );
    // The fullstack app registers views but no mailer: the note says how.
    let notes = report.notes.join("\n");
    assert!(
        notes.contains(".mailer(Mailer::from_env().expect("),
        "{notes}"
    );
    assert!(!notes.contains(".views(\"templates\")"), "{notes}");

    // A second email joins the module; regenerating needs --force and
    // never declares anything twice.
    let report = mailer::generate(&project, "welcome", false).unwrap();
    assert_eq!(names(&report.modified), ["src/mail/mod.rs"]);
    let mod_rs = read(&root, "src/mail/mod.rs");
    assert!(
        mod_rs.contains("pub mod order_shipped;\npub mod welcome;\n"),
        "{mod_rs}"
    );
    assert!(mailer::generate(&project, "welcome", false).is_err());
    mailer::generate(&project, "welcome", true).unwrap();
    assert_eq!(
        read(&root, "src/mail/mod.rs")
            .matches("pub mod welcome;")
            .count(),
        1
    );
    assert_eq!(
        read(&root, "src/lib.rs").matches("pub mod mail;").count(),
        1
    );
    assert!(mailer::generate(&project, "fn", false).is_err());

    if rustfmt_available() {
        for rel in [
            "src/mail/mod.rs",
            "src/mail/order_shipped.rs",
            "tests/order_shipped_email.rs",
            "src/lib.rs",
        ] {
            assert_rustfmt_clean(rel, &read(&root, rel));
        }
    }
}

#[test]
fn mailer_in_an_api_project_adds_both_features_and_notes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = scaffold(tmp.path(), Template::Api);
    let project = Project::discover(&root).unwrap();
    let report = mailer::generate(&project, "welcome", false).unwrap();
    let manifest = read(&root, "Cargo.toml");
    assert!(
        manifest.contains("zapline = { version = \"0.1\", features = [\"mail\", \"views\"] }"),
        "{manifest}"
    );
    let notes = report.notes.join("\n");
    assert!(notes.contains(".views(\"templates\")"), "{notes}");
    assert!(read(&root, "src/lib.rs").contains("pub mod mail;"));
}
