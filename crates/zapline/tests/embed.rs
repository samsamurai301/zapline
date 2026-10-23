//! Single-binary apps: `embed!` compiles directories into the test binary,
//! and static files, fingerprinted assets, templates, migrations and
//! translations are served from that copy, exactly like from disk.
#![cfg(feature = "macros")]

use zapline::embed::{Dir, EmbeddedFile};
use zapline::prelude::*;

fn embedded_static() -> Dir {
    zapline::embed!("tests/fixtures/embed/static").in_memory()
}

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/embed");

#[test]
fn embed_lists_and_reads_files_without_hidden_ones() {
    let dir = embedded_static();
    assert!(dir.is_embedded());
    assert!(dir.exists());
    assert_eq!(
        dir.files().unwrap(),
        [
            "app.js",
            "app.js.gz",
            "css/app.css",
            "docs/index.html",
            "index.html"
        ]
    );
    assert_eq!(
        dir.read_to_string("css/app.css").unwrap(),
        "body{color:rebeccapurple}\n"
    );
    assert_eq!(
        dir.read(".secret").unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(dir.read("../templates/base.html").is_err());
    assert_eq!(
        dir.path(),
        std::path::Path::new(&format!("{FIXTURES}/static"))
    );
}

#[test]
fn debug_builds_read_the_sources_and_release_builds_the_copy() {
    let dir = zapline::embed!("tests/fixtures/embed/static");
    // Tests are debug builds: next to the sources, the disk wins.
    assert_eq!(dir.is_embedded(), !cfg!(debug_assertions));
    // Either way the contents are the same, hidden files skipped.
    assert_eq!(dir.files().unwrap(), embedded_static().files().unwrap());
    assert_eq!(
        zapline::embed::content_hash(&dir.read("app.js").unwrap()),
        zapline::embed::content_hash(&embedded_static().read("app.js").unwrap())
    );
}

static LIVE: &[EmbeddedFile] = &[EmbeddedFile {
    path: "page.txt",
    bytes: b"compiled in",
    hash: "00000000",
}];

#[test]
fn edits_on_disk_show_up_in_debug_builds_but_not_in_memory() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("page.txt"), "v1").unwrap();
    let dir = Dir::__from_embedded(tmp.path().to_str().unwrap(), LIVE);
    if cfg!(debug_assertions) {
        assert_eq!(dir.read_to_string("page.txt").unwrap(), "v1");
        std::fs::write(tmp.path().join("page.txt"), "v2").unwrap();
        assert_eq!(dir.read_to_string("page.txt").unwrap(), "v2");
    }
    assert_eq!(
        dir.clone().in_memory().read_to_string("page.txt").unwrap(),
        "compiled in"
    );

    // Without the sources (a binary copied to a server) it is the copy.
    let gone = Dir::__from_embedded("/no/such/source/dir", LIVE);
    assert!(gone.is_embedded());
    assert_eq!(gone.read_to_string("page.txt").unwrap(), "compiled in");
}

#[cfg(feature = "fs")]
mod static_files {
    use super::*;
    use zapline::fs::ServeDir;

    fn client() -> zapline::test::TestClient {
        App::new()
            .static_files(
                "/s",
                ServeDir::new(embedded_static())
                    .precompressed_gzip()
                    .cache_control("public, max-age=60"),
            )
            .static_files(
                "/spa",
                ServeDir::new(embedded_static()).fallback_file("index.html"),
            )
            .test()
    }

