//! Fingerprinted assets: hashed URLs, long caching for current hashes,
//! plain names still served, and `asset()` in templates.
#![cfg(feature = "fs")]

use zapline::fs::Assets;
use zapline::prelude::*;

fn site() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("static/css")).unwrap();
    std::fs::write(dir.path().join("static/css/app.css"), "body{color:red}").unwrap();
    std::fs::write(dir.path().join("static/LICENSE"), "MIT").unwrap();
    std::fs::write(dir.path().join("static/.env"), "SECRET=1").unwrap();
    dir
}

#[get("/url/{*path}")]
async fn url(assets: Assets, path: Path<String>) -> String {
    assets.url(&path.0)
}

#[tokio::test]
async fn hashed_urls_are_cached_forever() {
    let dir = site();
    let client = App::new()
        .assets("/static", dir.path().join("static"))
        .mount(url)
        .test();

    let css = client.get("/url/css/app.css").send().await.text();
    let hash = css
        .strip_prefix("/static/css/app.")
        .and_then(|rest| rest.strip_suffix(".css"))
        .unwrap_or_else(|| panic!("{css}"));
    assert_eq!(hash.len(), 8);
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    // Stable for the same content.
    assert_eq!(client.get("/url/css/app.css").send().await.text(), css);

    let res = client.get(&css).send().await;
    res.assert_status(200).assert_text("body{color:red}");
    assert_eq!(
        res.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert!(res.header("content-type").unwrap().starts_with("text/css"));

    // The plain name works too, without the long cache.
    let res = client.get("/static/css/app.css").send().await;
    res.assert_status(200).assert_text("body{color:red}");
    assert_eq!(res.header("cache-control"), None);

    // Files without an extension.
    let license = client.get("/url/LICENSE").send().await.text();
    assert!(license.starts_with("/static/LICENSE."), "{license}");
    client
        .get(&license)
        .send()
        .await
        .assert_status(200)
        .assert_text("MIT");

    // A new version gets a new URL; the old one serves today's file, briefly.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(dir.path().join("static/css/app.css"), "body{color:blue}").unwrap();
    let newer = client.get("/url/css/app.css").send().await.text();
    assert_ne!(newer, css);
    let res = client.get(&css).send().await;
    res.assert_text("body{color:blue}");
    assert_eq!(res.header("cache-control"), Some("no-cache"));

    // Missing files keep their name; hidden files and escapes stay hidden.
    assert_eq!(
        client.get("/url/nope.js").send().await.text(),
        "/static/nope.js"
    );
    client
        .get("/static/nope.12345678.js")
        .send()
        .await
        .assert_status(404);
    assert_eq!(client.get("/url/.env").send().await.text(), "/static/.env");
    client.get("/static/.env").send().await.assert_status(404);
    client
        .get("/static/.env.12345678")
        .send()
        .await
        .assert_status(404);
    client
        .get("/static/css/..%2F.env.12345678")
        .send()
        .await
        .assert_status(404);
}

#[cfg(feature = "views")]
#[tokio::test]
async fn templates_call_asset() {
    use zapline::view::{context, Views};

    #[get("/")]
    async fn page(views: Views) -> Result<Html<String>> {
        views.render("page.html", context! {})
    }

    let dir = site();
    std::fs::create_dir_all(dir.path().join("templates")).unwrap();
    std::fs::write(
        dir.path().join("templates/page.html"),
        r#"<link rel="stylesheet" href="{{ asset('css/app.css') }}">"#,
    )
    .unwrap();
    // Views registered after assets work as well as before.
    let client = App::new()
        .assets("/static", dir.path().join("static"))
        .views(dir.path().join("templates"))
        .mount(page)
        .test();
    let html = client.get("/").send().await.text();
    let href = html
        .split('"')
        .nth(3)
        .unwrap_or_else(|| panic!("{html}"))
        .to_owned();
    assert!(
        href.starts_with("/static/css/app.") && href.ends_with(".css") && href.len() == 28,
        "{href}"
    );
    client
        .get(&href)
        .send()
        .await
        .assert_status(200)
        .assert_header("cache-control", "public, max-age=31536000, immutable");
}
