//! Static file serving: content types, index/fallback files, conditional
//! and range requests, precompressed variants, HEAD, and path security.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapline::fs::{ServeDir, ServeFile, StaticFilesRoute};
use zapline::middleware::NormalizePath;
use zapline::prelude::*;
use zapline::test::{TestClient, TestResponse};

struct Fixture {
    // Keeps the directory alive; also read by the unix-only symlink tests.
    #[cfg_attr(not(unix), allow(dead_code))]
    tmp: TempDir,
    root: PathBuf,
}

const BIG: usize = 300_000;

fn big_data() -> Vec<u8> {
    (0..BIG).map(|i| (i % 251) as u8).collect()
}

fn write(path: &Path, data: impl AsRef<[u8]>) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(path, data).unwrap();
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("public");
    write(&tmp.path().join("secret.txt"), "TOP SECRET");
    write(&tmp.path().join("Cargo.toml"), "TOP SECRET");
    write(&root.join("index.html"), "<h1>home</h1>");
    write(&root.join("style.css"), "body{color:red}");
    write(&root.join("app.js"), "console.log('plain')");
    write(&root.join("app.js.gz"), "GZIP-BYTES");
    write(&root.join("app.js.br"), "BROTLI-BYTES");
    write(&root.join("only-gz.txt"), "plain text");
    write(&root.join("only-gz.txt.gz"), "GZ");
    write(&root.join("data.bin"), big_data());
    write(&root.join("empty.txt"), "");
    write(&root.join("logo.png"), [0x89, b'P', b'N', b'G']);
    write(&root.join("weird name %.txt"), "spaces");
    write(&root.join(".env"), "SECRET=1");
    write(&root.join(".git/config"), "[core]");
    write(&root.join("sub/index.html"), "<h1>sub</h1>");
    write(&root.join("sub/page.txt"), "page");
    write(&root.join("noindex/a.txt"), "a");
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(tmp.path().join("secret.txt"), root.join("link-out.txt")).unwrap();
        symlink(tmp.path(), root.join("linkdir")).unwrap();
        symlink(root.join("style.css"), root.join("alias.css")).unwrap();
        symlink(root.join(".env"), root.join("env-link")).unwrap();
    }
    Fixture { tmp, root }
}

fn client(dir: ServeDir) -> TestClient {
    App::new().static_files("/assets", dir).test()
}

#[tokio::test]
async fn serves_files_with_metadata() {
    let f = fixture();
    let c = client(ServeDir::new(&f.root));

    let r = c.get("/assets/style.css").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.text(), "body{color:red}");
    assert_eq!(r.header("content-type"), Some("text/css; charset=utf-8"));
    assert_eq!(r.header("content-length"), Some("15"));
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert!(r.header("etag").unwrap().starts_with("W/\""));
    assert!(r.header("last-modified").unwrap().ends_with(" GMT"));
    assert_eq!(r.header("cache-control"), None);
    assert_eq!(r.header("vary"), None);

    let r = c.get("/assets/logo.png").send().await;
    assert_eq!(r.header("content-type"), Some("image/png"));
    let r = c.get("/assets/app.js").send().await;
    assert_eq!(
        r.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(r.text(), "console.log('plain')");

    // Percent-encoded names are decoded once.
    let r = c.get("/assets/weird%20name%20%25.txt").send().await;
    assert_eq!(r.text(), "spaces");

    let r = c.get("/assets/empty.txt").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-length"), Some("0"));
    assert!(r.bytes().is_empty());

    assert_eq!(c.get("/assets/missing.txt").send().await.status(), 404);
    assert_eq!(c.get("/assets/style.css/x").send().await.status(), 404);
}

#[tokio::test]
async fn large_files_are_streamed_exactly() {
    let f = fixture();
    let c = client(ServeDir::new(&f.root));
    let r = c.get("/assets/data.bin").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-length"), Some("300000"));
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    assert_eq!(r.bytes().as_ref(), big_data().as_slice());
}

