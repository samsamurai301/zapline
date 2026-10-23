//! `zapline::view`: templates, blocks for htmx, `HxRequest`, `Hx` headers,
//! hot reload, form round trips (`ValidForm`, `render_invalid`) and `flash`.
#![cfg(feature = "views")]

use zapline::prelude::*;
use zapline::view::{context, Hx, HxRequest, Views};

const TEMPLATES: &str = "tests/fixtures/templates";

async fn todos(views: Views, hx: HxRequest) -> Result<Html<String>> {
    let todos = ["a", "<b>"];
    views.page_or_block(&hx, "todos.html", "list", context! { todos })
}

fn app() -> App {
    App::new()
        .views(TEMPLATES)
        .get("/todos", todos)
        .post("/todos", || async {
            Hx::trigger("todo-added").with(StatusCode::CREATED)
        })
        .get(
            "/missing",
            |v: Views| async move { v.render("nope.html", ()) },
        )
}

#[tokio::test]
async fn full_page_extends_the_layout_and_escapes() {
    let res = app().test().get("/todos").send().await;
    assert_eq!(res.status(), 200);
    let html = res.text();
    assert!(
        html.starts_with("<!doctype html><title>Todos</title>"),
        "{html}"
    );
    assert!(html.contains("<li>a</li><li>&lt;b&gt;</li>"), "{html}");
}

#[tokio::test]
async fn htmx_requests_get_only_the_block() {
    let client = app().test();
    let res = client
        .get("/todos")
        .header("hx-request", "true")
        .send()
        .await;
    assert_eq!(res.text(), "<li>a</li><li>&lt;b&gt;</li>");

    // Boosted navigation and history restores need the whole page.
    let res = client
        .get("/todos")
        .header("hx-request", "true")
        .header("hx-boosted", "true")
        .send()
        .await;
    assert!(res.text().starts_with("<!doctype html>"));
}

#[tokio::test]
async fn hx_response_headers() {
    let res = app().test().post("/todos").send().await;
    assert_eq!(res.status(), 201);
    assert_eq!(res.header("hx-trigger"), Some("todo-added"));
}

#[tokio::test]
async fn missing_template_is_a_500_naming_it() {
    // The details go to the log (and to the client only under ZAPLINE_DEV=1).
    let res = app().test().get("/missing").send().await;
    assert_eq!(res.status(), 500);
    assert_eq!(res.text(), r#"{"error":"internal server error"}"#);
}

#[tokio::test]
async fn templates_reload_without_a_rebuild_in_debug() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("page.html");
    std::fs::write(&file, "v1").unwrap();
    let client = App::new()
        .state(Views::new(dir.path(), true))
        .get("/", |v: Views| async move { v.render("page.html", ()) })
        .test();
    assert_eq!(client.get("/").send().await.text(), "v1");
    std::fs::write(&file, "v2").unwrap();
    assert_eq!(client.get("/").send().await.text(), "v2");
}

// ---- forms: ValidForm + render_invalid -----------------------------------------

#[cfg(all(feature = "form", feature = "validate"))]
mod forms {
    use serde::{Deserialize, Serialize};
    use validator::Validate;
    use zapline::prelude::*;
    use zapline::view::{context, FormErrors, Invalid, ValidForm, Views};

    use super::TEMPLATES;

    #[derive(Debug, Deserialize, Serialize, Validate)]
    pub struct SignUp {
        #[validate(email)]
        email: String,
        #[validate(range(min = 13, message = "you must be 13 or older"))]
        age: u32,
        #[validate(length(min = 8))]
        password: String,
    }

    async fn signup(views: Views, ValidForm(form): ValidForm<SignUp>) -> Result<Response> {
        let form = match form {
            Ok(form) => form,
            Err(invalid) => {
                return views.render_invalid("signup.html", context! { heading => "Join" }, invalid)
            }
        };
        if form.email == "taken@example.com" {
            let errors = FormErrors::one("email", "is already registered");
            return views.render_invalid("signup.html", context! {}, Invalid::new(errors, &form));
        }
        Ok(Redirect::to("/").into_response())
    }

    fn app() -> App {
        App::new().views(TEMPLATES).post("/signup", signup)
    }

