//! File storage: memory, a local directory, and S3 when
//! `ZAPLINE_TEST_S3_ENDPOINT` (plus `ZAPLINE_TEST_S3_BUCKET`,
//! `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`) points at an
//! S3-compatible server such as MinIO. Set `ZAPLINE_TEST_S3_NO_PRESIGN` for
//! servers that can't check presigned links (moto).
#![cfg(all(feature = "storage", feature = "macros"))]

use std::time::Duration;

use zapline::prelude::*;
use zapline::storage::{unique_key, PutOptions, Storage, FILES_PATH};

const HOUR: Duration = Duration::from_secs(3600);

async fn round_trip(storage: &Storage) {
    let key = unique_key("docs", "Report 2026.txt");
    assert!(!storage.exists(&key).await.unwrap());
    storage.put(&key, "first").await.unwrap();
    storage.put(&key, "second").await.unwrap();
    assert!(storage.exists(&key).await.unwrap());
    assert_eq!(storage.get(&key).await.unwrap(), "second");

    storage.delete(&key).await.unwrap();
    storage.delete(&key).await.unwrap();
    assert!(!storage.exists(&key).await.unwrap());
    assert_eq!(storage.get(&key).await.unwrap_err().status(), 404);
}

#[tokio::test]
async fn memory_round_trip() {
    round_trip(&Storage::memory()).await;
}

#[tokio::test]
async fn local_round_trip_writes_files_under_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(dir.path());
    round_trip(&storage).await;

    storage.put("a/b/c.txt", "deep").await.unwrap();
    let on_disk = std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap();
    assert_eq!(on_disk, "deep");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("a/b"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers.len(), 1, "no temp files left: {leftovers:?}");
}

#[tokio::test]
async fn keys_cannot_escape() {
    let storage = Storage::memory();
    for key in [
        "",
        "/etc/passwd",
        "../x",
        "a/../../x",
        "a//b",
        "a\\b",
        "./a",
        "a/",
    ] {
        let err = storage.put(key, "x").await.unwrap_err();
        assert_eq!(err.status(), 400, "{key:?}");
        assert_eq!(err.code(), Some("invalid_key"));
    }
    let long = "a".repeat(1025);
    assert_eq!(storage.get(&long).await.unwrap_err().status(), 400);
}

#[test]
fn unique_keys_are_fresh_and_safe() {
    let a = unique_key("/avatars/", "C:\\Users\\me\\..\\photo (1).png");
    let b = unique_key("avatars", "photo (1).png");
    assert_ne!(a, b);
    assert!(
        a.starts_with("avatars/") && a.ends_with("/photo--1-.png"),
        "{a}"
    );
    assert!(unique_key("", "...").ends_with("/file"));
}

#[tokio::test]
async fn the_app_serves_signed_links_only() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(dir.path());
    let client = App::new().storage(storage.clone()).test();
    storage
        .put("avatars/1.png", vec![0x89, b'P', b'N', b'G'])
        .await
        .unwrap();

    let link = storage.url("avatars/1.png", HOUR);
    assert!(link.starts_with(&format!("{FILES_PATH}/avatars/1.png?expires=")));
    let res = client.get(&link).send().await;
    res.assert_status(200)
        .assert_header("content-type", "image/png")
        .assert_header("x-content-type-options", "nosniff");
    assert_eq!(res.bytes().as_ref(), [0x89, b'P', b'N', b'G']);

    // No signature, a tampered one, another file's, or an expired link.
    client
        .get("/files/avatars/1.png")
        .send()
        .await
        .assert_status(403);
    let tampered = format!("{}0", &link[..link.len() - 1]);
    let tampered = if tampered == link {
        link.replace("sig=", "sig=1")
    } else {
        tampered
    };
    client.get(&tampered).send().await.assert_status(403);
    let other = link.replace("avatars/1.png", "avatars/2.png");
    client.get(&other).send().await.assert_status(403);
    let expired = storage
        .url("avatars/1.png", Duration::ZERO)
        .replace("expires=", "expires=1");
    client.get(&expired).send().await.assert_status(403);

    // A valid link to a missing file is a 404.
    let missing = storage.url("avatars/9.png", HOUR);
    client.get(&missing).send().await.assert_status(404);
}