#[tokio::test]
async fn index_files_redirects_and_fallback() {
    let f = fixture();
    let c = client(ServeDir::new(&f.root));
    assert_eq!(c.get("/assets").send().await.text(), "<h1>home</h1>");
    assert_eq!(c.get("/assets/sub/").send().await.text(), "<h1>sub</h1>");
    assert_eq!(c.get("/assets/sub/page.txt").send().await.text(), "page");

    let r = c.get("/assets/sub?x=1").send().await;
    assert_eq!(r.status(), 307);
    assert_eq!(r.header("location"), Some("/assets/sub/?x=1"));

    // Directories without an index are not listed.
    assert_eq!(c.get("/assets/noindex/").send().await.status(), 404);
    assert_eq!(c.get("/assets/noindex").send().await.status(), 404);

    let c = client(ServeDir::new(&f.root).no_index_file());
    assert_eq!(c.get("/assets").send().await.status(), 404);
    assert_eq!(c.get("/assets/sub/").send().await.status(), 404);

    let c = client(ServeDir::new(&f.root).index_file("page.txt"));
    assert_eq!(c.get("/assets/sub/").send().await.text(), "page");

    // SPA fallback: unknown paths get index.html with 200...
    let c = client(ServeDir::new(&f.root).fallback_file("index.html"));
    let r = c.get("/assets/some/client/route").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.text(), "<h1>home</h1>");
    assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
    // ...but rejected paths still 404.
    assert_eq!(c.get("/assets/../secret.txt").send().await.status(), 404);
    assert_eq!(c.get("/assets/.env").send().await.status(), 404);
}

async fn assert_not_served(c: &TestClient, path: &str) {
    let r = c.get(path).send().await;
    assert_eq!(r.status(), 404, "{path} -> {}", r.text());
    assert!(!r.text().contains("SECRET"), "{path} leaked: {}", r.text());
}

#[tokio::test]
async fn path_traversal_is_rejected() {
    let f = fixture();
    let c = client(ServeDir::new(&f.root).show_hidden(true));
    for path in [
        "/assets/../secret.txt",
        "/assets/../Cargo.toml",
        "/assets/sub/../../secret.txt",
        "/assets/%2e%2e/secret.txt",
        "/assets/%2e%2e%2fsecret.txt",
        "/assets/%2E%2E%2Fsecret.txt",
        "/assets/..%2fsecret.txt",
        "/assets/..%5csecret.txt",
        "/assets/sub%5c..%5c..%5csecret.txt",
        "/assets/%252e%252e%252fsecret.txt",
        "/assets/style.css%00.png",
        "/assets//etc/passwd",
        "/assets/%2fetc%2fpasswd",
    ] {
        assert_not_served(&c, path).await;
    }
    // Sanity: the same client does serve legitimate files.
    assert_eq!(c.get("/assets/style.css").send().await.status(), 200);
}