    #[tokio::test]
    async fn invalid_post_is_a_422_page_with_messages_and_old_values_but_no_password() {
        let res = app()
            .test()
            .post("/signup")
            .form(&[
                ("email", "not-an-email"),
                ("age", "30"),
                ("password", "hunter2"),
            ])
            .send()
            .await;
        assert_eq!(res.status(), 422);
        assert_eq!(res.header("content-type"), Some("text/html; charset=utf-8"));
        let html = res.text();
        // The full page, with the rest of the context.
        assert!(html.starts_with("<!doctype html>"), "{html}");
        assert!(html.contains("<h1>Join</h1>"), "{html}");
        assert!(html.contains(r#"value="not-an-email""#), "{html}");
        assert!(
            html.contains("<small>must be a valid email address</small>"),
            "{html}"
        );
        // The password is checked but never echoed back.
        assert!(
            html.contains("<small>length must be at least 8</small>"),
            "{html}"
        );
        assert!(!html.contains("hunter2"), "{html}");
        assert!(html.contains(r#"name="password" value="""#), "{html}");
    }

    #[tokio::test]
    async fn unparsable_fields_get_messages_and_their_values_back_escaped() {
        let res = app()
            .test()
            .post("/signup")
            .form(&[
                ("email", "not-an-email"),
                ("age", "<b>12</b>"),
                ("password", "hunter2"),
            ])
            .send()
            .await;
        assert_eq!(res.status(), 422);
        let html = res.text();
        assert!(html.contains("<small>must be a number</small>"), "{html}");
        assert!(
            html.contains(r#"value="&lt;b&gt;12&lt;&#x2f;b&gt;""#),
            "{html}"
        );
        assert!(html.contains(r#"value="not-an-email""#), "{html}");
        assert!(!html.contains("hunter2"), "{html}");
    }

    #[tokio::test]
    async fn validation_rules_run_once_the_form_parses() {
        let res = app()
            .test()
            .post("/signup")
            .form(&[
                ("email", "ada@example.com"),
                ("age", "9"),
                ("password", "longenough"),
            ])
            .send()
            .await;
        assert_eq!(res.status(), 422);
        let html = res.text();
        assert!(
            html.contains("<small>you must be 13 or older</small>"),
            "{html}"
        );
        assert!(!html.contains("longenough"), "{html}");

        let res = app()
            .test()
            .post("/signup")
            .form(&[("age", "30"), ("password", "longenough")])
            .send()
            .await;
        assert!(res.text().contains("<small>is required</small>"));
    }

    #[tokio::test]
    async fn handler_checks_render_the_same_way() {
        let res = app()
            .test()
            .post("/signup")
            .form(&[
                ("email", "taken@example.com"),
                ("age", "30"),
                ("password", "longenough"),
            ])
            .send()
            .await;
        assert_eq!(res.status(), 422);
        let html = res.text();
        assert!(
            html.contains("<small>is already registered</small>"),
            "{html}"
        );
        assert!(html.contains(r#"value="taken@example.com""#), "{html}");
        assert!(html.contains(r#"name="age" value="30""#), "{html}");
        assert!(!html.contains("longenough"), "{html}");
    }

    #[tokio::test]
    async fn valid_post_reaches_the_handler() {
        let res = app()
            .test()
            .post("/signup")
            .form(&[
                ("email", "ada@example.com"),
                ("age", "30"),
                ("password", "longenough"),
            ])
            .send()
            .await;
        assert_eq!(res.status(), 303);
    }

    #[tokio::test]
    async fn htmx_gets_the_form_block_retargeted_at_the_form() {
        let res = app()
            .test()
            .post("/signup")
            .header("hx-request", "true")
            .header("hx-trigger", "signup")
            .header("hx-target", "main")
            .form(&[("email", "x"), ("age", "20"), ("password", "longenough")])
            .send()
            .await;
        assert_eq!(res.status(), 422);
        assert_eq!(res.header("hx-retarget"), Some("#signup"));
        assert_eq!(res.header("hx-reswap"), Some("innerHTML"));
        let html = res.text();
        assert!(
            html.starts_with(r#"<input name="email" value="x">"#),
            "{html}"
        );
        assert!(!html.contains("<h1>"), "{html}");

        // A form that targets itself needs no retarget.
        let res = app()
            .test()
            .post("/signup")
            .header("hx-request", "true")
            .header("hx-trigger", "signup")
            .header("hx-target", "signup")
            .form(&[("email", "x"), ("age", "20"), ("password", "longenough")])
            .send()
            .await;
        assert_eq!(res.header("hx-retarget"), None);
        assert_eq!(res.header("hx-reswap"), Some("innerHTML"));
    }

    #[tokio::test]
    async fn not_a_form_is_still_rejected() {
        let res = app()
            .test()
            .post("/signup")
            .json(&serde_json::json!({"email": "a@b.c"}))
            .send()
            .await;
        assert_eq!(res.status(), 415);
    }

    #[test]
    fn form_errors_from_errors_and_validation() {
        let err = Error::conflict("already there");
        assert_eq!(
            FormErrors::from_error(&err).field("_form"),
            Some("already there")
        );
        let err = Error::internal("db password is wrong");
        let errors = FormErrors::from_error(&err);
        assert!(!errors.field("_form").unwrap().contains("password"));

        let form = SignUp {
            email: "nope".into(),
            age: 1,
            password: "x".into(),
        };
        let validation = form.validate().unwrap_err();
        let errors = FormErrors::from(&validation);
        assert_eq!(errors.len(), 3);
        // The same messages as the JSON 422.
        let err = zapline::validate::validation_error(validation);
        assert_eq!(FormErrors::from_error(&err), errors);
        assert_eq!(
            serde_json::to_value(&errors).unwrap()["age"],
            "you must be 13 or older"
        );
    }
}

// ---- flash in templates ----------------------------------------------------------

#[cfg(feature = "cookies")]
mod flash {
    use zapline::cookies::Key;
    use zapline::prelude::*;
    use zapline::session::{Flash, MemoryStore, SessionLayer};
    use zapline::view::{context, Views};

    use super::TEMPLATES;

    fn app() -> App {
        App::new()
            .views(TEMPLATES)
            .cookie_key(Key::from(&[7u8; 64]))
            .wrap(SessionLayer::new(MemoryStore::new()))
            .post("/save", |flash: Flash| async move {
                flash.success("Saved <3").redirect("/page")
            })
            .get("/page", |views: Views| async move {
                views.render("signup.html", context! { heading => "Page" })
            })
            .get("/inline", |views: Views, flash: Flash| async move {
                flash.info("Shown now");
                views.render("signup.html", context! {})
            })
    }

    #[tokio::test]
    async fn templates_show_a_flash_once_after_the_redirect() {
        let browser = app().test().with_cookies();
        let res = browser.post("/save").send().await;
        assert_eq!(res.status(), 303);
        assert_eq!(res.header("location"), Some("/page"));
        assert_eq!(browser.cookie("zapline.sid.flash").as_deref(), Some("1"));

        let html = browser.get("/page").send().await.text();
        assert!(
            html.contains(r#"<p class="flash-success">Saved &lt;3</p>"#),
            "{html}"
        );
        // Shown, so gone; the marker cookie too.
        assert_eq!(browser.cookie("zapline.sid.flash"), None);
        let html = browser.get("/page").send().await.text();
        assert!(!html.contains("flash-"), "{html}");
    }

    #[tokio::test]
    async fn a_flash_queued_while_rendering_shows_on_the_same_page() {
        let browser = app().test().with_cookies();
        let html = browser.get("/inline").send().await.text();
        assert!(
            html.contains(r#"<p class="flash-info">Shown now</p>"#),
            "{html}"
        );
        assert!(!browser.get("/page").send().await.text().contains("flash-"));
    }

    /// Counts loads, to see that pages without a waiting flash skip the store.
    #[derive(Clone, Default)]
    struct Counting {
        inner: MemoryStore,
        loads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl zapline::session::SessionStore for Counting {
        async fn load(&self, id: &str) -> Result<Option<zapline::session::SessionRecord>> {
            self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.load(id).await
        }
        async fn save(&self, id: &str, record: &zapline::session::SessionRecord) -> Result<()> {
            self.inner.save(id, record).await
        }
        async fn delete(&self, id: &str) -> Result<()> {
            self.inner.delete(id).await
        }
    }

    #[tokio::test]
    async fn pages_load_the_session_only_when_a_flash_waits() {
        let store = Counting::default();
        let loads = || store.loads.load(std::sync::atomic::Ordering::SeqCst);
        let browser = App::new()
            .views(TEMPLATES)
            .cookie_key(Key::from(&[7u8; 64]))
            .wrap(SessionLayer::new(store.clone()))
            .post("/login", |s: zapline::session::Session| async move {
                s.insert("user", 1)
            })
            .post("/save", |flash: Flash| async move {
                flash.success("Saved").redirect("/page")
            })
            .get("/page", |views: Views| async move {
                views.render("signup.html", context! {})
            })
            .test()
            .with_cookies();
        browser.post("/login").send().await;
        let before = loads();
        // A session cookie, but no flash waiting: the store is not asked.
        browser.get("/page").send().await;
        assert_eq!(loads(), before);
        browser.post("/save").send().await;
        let before = loads();
        assert!(browser.get("/page").send().await.text().contains("Saved"));
        assert_eq!(loads(), before + 1);
    }

    #[tokio::test]
    async fn without_sessions_flash_is_empty() {
        let res = App::new()
            .views(TEMPLATES)
            .get("/", |views: Views| async move {
                views.render("signup.html", context! {})
            })
            .test()
            .get("/")
            .send()
            .await;
        assert_eq!(res.status(), 200, "{}", res.text());
        assert!(!res.text().contains("flash-"));
    }
}

#[tokio::test]
async fn views_with_adds_globals_and_filters() {
    let client = App::new()
        .views_with(TEMPLATES, |env| {
            env.add_global("site", "Acme");
            env.add_template("inline.html", "{{ site | upper }}")
                .unwrap();
        })
        .get("/", |v: Views| async move { v.render("inline.html", ()) })
        .test();
    assert_eq!(client.get("/").send().await.text(), "ACME");
}