#[tokio::test]
async fn uploads_never_run_as_pages_on_the_app_origin() {
    let storage = Storage::memory();
    let client = App::new().storage(storage.clone()).test();
    for key in ["x.html", "x.svg", "x.xml", "x.js"] {
        storage.put(key, "<script>alert(1)</script>").await.unwrap();
        let res = client.get(&storage.url(key, HOUR)).send().await;
        res.assert_status(200)
            .assert_header("content-disposition", "attachment");
        let csp = res.headers()["content-security-policy"].to_str().unwrap();
        assert!(
            csp.contains("sandbox") && csp.contains("default-src 'none'"),
            "{csp}"
        );
    }
    // Images and the like still show inline, sandboxed.
    storage.put("a.png", vec![0x89, b'P']).await.unwrap();
    let res = client.get(&storage.url("a.png", HOUR)).send().await;
    assert!(res.headers().get("content-disposition").is_none());
    assert!(res.headers().contains_key("content-security-policy"));
}

#[tokio::test]
async fn a_shared_secret_keeps_links_valid_across_instances() {
    let first = Storage::memory().secret("s3cr3t");
    let second = Storage::memory().secret("s3cr3t");
    second.put("a.txt", "hi").await.unwrap();
    let client = App::new().storage(second).test();
    let res = client.get(&first.url("a.txt", HOUR)).send().await;
    res.assert_status(200)
        .assert_header("content-type", "text/plain");
    assert_eq!(res.text(), "hi");

    let stranger = Storage::memory();
    client
        .get(&stranger.url("a.txt", HOUR))
        .send()
        .await
        .assert_status(403);
}

#[tokio::test]
async fn handlers_take_storage_as_an_argument() {
    #[post("/notes/{name}")]
    async fn save(storage: Storage, Path(name): Path<String>, body: String) -> Result<String> {
        let key = unique_key("notes", &name);
        storage.put(&key, body).await?;
        Ok(storage.url(&key, Duration::from_secs(60)))
    }

    let client = App::new().storage(Storage::memory()).mount(save).test();
    let res = client.post("/notes/todo.txt").body("buy milk").send().await;
    res.assert_status(200);
    let link = res.text();
    assert_eq!(client.get(&link).send().await.text(), "buy milk");

    // Without `.storage(..)` the extractor explains what is missing.
    let client = App::new().mount(save).test();
    let res = client.post("/notes/x").body("y").send().await;
    res.assert_status(500);
}

