//! Translations: loading, language negotiation, the `lang` cookie,
//! fallbacks, plurals, and `t(...)` in templates.
#![cfg(all(feature = "i18n", feature = "macros"))]

use zapline::i18n::{I18n, Locale};
use zapline::prelude::*;

const EN: &str = r#"
hello = "Hello, {name}!"
only_in_english = "Only in English"

[inbox]
count.zero = "No messages"
count.one = "{count} message"
count.other = "{count} messages"
"#;

const FR: &str = r#"
hello = "Bonjour, {name} !"
inbox.count.one = "{count} message"
inbox.count.other = "{count} messages"
"#;

const RU: &str = r#"
hello = "Привет, {name}!"
inbox.count.one = "{count} сообщение"
inbox.count.few = "{count} сообщения"
inbox.count.many = "{count} сообщений"
"#;

fn i18n() -> I18n {
    I18n::default()
        .messages("fr", FR)
        .unwrap()
        .messages("en", EN)
        .unwrap()
        .messages("ru", RU)
        .unwrap()
}

#[get("/hello")]
async fn hello(t: Locale) -> String {
    format!("{} {}", t.lang(), t.t_with("hello", &[("name", &"Ada")]))
}

#[get("/inbox/{n}")]
async fn inbox(t: Locale, n: Path<i64>) -> String {
    t.plural("inbox.count", n.0, &[])
}

#[get("/fallback")]
async fn fallback(t: Locale) -> String {
    format!("{} | {}", t.t("only_in_english"), t.t("no.such.key"))
}

fn client() -> zapline::test::TestClient {
    App::new()
        .i18n(i18n())
        .mount((hello, inbox, fallback))
        .test()
}

#[tokio::test]
async fn the_language_comes_from_accept_language() {
    let client = client();
    client
        .get("/hello")
        .send()
        .await
        .assert_text("en Hello, Ada!");
    for (header, expected) in [
        ("fr-CH, fr;q=0.9, en;q=0.8", "fr Bonjour, Ada !"),
        ("de-DE, ru;q=0.5, en;q=0.4", "ru Привет, Ada!"),
        ("en;q=0.2, fr;q=0.9", "fr Bonjour, Ada !"),
        ("ja, *;q=0.5", "en Hello, Ada!"),
        ("fr;q=0", "en Hello, Ada!"),
    ] {
        client
            .get("/hello")
            .header("accept-language", header)
            .send()
            .await
            .assert_text(expected);
    }
}

#[tokio::test]
async fn lang_in_the_query_wins_and_is_remembered() {
    let client = client().with_cookies();
    let res = client
        .get("/hello?lang=ru")
        .header("accept-language", "fr")
        .send()
        .await;
    res.assert_text("ru Привет, Ada!");
    let cookie = res.cookie("lang").expect("lang cookie");
    assert_eq!(cookie.value(), "ru");
    assert_eq!(cookie.path(), Some("/"));

    // Later requests keep it, even against Accept-Language.
    client
        .get("/hello")
        .header("accept-language", "fr")
        .send()
        .await
        .assert_text("ru Привет, Ada!");

    // An unknown language is ignored and not remembered.
    let res = client.get("/hello?lang=xx").send().await;
    res.assert_text("ru Привет, Ada!");
    assert!(res.cookie("lang").is_none());
}

#[tokio::test]
async fn plurals_follow_each_language() {
    let client = client();
    let get = |lang: &'static str, n: i64| {
        let client = &client;
        async move {
            client
                .get(&format!("/inbox/{n}?lang={lang}"))
                .send()
                .await
                .text()
        }
    };
    assert_eq!(get("en", 0).await, "No messages");
    assert_eq!(get("en", 1).await, "1 message");
    assert_eq!(get("en", 7).await, "7 messages");
    // French has no `zero` form of its own: 0 is singular there, but the
    // default language's `zero` message isn't borrowed.
    assert_eq!(get("fr", 0).await, "0 message");
    assert_eq!(get("ru", 21).await, "21 сообщение");
    assert_eq!(get("ru", 3).await, "3 сообщения");
    assert_eq!(get("ru", 11).await, "11 сообщений");
}

#[tokio::test]
async fn missing_messages_fall_back_and_are_listed() {
    let client = client();
    client
        .get("/fallback?lang=fr")
        .send()
        .await
        .assert_text("Only in English | no.such.key");
    assert_eq!(
        i18n().missing(),
        [
            ("fr".to_owned(), "only_in_english".to_owned()),
            ("ru".to_owned(), "only_in_english".to_owned()),
        ]
    );
}

#[test]
fn load_reads_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("en.toml"), EN).unwrap();
    std::fs::write(dir.path().join("pt-BR.toml"), "hello = \"Olá, {name}!\"").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
    let i18n = I18n::load(dir.path()).unwrap();
    assert_eq!(i18n.langs().collect::<Vec<_>>(), ["en", "pt-BR"]);
    assert_eq!(
        i18n.locale("pt").t_with("hello", &[("name", &"Ada")]),
        "Olá, Ada!"
    );

    std::fs::write(dir.path().join("de.toml"), "hello = [1]").unwrap();
    let err = I18n::load(dir.path()).unwrap_err();
    assert!(
        err.message().contains("\"de\": hello is not text"),
        "{}",
        err.message()
    );

    let empty = tempfile::tempdir().unwrap();
    assert!(I18n::load(empty.path()).is_err());
    assert!(I18n::load(empty.path().join("nope")).is_err());
}

#[cfg(feature = "views")]
#[tokio::test]
async fn templates_call_t() {
    use zapline::view::{context, Views};

    #[get("/page")]
    async fn page(views: Views, t: Locale) -> Result<Html<String>> {
        let unread = 3;
        views.render("page.html", context! { t, unread })
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("page.html"),
        "<html lang=\"{{ t.lang }}\"><p>{{ t(\"hello\", name=\"<Ada>\") }}</p>\
         <p>{{ t.plural(\"inbox.count\", unread) }}</p>",
    )
    .unwrap();
    let client = App::new()
        .views(dir.path().to_path_buf())
        .i18n(i18n())
        .mount(page)
        .test();
    client
        .get("/page")
        .header("accept-language", "ru")
        .send()
        .await
        .assert_text("<html lang=\"ru\"><p>Привет, &lt;Ada&gt;!</p><p>3 сообщения</p>");
}
