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
        "#[post(\"/blog-posts\")]",
        "#[put(\"/blog-posts/{id}\")]",
        "#[delete(\"/blog-posts/{id}\")]",
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
        assert!(
            src.contains(&format!("#[{}(\"/things\")]", method.lower())),
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