/// `list`, `metadata`, `copy` and `put_with`, the same on every backend.
async fn listing_copy_and_metadata(storage: &Storage) {
    let root = unique_key("t", "x");
    let root = root.rsplit_once('/').unwrap().0; // a fresh "t/<random>"
    let key = |name: &str| format!("{root}/{name}");
    storage.put(&key("a/1.png"), "one").await.unwrap();
    storage.put(&key("a/2.txt"), "two!").await.unwrap();
    storage.put(&key("a/sub/3.txt"), "3").await.unwrap();
    storage.put(&key("ab.txt"), "ab").await.unwrap();
    storage.put(&key("b/4.txt"), "4").await.unwrap();

    let names = |files: Vec<zapline::storage::ObjectInfo>| -> Vec<String> {
        files
            .into_iter()
            .map(|f| f.key[root.len() + 1..].to_owned())
            .collect()
    };
    assert_eq!(
        names(storage.list(&key("a/")).await.unwrap()),
        ["a/1.png", "a/2.txt", "a/sub/3.txt"]
    );
    // A prefix needn't end at a slash.
    assert_eq!(
        names(storage.list(&key("a")).await.unwrap()),
        ["a/1.png", "a/2.txt", "a/sub/3.txt", "ab.txt"]
    );
    assert_eq!(
        names(storage.list(&format!("{root}/")).await.unwrap()).len(),
        5
    );
    assert!(storage.list(&key("nothing/")).await.unwrap().is_empty());
    let listed = storage.list(&key("a/2")).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].size, 4);
    let age = std::time::SystemTime::now()
        .duration_since(listed[0].modified)
        .unwrap_or_default();
    assert!(age < Duration::from_secs(600), "modified {age:?} ago");

    let info = storage.metadata(&key("a/1.png")).await.unwrap();
    assert_eq!(info.size, 3);
    assert_eq!(info.content_type.as_deref(), Some("image/png"));
    assert_eq!(info.cache_control, None);
    assert_eq!(
        storage
            .metadata(&key("missing"))
            .await
            .unwrap_err()
            .status(),
        404
    );

    // Copies keep the content type and cache control given at upload.
    let options = PutOptions::new()
        .content_type("application/vnd.acme+json")
        .cache_control("private, max-age=60");
    storage
        .put_with(&key("data.bin"), r#"{"v":1}"#, options)
        .await
        .unwrap();
    storage
        .copy(&key("data.bin"), &key("copies/data.bin"))
        .await
        .unwrap();
    assert_eq!(
        storage.get(&key("copies/data.bin")).await.unwrap(),
        r#"{"v":1}"#
    );
    let info = storage.metadata(&key("copies/data.bin")).await.unwrap();
    assert_eq!(
        info.content_type.as_deref(),
        Some("application/vnd.acme+json")
    );
    assert_eq!(info.cache_control.as_deref(), Some("private, max-age=60"));
    // A plain put over it forgets them.
    storage.put(&key("copies/data.bin"), "x").await.unwrap();
    let info = storage.metadata(&key("copies/data.bin")).await.unwrap();
    assert_eq!(
        info.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(info.cache_control, None);

    assert_eq!(
        storage
            .copy(&key("missing"), &key("x"))
            .await
            .unwrap_err()
            .status(),
        404
    );
    for name in [
        "a/1.png",
        "a/2.txt",
        "a/sub/3.txt",
        "ab.txt",
        "b/4.txt",
        "data.bin",
        "copies/data.bin",
    ] {
        storage.delete(&key(name)).await.unwrap();
    }
    assert!(storage.list(&format!("{root}/")).await.unwrap().is_empty());
}

/// `get_stream` and `put_stream` move the same bytes as `get` and `put`.
async fn streams(storage: &Storage) {
    use futures_util::stream;
    use http_body_util::BodyExt;

    let key = unique_key("streams", "big.bin");
    let chunks: Vec<Result<Bytes, std::io::Error>> = (0..40u8)
        .map(|i| Ok(Bytes::from(vec![i; 100_000])))
        .collect();
    let size = storage
        .put_stream(&key, stream::iter(chunks), PutOptions::new())
        .await
        .unwrap();
    assert_eq!(size, 4_000_000);
    let whole = storage.get(&key).await.unwrap();
    assert_eq!(whole.len(), 4_000_000);
    assert!(whole[..100_000].iter().all(|&b| b == 0));
    assert!(whole[3_900_000..].iter().all(|&b| b == 39));

    let streamed = storage
        .get_stream(&key)
        .await
        .unwrap()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(streamed, whole);
    assert_eq!(
        storage
            .get_stream("streams/missing")
            .await
            .err()
            .unwrap()
            .status(),
        404
    );

    // A failing stream stores nothing and its error comes back as is.
    let failing = stream::iter([
        Ok(Bytes::from_static(b"partial")),
        Err(Error::payload_too_large()),
    ]);
    let other = unique_key("streams", "failed.bin");
    let err = storage
        .put_stream(&other, failing, PutOptions::new())
        .await
        .unwrap_err();
    assert_eq!(err.status(), 413);
    assert!(!storage.exists(&other).await.unwrap());
    storage.delete(&key).await.unwrap();
}

#[tokio::test]
async fn memory_lists_copies_and_streams() {
    let storage = Storage::memory();
    listing_copy_and_metadata(&storage).await;
    streams(&storage).await;
}

#[tokio::test]
async fn local_lists_copies_and_streams() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(dir.path());
    listing_copy_and_metadata(&storage).await;
    streams(&storage).await;

    // Temporary and metadata files never show up as keys, symlinks aren't
    // followed, and no key can name the backend's own files.
    storage
        .put_with("m/a.txt", "a", PutOptions::new().content_type("text/x-a"))
        .await
        .unwrap();
    std::fs::write(dir.path().join("m/.zl-tmp.leftover"), "x").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", dir.path().join("m/etc")).unwrap();
    let keys: Vec<String> = storage
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.key)
        .collect();
    assert_eq!(keys, ["m/a.txt"]);
    for key in ["m/.zl-meta.a.txt", ".zl-tmp.x"] {
        assert_eq!(
            storage.get(key).await.unwrap_err().code(),
            Some("invalid_key")
        );
    }
    for prefix in ["/", "../", "a//", "a/../", ".zl-"] {
        let err = storage.list(prefix).await.unwrap_err();
        assert_eq!(err.status(), 400, "{prefix:?}");
    }
}