    #[tokio::test]
    async fn serves_files_from_memory_with_validators_and_ranges() {
        let client = client();
        let res = client.get("/s/css/app.css").send().await;
        res.assert_status(200)
            .assert_text("body{color:rebeccapurple}\n");
        assert_eq!(res.header("content-type"), Some("text/css; charset=utf-8"));
        assert_eq!(res.header("content-length"), Some("26"));
        assert_eq!(res.header("cache-control"), Some("public, max-age=60"));
        assert_eq!(res.header("accept-ranges"), Some("bytes"));
        assert_eq!(res.header("last-modified"), None);
        let etag = res.header("etag").unwrap().to_owned();
        assert!(etag.starts_with("W/\"1a-"), "{etag}");

        client
            .get("/s/css/app.css")
            .header("if-none-match", &etag)
            .send()
            .await
            .assert_status(304);

        let res = client
            .get("/s/css/app.css")
            .header("range", "bytes=5-9")
            .send()
            .await;
        res.assert_status(206).assert_text("color");
        assert_eq!(res.header("content-range"), Some("bytes 5-9/26"));
        client
            .get("/s/css/app.css")
            .header("range", "bytes=99-")
            .send()
            .await
            .assert_status(416);

        let res = client.request(Method::HEAD, "/s/css/app.css").send().await;
        res.assert_status(200).assert_text("");
        assert_eq!(res.header("content-length"), Some("26"));
    }

    #[tokio::test]
    async fn indexes_redirects_hidden_files_and_precompressed_variants() {
        let client = client();
        client
            .get("/s")
            .send()
            .await
            .assert_status(200)
            .assert_text("<h1>home</h1>\n");
        let res = client.get("/s/docs").send().await;
        res.assert_status(307);
        assert_eq!(res.header("location"), Some("/s/docs/"));
        client
            .get("/s/docs/")
            .send()
            .await
            .assert_status(200)
            .assert_text("<h1>docs</h1>\n");

        client.get("/s/.secret").send().await.assert_status(404);
        client.get("/s/nope.txt").send().await.assert_status(404);
        client
            .get("/s/../Cargo.toml")
            .send()
            .await
            .assert_status(404);
        client.post("/s/app.js").send().await.assert_status(405);

        let res = client
            .get("/s/app.js")
            .header("accept-encoding", "gzip")
            .send()
            .await;
        res.assert_status(200).assert_text("GZIPPED-app.js");
        assert_eq!(res.header("content-encoding"), Some("gzip"));
        assert_eq!(
            res.header("content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(res.header("vary"), Some("accept-encoding"));
        let res = client.get("/s/app.js").send().await;
        res.assert_status(200)
            .assert_text("console.log(\"embedded\");\n");
        assert_eq!(res.header("content-encoding"), None);

        // Single-page apps: unknown paths get the fallback file.
        client
            .get("/spa/some/client/route")
            .send()
            .await
            .assert_status(200)
            .assert_text("<h1>home</h1>\n");
    }

    #[get("/url/{*path}")]
    async fn url(assets: zapline::fs::Assets, path: Path<String>) -> String {
        assets.url(&path.0)
    }

    #[tokio::test]
    async fn fingerprinted_urls_match_the_disk_and_are_cached_forever() {
        let memory = App::new()
            .assets("/static", embedded_static())
            .mount(url)
            .test();
        let disk = App::new()
            .assets("/static", format!("{FIXTURES}/static"))
            .mount(url)
            .test();

        let css = memory.get("/url/css/app.css").send().await.text();
        assert_eq!(css, disk.get("/url/css/app.css").send().await.text());
        let hash = zapline::embed::content_hash(b"body{color:rebeccapurple}\n");
        assert_eq!(css, format!("/static/css/app.{hash}.css"));

        let res = memory.get(&css).send().await;
        res.assert_status(200)
            .assert_text("body{color:rebeccapurple}\n");
        assert_eq!(
            res.header("cache-control"),
            Some("public, max-age=31536000, immutable")
        );
        // A stale hash still gets today's file, without the long cache.
        let res = memory.get("/static/css/app.0badf00d.css").send().await;
        res.assert_status(200);
        assert_eq!(res.header("cache-control"), Some("no-cache"));
        // Missing files get no hash.
        assert_eq!(
            memory.get("/url/missing.css").send().await.text(),
            "/static/missing.css"
        );
    }
}

#[cfg(all(feature = "views", feature = "fs"))]
mod views {
    use super::*;
    use zapline::view::{context, HxRequest, Views};

    #[get("/hello/{name}")]
    async fn hello(views: Views, hx: HxRequest, name: Path<String>) -> Result<Html<String>> {
        views.page_or_block(
            &hx,
            "pages/hello.html",
            "greeting",
            context! { name => name.0 },
        )
    }

    #[tokio::test]
    async fn templates_render_from_memory_with_asset_urls() {
        let client = App::new()
            .views(zapline::embed!("tests/fixtures/embed/templates").in_memory())
            .assets("/static", embedded_static())
            .mount(hello)
            .test();
        let hash = zapline::embed::content_hash(b"body{color:rebeccapurple}\n");
        let page = client.get("/hello/Ada").send().await;
        page.assert_status(200);
        let html = page.text();
        assert!(html.contains("<title>Hello</title>"), "{html}");
        assert!(html.contains("<p id=greeting>Hello, Ada!</p>"), "{html}");
        assert!(
            html.contains(&format!("href=\"/static/css/app.{hash}.css\"")),
            "{html}"
        );
        // htmx partial: just the block.
        client
            .get("/hello/Ada")
            .header("hx-request", "true")
            .send()
            .await
            .assert_status(200)
            .assert_text("Hello, Ada!");
    }

    #[tokio::test]
    async fn a_missing_template_is_a_server_error() {
        let views = Views::new(
            zapline::embed!("tests/fixtures/embed/templates").in_memory(),
            false,
        );
        let Err(err) = views.render("nope.html", context! {}) else {
            panic!("rendered a missing template");
        };
        assert_eq!(err.status(), 500);
    }
}

#[cfg(feature = "sqlite")]
mod migrations {
    use super::*;
    use zapline::db::{sqlx, Database, SqliteDb as Db};

    #[get("/columns")]
    async fn columns(db: Db) -> Result<String> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info('notes')")
            .fetch_all(&*db)
            .await?;
        Ok(rows.into_iter().map(|r| r.0).collect::<Vec<_>>().join(","))
    }

    #[tokio::test]
    async fn migrations_run_from_memory() {
        let client = App::new()
            .database(
                Database::sqlite_memory()
                    .migrations(zapline::embed!("tests/fixtures/embed/migrations").in_memory()),
            )
            .mount(columns)
            .test();
        client
            .get("/columns")
            .send()
            .await
            .assert_status(200)
            .assert_text("id,title,body");
    }

    #[tokio::test]
    async fn embedded_and_disk_migrations_have_the_same_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("app.db").display());
        // First deploy reads the disk...
        App::new()
            .database(Database::sqlite(&url).migrations(format!("{FIXTURES}/migrations")))
            .mount(columns)
            .test()
            .get("/columns")
            .send()
            .await
            .assert_status(200);
        // ...the next one the embedded copy: nothing looks edited.
        App::new()
            .database(
                Database::sqlite(&url)
                    .migrations(zapline::embed!("tests/fixtures/embed/migrations").in_memory()),
            )
            .mount(columns)
            .test()
            .get("/columns")
            .send()
            .await
            .assert_status(200)
            .assert_text("id,title,body");
    }
}