#[tokio::test]
async fn path_traversal_over_a_real_socket() {
    let f = fixture();
    let addr = serve(App::new().static_files("/assets", ServeDir::new(&f.root))).await;
    for path in ["/assets/../secret.txt", "/assets/%2e%2e/secret.txt"] {
        let out = raw(
            addr,
            &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 404"), "{path}: {out}");
        assert!(!out.contains("TOP SECRET"), "{path}: {out}");
    }
}

#[tokio::test]
async fn hidden_files() {
    let f = fixture();
    let c = client(ServeDir::new(&f.root));
    assert_not_served(&c, "/assets/.env").await;
    assert_not_served(&c, "/assets/.git/config").await;
    assert_not_served(&c, "/assets/%2eenv").await;

    let c = client(ServeDir::new(&f.root).show_hidden(true));
    assert_eq!(c.get("/assets/.env").send().await.text(), "SECRET=1");
    assert_eq!(c.get("/assets/.git/config").send().await.text(), "[core]");
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_cannot_escape_the_root() {
    let f = fixture();
    let c = client(ServeDir::new(&f.root));
    assert_not_served(&c, "/assets/link-out.txt").await;
    assert_not_served(&c, "/assets/linkdir/secret.txt").await;
    // A link to a hidden file is still hidden.
    assert_not_served(&c, "/assets/env-link").await;
    // Links that stay inside the root work.
    assert_eq!(
        c.get("/assets/alias.css").send().await.text(),
        "body{color:red}"
    );

    // The root itself may be a symlink.
    let link_root = f.tmp.path().join("root-link");
    std::os::unix::fs::symlink(&f.root, &link_root).unwrap();
    let c = client(ServeDir::new(&link_root));
    assert_eq!(c.get("/assets/style.css").send().await.status(), 200);
    assert_not_served(&c, "/assets/link-out.txt").await;
}

#[tokio::test]
async fn conditional_requests() {
    let f = fixture();
    let c = client(ServeDir::new(&f.root).cache_control("public, max-age=60"));
    let r = c.get("/assets/style.css").send().await;
    let etag = r.header("etag").unwrap().to_owned();
    let last_modified = r.header("last-modified").unwrap().to_owned();
    assert_eq!(r.header("cache-control"), Some("public, max-age=60"));

    for inm in [
        etag.as_str(),
        "*",
        &format!("\"nope\", {etag}"),
        etag.trim_start_matches("W/"),
    ] {
        let r = c
            .get("/assets/style.css")
            .header("if-none-match", inm)
            .send()
            .await;
        assert_eq!(r.status(), 304, "{inm}");
        assert!(r.bytes().is_empty());
        assert_eq!(r.header("etag"), Some(etag.as_str()));
        assert_eq!(r.header("cache-control"), Some("public, max-age=60"));
    }
    let r = c
        .get("/assets/style.css")
        .header("if-none-match", "W/\"other\"")
        .send()
        .await;
    assert_eq!(r.status(), 200);

    let r = c
        .get("/assets/style.css")
        .header("if-modified-since", &last_modified)
        .send()
        .await;
    assert_eq!(r.status(), 304);
    let r = c
        .get("/assets/style.css")
        .header("if-modified-since", "Mon, 01 Jan 1990 00:00:00 GMT")
        .send()
        .await;
    assert_eq!(r.status(), 200);
    let r = c
        .get("/assets/style.css")
        .header("if-modified-since", "garbage")
        .send()
        .await;
    assert_eq!(r.status(), 200);
    // If-None-Match takes precedence over If-Modified-Since.
    let r = c
        .get("/assets/style.css")
        .header("if-none-match", "\"other\"")
        .header("if-modified-since", &last_modified)
        .send()
        .await;
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn range_requests() {
    let f = fixture();
    let c = client(ServeDir::new(&f.root));
    let range = |r: &'static str| c.get("/assets/style.css").header("range", r);

    let r = range("bytes=0-3").send().await;
    assert_eq!(r.status(), 206);
    assert_eq!(r.text(), "body");
    assert_eq!(r.header("content-range"), Some("bytes 0-3/15"));
    assert_eq!(r.header("content-length"), Some("4"));

    let r = range("bytes=5-").send().await;
    assert_eq!((r.status().as_u16(), r.text()), (206, "color:red}".into()));
    assert_eq!(r.header("content-range"), Some("bytes 5-14/15"));

    let r = range("bytes=-4").send().await;
    assert_eq!((r.status().as_u16(), r.text()), (206, "red}".into()));

    let r = range("bytes=10-1000").send().await;
    assert_eq!(r.header("content-range"), Some("bytes 10-14/15"));

    for unsatisfiable in ["bytes=15-", "bytes=100-200", "bytes=-0"] {
        let r = range(unsatisfiable).send().await;
        assert_eq!(r.status(), 416, "{unsatisfiable}");
        assert_eq!(r.header("content-range"), Some("bytes */15"));
        assert_eq!(r.headers().get_all("content-type").iter().count(), 1);
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(
            r.json::<serde_json::Value>()["code"],
            "range_not_satisfiable"
        );
        assert!(r.header("etag").is_some());
    }

    // Multiple ranges and malformed headers get the full file.
    for full in ["bytes=0-1,4-5", "bytes=5-2", "bytes=abc", "lines=1-2"] {
        let r = range(full).send().await;
        assert_eq!(r.status(), 200, "{full}");
        assert_eq!(r.text(), "body{color:red}");
    }

    // If-Range: a matching date honors the range; an entity tag (ours are
    // weak) or a stale date sends the full file.
    let full = c.get("/assets/style.css").send().await;
    let lm = full.header("last-modified").unwrap().to_owned();
    let etag = full.header("etag").unwrap().to_owned();
    let r = range("bytes=0-3").header("if-range", &lm).send().await;
    assert_eq!(r.status(), 206);
    let r = range("bytes=0-3").header("if-range", &etag).send().await;
    assert_eq!(r.status(), 200);
    let r = range("bytes=0-3")
        .header("if-range", "Mon, 01 Jan 1990 00:00:00 GMT")
        .send()
        .await;
    assert_eq!(r.status(), 200);

    // Empty files cannot satisfy any range.
    let r = c
        .get("/assets/empty.txt")
        .header("range", "bytes=0-")
        .send()
        .await;
    assert_eq!(r.status(), 416);
    assert_eq!(r.header("content-range"), Some("bytes */0"));

    // Ranges inside a large, streamed file.
    let data = big_data();
    let r = c
        .get("/assets/data.bin")
        .header("range", "bytes=100000-250000")
        .send()
        .await;
    assert_eq!(r.status(), 206);
    assert_eq!(r.header("content-length"), Some("150001"));
    assert_eq!(r.bytes().as_ref(), &data[100_000..=250_000]);
    let r = c
        .get("/assets/data.bin")
        .header("range", "bytes=-10")
        .send()
        .await;
    assert_eq!(r.bytes().as_ref(), &data[BIG - 10..]);
}

#[tokio::test]
async fn head_requests_have_headers_but_no_body() {
    let f = fixture();
    let c = client(ServeDir::new(&f.root));
    let r = c.request(Method::HEAD, "/assets/data.bin").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-length"), Some("300000"));
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    assert!(r.bytes().is_empty());
    // Range is only defined for GET.
    let r = c
        .request(Method::HEAD, "/assets/style.css")
        .header("range", "bytes=0-1")
        .send()
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-length"), Some("15"));

    let addr = serve(App::new().static_files("/assets", ServeDir::new(&f.root))).await;
    let out = raw(
        addr,
        "HEAD /assets/data.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains("content-length: 300000"), "{out}");
    assert!(out.ends_with("\r\n\r\n"), "HEAD response has a body: {out}");
}

#[tokio::test]
async fn large_file_over_a_real_socket() {
    let f = fixture();
    let addr = serve(App::new().static_files("/assets", ServeDir::new(&f.root))).await;
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /assets/data.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&out[..split]);
    assert!(head.contains("content-length: 300000"), "{head}");
    assert!(!head.contains("chunked"), "{head}");
    assert_eq!(&out[split..], big_data().as_slice());
}

#[tokio::test]
async fn precompressed_variants() {
    let f = fixture();
    let c = client(
        ServeDir::new(&f.root)
            .precompressed_gzip()
            .precompressed_br(),
    );
    let get = |ae: Option<&'static str>| {
        let req = c.get("/assets/app.js");
        match ae {
            Some(v) => req.header("accept-encoding", v),
            None => req,
        }
    };

    let r = get(Some("gzip")).send().await;
    assert_eq!(r.text(), "GZIP-BYTES");
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(
        r.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(r.header("vary"), Some("accept-encoding"));
    assert_eq!(r.header("content-length"), Some("10"));

    let r = get(Some("gzip, deflate, br")).send().await;
    assert_eq!(r.text(), "BROTLI-BYTES");
    assert_eq!(r.header("content-encoding"), Some("br"));
    let gz_etag = get(Some("gzip"))
        .send()
        .await
        .header("etag")
        .unwrap()
        .to_owned();
    assert_ne!(r.header("etag").unwrap(), gz_etag);

    let r = get(Some("br;q=0.1, gzip;q=0.9")).send().await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    let r = get(Some("*")).send().await;
    assert_eq!(r.header("content-encoding"), Some("br"));

    for identity in [None, Some("identity"), Some("gzip;q=0, br;q=0")] {
        let r = get(identity).send().await;
        assert_eq!(r.text(), "console.log('plain')", "{identity:?}");
        assert_eq!(r.header("content-encoding"), None);
        assert_eq!(r.header("vary"), Some("accept-encoding"));
    }

    // Falls back to the next acceptable variant that exists.
    let r = c
        .get("/assets/only-gz.txt")
        .header("accept-encoding", "br, gzip")
        .send()
        .await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));

    // Not enabled: the raw file is served.
    let c = client(ServeDir::new(&f.root));
    let r = c
        .get("/assets/app.js")
        .header("accept-encoding", "gzip, br")
        .send()
        .await;
    assert_eq!(r.text(), "console.log('plain')");
    assert_eq!(r.header("content-encoding"), None);
}

#[tokio::test]
async fn serve_file_handler() {
    let f = fixture();
    let c = App::new()
        .get("/favicon.png", ServeFile::new(f.root.join("logo.png")))
        .get(
            "/robots.txt",
            ServeFile::new(f.root.join("sub/page.txt")).cache_control("public, max-age=86400"),
        )
        .get("/gone", ServeFile::new(f.root.join("nope.txt")))
        .any("/any", ServeFile::new(f.root.join("style.css")))
        .get(
            "/app.js",
            ServeFile::new(f.root.join("app.js")).precompressed_br(),
        )
        .test();

    let r = c.get("/favicon.png").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-type"), Some("image/png"));
    assert_eq!(r.bytes().len(), 4);

    let r = c.get("/robots.txt").send().await;
    assert_eq!(r.text(), "page");
    assert_eq!(r.header("cache-control"), Some("public, max-age=86400"));
    let r = c
        .get("/robots.txt")
        .header("range", "bytes=1-2")
        .send()
        .await;
    assert_eq!((r.status().as_u16(), r.text()), (206, "ag".into()));

    assert_eq!(c.get("/gone").send().await.status(), 404);

    let r = c.post("/any").send().await;
    assert_eq!(r.status(), 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD"));

    let r = c
        .get("/app.js")
        .header("accept-encoding", "br")
        .send()
        .await;
    assert_eq!(r.text(), "BROTLI-BYTES");
}

#[tokio::test]
async fn as_fallback_handler_and_nested() {
    let f = fixture();
    let c = App::new()
        .get("/api/ping", || async { "pong" })
        .fallback(ServeDir::new(&f.root))
        .test();
    assert_eq!(c.get("/api/ping").send().await.text(), "pong");
    assert_eq!(c.get("/").send().await.text(), "<h1>home</h1>");
    assert_eq!(c.get("/style.css").send().await.text(), "body{color:red}");
    let r = c.get("/sub").send().await;
    assert_eq!(r.status(), 307);
    assert_eq!(r.header("location"), Some("/sub/"));
    // Leading slashes can't turn the redirect into `//host/`.
    let r = c.get("//sub").send().await;
    assert_eq!(r.header("location"), Some("/sub/"));
    assert_not_served(&c, "/../secret.txt").await;

    // Mounted at the root next to API routes.
    let c = App::new()
        .get("/api/ping", || async { "pong" })
        .static_files("/", ServeDir::new(&f.root))
        .test();
    assert_eq!(c.get("/api/ping").send().await.text(), "pong");
    assert_eq!(c.get("/").send().await.text(), "<h1>home</h1>");
    assert_eq!(c.get("/sub/page.txt").send().await.text(), "page");

    // Nested route groups.
    let docs = Routes::new().static_files("/docs/", ServeDir::new(f.root.join("sub")));
    let c = App::new().nest("/v1", docs).test();
    assert_eq!(c.get("/v1/docs").send().await.text(), "<h1>sub</h1>");
    assert_eq!(c.get("/v1/docs/page.txt").send().await.text(), "page");
    let paths: Vec<String> = c
        .routes()
        .iter()
        .map(|r| format!("{} {}", r.method().unwrap(), r.path()))
        .collect();
    assert_eq!(
        paths,
        [
            "GET /v1/docs",
            "HEAD /v1/docs",
            "GET /v1/docs/{*path}",
            "HEAD /v1/docs/{*path}"
        ]
    );
    let meta = c.routes()[0].meta::<StaticFilesRoute>().unwrap();
    assert_eq!(meta.root, f.root.join("sub"));
}

/// GET `path` like a browser: follow redirects, giving up after 10 hops
/// (`ERR_TOO_MANY_REDIRECTS`). Returns the final response and every
/// `Location` visited.
async fn browse(c: &TestClient, path: &str) -> (TestResponse, Vec<String>) {
    let mut hops = Vec::new();
    let mut r = c.get(path).send().await;
    while r.status().is_redirection() {
        assert!(hops.len() < 10, "redirect loop: {path} -> {hops:?}");
        let next = r.header("location").unwrap().to_owned();
        r = c.get(&next).send().await;
        hops.push(next);
    }
    (r, hops)
}

#[tokio::test]
async fn directories_behind_normalize_path_never_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("docs");
    write(&root.join("index.html"), "<h1>home</h1>");
    write(&root.join("guide/index.html"), "<h1>guide</h1>");
    write(&root.join("guide/intro.txt"), "intro");
    let mounted = |mw: NormalizePath| {
        App::new()
            .wrap(mw)
            .static_files("/", ServeDir::new(&root))
            .test()
    };

    // Trimming by rewrite: `/guide/` becomes `/guide`, where the index is
    // served directly instead of redirecting back to `/guide/`.
    for c in [
        mounted(NormalizePath::default()),
        App::new()
            .wrap(NormalizePath::trim_trailing_slash())
            .fallback(ServeDir::new(&root))
            .test(),
    ] {
        for path in ["/guide", "/guide/", "/guide?x=1", "/guide/?x=1"] {
            let (r, hops) = browse(&c, path).await;
            assert_eq!(
                (r.status().as_u16(), r.text()),
                (200, "<h1>guide</h1>".into()),
                "{path}"
            );
            assert!(hops.is_empty(), "{path}: {hops:?}");
        }
        assert_eq!(c.get("/guide/intro.txt").send().await.text(), "intro");
        assert_eq!(c.get("/").send().await.text(), "<h1>home</h1>");
        assert_eq!(c.get("/missing").send().await.status(), 404);
    }

    // Trimming by redirect: one hop to the canonical `/guide`, which serves
    // the index.
    let c = mounted(NormalizePath::trim_trailing_slash().redirect(StatusCode::PERMANENT_REDIRECT));
    let r = c.get("/guide/?x=1").send().await;
    assert_eq!(r.status(), 308);
    assert_eq!(r.header("location"), Some("/guide?x=1"));
    for path in ["/guide", "/guide/"] {
        let (r, hops) = browse(&c, path).await;
        assert_eq!(
            (r.status().as_u16(), r.text()),
            (200, "<h1>guide</h1>".into()),
            "{path}"
        );
        assert!(hops.len() <= 1, "{path}: {hops:?}");
    }

    // Appending slashes agrees with `ServeDir` already: `/guide/` is the
    // canonical URL either way.
    let c = mounted(NormalizePath::append_trailing_slash());
    for path in ["/guide", "/guide/"] {
        let (r, hops) = browse(&c, path).await;
        assert_eq!(
            (r.status().as_u16(), r.text()),
            (200, "<h1>guide</h1>".into()),
            "{path}"
        );
        assert!(hops.is_empty(), "{path}: {hops:?}");
    }
    let c =
        mounted(NormalizePath::append_trailing_slash().redirect(StatusCode::PERMANENT_REDIRECT));
    let (r, hops) = browse(&c, "/guide").await;
    assert_eq!(r.text(), "<h1>guide</h1>");
    assert_eq!(hops, ["/guide/"]);
    assert_eq!(c.get("/guide/intro.txt").send().await.text(), "intro");

    // Without `NormalizePath`, directories still redirect to add the slash.
    let c = App::new().static_files("/", ServeDir::new(&root)).test();
    let (r, hops) = browse(&c, "/guide").await;
    assert_eq!(r.text(), "<h1>guide</h1>");
    assert_eq!(hops, ["/guide/"]);
}

#[tokio::test]
async fn missing_root_and_late_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("later");
    let c = client(ServeDir::new(&root));
    assert_eq!(c.get("/assets/a.txt").send().await.status(), 404);
    write(&root.join("a.txt"), "now");
    assert_eq!(c.get("/assets/a.txt").send().await.text(), "now");
}

#[test]
fn debug_output() {
    let d = ServeDir::new("public").precompressed_br().show_hidden(true);
    let s = format!("{d:?}");
    assert!(s.contains("public") && s.contains("br: true"), "{s}");
    assert_eq!(d.root(), Path::new("public"));
    assert_eq!(ServeFile::new("x.txt").path(), Path::new("x.txt"));
}

async fn serve(app: App) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

async fn raw(addr: SocketAddr, request: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&out).into_owned()
}