#[tokio::test]
async fn served_files_keep_the_type_and_caching_they_were_stored_with() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(dir.path());
    let client = App::new().storage(storage.clone()).test();
    let options = PutOptions::new()
        .content_type("image/png")
        .cache_control("public, max-age=31536000, immutable");
    storage
        .put_with("logo", vec![0x89, b'P', b'N', b'G'], options)
        .await
        .unwrap();
    let res = client.get(&storage.url("logo", HOUR)).send().await;
    res.assert_status(200)
        .assert_header("content-type", "image/png")
        .assert_header("cache-control", "public, max-age=31536000, immutable");
    assert!(res.headers().get("content-disposition").is_none());

    // Declaring HTML doesn't make it run on the app's origin.
    let options = PutOptions::new().content_type("text/html");
    storage
        .put_with("page", "<script>1</script>", options)
        .await
        .unwrap();
    let res = client.get(&storage.url("page", HOUR)).send().await;
    res.assert_header("content-disposition", "attachment");
    assert!(res.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("sandbox"));

    let bad = PutOptions::new().content_type("text/html\r\nX-Evil: 1");
    assert_eq!(
        storage.put_with("x", "x", bad).await.unwrap_err().status(),
        400
    );
}

#[tokio::test]
async fn signed_uploads_check_signature_size_type_and_expiry() {
    let dir = tempfile::tempdir().unwrap();
    for storage in [Storage::memory(), Storage::local(dir.path())] {
        let client = App::new().storage(storage.clone()).test();
        let up = storage
            .upload_url("avatars/7.png", HOUR, 8, "image/png")
            .unwrap();
        assert_eq!(up.method, "PUT");
        assert_eq!(up.key, "avatars/7.png");
        assert_eq!(up.headers["Content-Type"], "image/png");
        assert!(
            up.url.starts_with("/files/avatars/7.png?expires="),
            "{}",
            up.url
        );

        // Accepted: right type, within the size.
        client
            .put(&up.url)
            .header("content-type", "image/png")
            .body(vec![0x89, b'P', b'N', b'G'])
            .send()
            .await
            .assert_status(200);
        assert_eq!(storage.get("avatars/7.png").await.unwrap().len(), 4);
        let info = storage.metadata("avatars/7.png").await.unwrap();
        assert_eq!(info.content_type.as_deref(), Some("image/png"));
        // ...and readable through a normal link.
        client
            .get(&storage.url("avatars/7.png", HOUR))
            .send()
            .await
            .assert_status(200)
            .assert_header("content-type", "image/png");

        // Too large: refused, and the stored file is untouched.
        let res = client
            .put(&up.url)
            .header("content-type", "image/png")
            .body(vec![0; 9])
            .send()
            .await;
        res.assert_status(413);
        assert_eq!(storage.get("avatars/7.png").await.unwrap().len(), 4);

        // The wrong type.
        client
            .put(&up.url)
            .header("content-type", "text/html")
            .body("<b>")
            .send()
            .await
            .assert_status(415);

        // Tampered: another key, a bigger limit, another type, no signature.
        for tampered in [
            up.url.replace("avatars/7.png", "avatars/8.png"),
            up.url.replace("max=8", "max=9999"),
            up.url.replace("type=image%2Fpng", "type=text%2Fhtml"),
            up.url.split("&sig=").next().unwrap().to_owned(),
        ] {
            client
                .put(&tampered)
                .header("content-type", "image/png")
                .body("x")
                .send()
                .await
                .assert_status(403);
        }

        // A download link is not an upload link, and vice versa.
        let download = storage.url("avatars/7.png", HOUR);
        client
            .put(&download)
            .header("content-type", "image/png")
            .body("x")
            .send()
            .await
            .assert_status(403);
        client.get(&up.url).send().await.assert_status(403);

        // Expired.
        let expired = storage
            .upload_url("avatars/9.png", Duration::ZERO, 8, "image/png")
            .unwrap();
        let expired_url = expired.url.replace("expires=", "expires=1");
        client
            .put(&expired_url)
            .header("content-type", "image/png")
            .body("x")
            .send()
            .await
            .assert_status(403);
        assert!(!storage.exists("avatars/9.png").await.unwrap());
    }

    let storage = Storage::memory();
    assert_eq!(
        storage
            .upload_url("../x", HOUR, 1, "image/png")
            .unwrap_err()
            .status(),
        400
    );
    assert_eq!(
        storage
            .upload_url("x", HOUR, 1, "not a type")
            .unwrap_err()
            .status(),
        400
    );
}