#[cfg(feature = "i18n")]
#[test]
fn translations_load_from_memory() {
    let i18n =
        zapline::i18n::I18n::load(zapline::embed!("tests/fixtures/embed/locales").in_memory())
            .unwrap();
    assert_eq!(i18n.langs().collect::<Vec<_>>(), ["en", "fr"]);
    assert_eq!(
        i18n.locale("fr").t_with("hello", &[("name", &"Ada")]),
        "Bonjour, Ada !"
    );
}

#[cfg(feature = "config")]
#[test]
fn configuration_layers_load_from_memory() {
    use zapline::config::Config;

    let embedded = zapline::embed!("tests/fixtures/embed/config").in_memory();
    let prod = Config::builder()
        .standard_in(embedded.clone())
        .env_source([("APP_ENV", "production")])
        .build()
        .unwrap();
    assert_eq!(prod.profile(), "production");
    assert_eq!(prod.get::<u16>("server.port").unwrap(), 80);
    assert_eq!(prod.get::<String>("server.name").unwrap(), "embedded");
    // Environment variables still win over the embedded files.
    let overridden = Config::builder()
        .standard_in(embedded)
        .env_source([("APP_ENV", "production"), ("APP__SERVER__PORT", "9000")])
        .build()
        .unwrap();
    assert_eq!(overridden.get::<u16>("server.port").unwrap(), 9000);
}
