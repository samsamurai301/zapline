//! File storage: memory, a local directory, and S3 when
//! `ZAPLINE_TEST_S3_ENDPOINT` (plus `ZAPLINE_TEST_S3_BUCKET`,
//! `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`) points at an
//! S3-compatible server such as MinIO. Set `ZAPLINE_TEST_S3_NO_PRESIGN` for
//! servers that can't check presigned links (moto).
#![cfg(feature = "storage")]

use std::time::Duration;

use zapline::prelude::*;
use zapline::storage::{unique_key, Storage, FILES_PATH};

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
    async fn bad_credentials_are_a_503_with_the_s3_code() {
        let Some(_) = storage() else { return };
        let s3 = S3::new("zapline-test", "us-east-1")
            .endpoint(std::env::var("ZAPLINE_TEST_S3_ENDPOINT").unwrap())
            .credentials("AKIANOTAREALKEY00000", "nope");
        let err = Storage::s3(s3).put("x.txt", "x").await.unwrap_err();
        assert_eq!(err.status(), 503, "{}", err.message());
    }
}