#[cfg(feature = "json")]
#[test]
fn upload_urls_serialize_for_the_browser() {
    let up = Storage::memory()
        .upload_url("a.png", HOUR, 10, "image/png")
        .unwrap();
    let json = serde_json::to_value(&up).unwrap();
    assert_eq!(json["method"], "PUT");
    assert_eq!(json["headers"]["Content-Type"], "image/png");
    assert_eq!(json["max_bytes"], 10);
    assert!(json["url"].as_str().unwrap().contains("&sig="));
}

#[tokio::test]
async fn handlers_can_return_a_stored_stream() {
    #[get("/download/{name}")]
    async fn download(storage: Storage, Path(name): Path<String>) -> Result<zapline::Body> {
        storage.get_stream(&format!("exports/{name}")).await
    }
    let storage = Storage::memory();
    storage.put("exports/a.csv", "id\n1\n").await.unwrap();
    let client = App::new().storage(storage).mount(download).test();
    let res = client.get("/download/a.csv").send().await;
    res.assert_status(200);
    assert_eq!(res.text(), "id\n1\n");
    client
        .get("/download/missing.csv")
        .send()
        .await
        .assert_status(404);
}

#[cfg(feature = "s3")]
mod s3 {
    use super::*;
    use http_body_util::{BodyExt, Empty};
    use hyper::body::Bytes;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;
    use zapline::storage::S3;

    fn storage() -> Option<Storage> {
        let endpoint = std::env::var("ZAPLINE_TEST_S3_ENDPOINT").ok()?;
        let bucket =
            std::env::var("ZAPLINE_TEST_S3_BUCKET").unwrap_or_else(|_| "zapline-test".into());
        let s3 = S3::new(bucket, "us-east-1").endpoint(endpoint).credentials(
            std::env::var("AWS_ACCESS_KEY_ID").unwrap(),
            std::env::var("AWS_SECRET_ACCESS_KEY").unwrap(),
        );
        Some(Storage::s3(s3))
    }

    #[tokio::test]
    async fn s3_round_trip_and_presigned_links() {
        let Some(storage) = storage() else {
            eprintln!("skipped: ZAPLINE_TEST_S3_ENDPOINT is not set");
            return;
        };
        round_trip(&storage).await;

        let key = unique_key("s3 test", "naïve file.json");
        storage.put(&key, r#"{"ok":true}"#).await.unwrap();
        let link = storage.url(&key, Duration::from_secs(60));
        assert!(link.contains("X-Amz-Signature="), "{link}");
        if std::env::var_os("ZAPLINE_TEST_S3_NO_PRESIGN").is_some() {
            // moto's signature checking doesn't cover presigned links; the
            // signing itself is covered by unit tests against AWS and botocore.
            storage.delete(&key).await.unwrap();
            return;
        }

        let client = Client::builder(TokioExecutor::new()).build_http::<Empty<Bytes>>();
        let res = client.get(link.parse().unwrap()).await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers()["content-type"],
            "application/json",
            "the content type is stored with the object"
        );
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, r#"{"ok":true}"#);

        // A tampered link is refused by S3.
        let bad = link.replace(&key.replace(' ', "%20"), "other");
        let res = client.get(bad.parse().unwrap()).await.unwrap();
        assert_eq!(res.status(), 403);
        storage.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn s3_lists_copies_streams_and_takes_multipart_uploads() {
        let Some(storage) = storage() else {
            eprintln!("skipped: ZAPLINE_TEST_S3_ENDPOINT is not set");
            return;
        };
        listing_copy_and_metadata(&storage).await;
        streams(&storage).await;

        // Over one 8 MiB part: a multipart upload, in order.
        use futures_util::stream;
        use http_body_util::BodyExt;
        let key = unique_key("s3 multipart", "big file.bin");
        let chunks = (0..20u8).map(|i| Ok::<_, std::io::Error>(Bytes::from(vec![i; 1 << 20])));
        let options = PutOptions::new().content_type("application/x-test");
        let size = storage
            .put_stream(&key, stream::iter(chunks), options)
            .await
            .unwrap();
        assert_eq!(size, 20 << 20);
        let info = storage.metadata(&key).await.unwrap();
        assert_eq!(info.size, 20 << 20);
        assert_eq!(info.content_type.as_deref(), Some("application/x-test"));
        let body = storage
            .get_stream(&key)
            .await
            .unwrap()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(body.len(), 20 << 20);
        for i in 0..20u8 {
            assert_eq!(body[usize::from(i) << 20], i);
        }
        // A failure midway aborts the upload: nothing is stored.
        let failing = stream::iter((0..10u8).map(|i| {
            if i < 9 {
                Ok(Bytes::from(vec![i; 1 << 20]))
            } else {
                Err(Error::payload_too_large())
            }
        }));
        let other = unique_key("s3 multipart", "aborted.bin");
        let err = storage
            .put_stream(&other, failing, PutOptions::new())
            .await
            .unwrap_err();
        assert_eq!(err.status(), 413);
        assert!(!storage.exists(&other).await.unwrap());
        storage.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn s3_presigned_uploads() {
        let Some(storage) = storage() else { return };
        let key = unique_key("s3 uploads", "photo.png");
        let up = storage
            .upload_url(&key, Duration::from_secs(60), 1 << 20, "image/png")
            .unwrap();
        assert!(up.url.contains("X-Amz-SignedHeaders=content-type%3Bhost"));
        let client =
            Client::builder(TokioExecutor::new()).build_http::<http_body_util::Full<Bytes>>();
        let req = hyper::Request::put(&up.url)
            .header("content-type", "image/png")
            .body(http_body_util::Full::new(Bytes::from_static(b"\x89PNG")))
            .unwrap();
        let res = client.request(req).await.unwrap();
        assert_eq!(res.status(), 200);
        let info = storage.metadata(&key).await.unwrap();
        assert_eq!(
            (info.size, info.content_type.as_deref()),
            (4, Some("image/png"))
        );
        if std::env::var_os("ZAPLINE_TEST_S3_NO_PRESIGN").is_none() {
            // S3 checks the signed content type.
            let req = hyper::Request::put(&up.url)
                .header("content-type", "text/html")
                .body(http_body_util::Full::new(Bytes::from_static(b"<b>")))
                .unwrap();
            assert_eq!(client.request(req).await.unwrap().status(), 403);
        }
        storage.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn bad_credentials_are_a_503_with_the_s3_code() {
        let Some(_) = storage() else { return };
        let s3 = S3::new("zapline-test", "us-east-1")
            .endpoint(std::env::var("ZAPLINE_TEST_S3_ENDPOINT").unwrap())
            .credentials("AKIANOTAREALKEY00000", "nope");
        let err = Storage::s3(s3).put("x.txt", "x").await.unwrap_err();
        assert_eq!(err.status(), 503, "{}", err.message());
    }
}
