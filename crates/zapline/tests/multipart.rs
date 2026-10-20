//! Multipart form data: fields, file uploads, limits and error mapping.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapline::multipart::{Multipart, MultipartConfig};
use zapline::prelude::*;

const BOUNDARY: &str = "X-ZAPLINE-BOUNDARY";

/// A part: (name, optional (filename, content type), data).
type Part<'a> = (&'a str, Option<(&'a str, &'a str)>, &'a [u8]);

fn body(parts: &[Part<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, file, data) in parts {
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match file {
            Some((filename, ct)) => out.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {ct}\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => out.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

fn content_type() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

/// Describes every field: `name|file|type|size|text-or-dash`.
async fn describe(mut form: Multipart) -> Result<String> {
    let mut lines = Vec::new();
    while let Some(field) = form.next_field().await? {
        let name = field.name().unwrap_or("?").to_owned();
        let file = field.file_name().unwrap_or("-").to_owned();
        let ct = field.content_type().unwrap_or("-").to_owned();
        let index = field.index();
        let data = field.bytes().await?;
        let text = std::str::from_utf8(&data).unwrap_or("<binary>");
        lines.push(format!("{index}:{name}|{file}|{ct}|{}|{text}", data.len()));
    }
    Ok(lines.join("\n"))
}

fn error_code(r: &zapline::test::TestResponse) -> String {
    r.json::<serde_json::Value>()["code"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

#[tokio::test]
async fn text_fields_and_file_upload() {
    let c = App::new().post("/upload", describe).test();
    let payload = body(&[
        ("title", None, b"Holiday"),
        ("note", None, "caf\u{e9} \r\n second line".as_bytes()),
        (
            "photo",
            Some(("beach.png", "image/png")),
            &[0x89, b'P', b'N', b'G', 0, 255],
        ),
        ("empty", None, b""),
    ]);
    let r = c
        .post("/upload")
        .header("content-type", content_type())
        .body(payload)
        .send()
        .await;
    assert_eq!(r.status(), 200, "{}", r.text());
    assert_eq!(
        r.text(),
        "0:title|-|-|7|Holiday\n\
         1:note|-|-|20|caf\u{e9} \r\n second line\n\
         2:photo|beach.png|image/png|6|<binary>\n\
         3:empty|-|-|0|"
    );
}

#[tokio::test]
async fn chunks_text_json_and_save_to() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().to_owned();
    let c = App::new()
        .post("/", move |mut form: Multipart| {
            let target = target.clone();
            async move {
                let mut out = Vec::new();
                while let Some(mut field) = form.next_field().await? {
                    match field.name() {
                        Some("meta") => {
                            let v: serde_json::Value = field.json().await?;
                            out.push(format!("meta.id={}", v["id"]));
                        }
                        Some("text") => out.push(format!("text={}", field.text().await?)),
                        Some("upload") => {
                            let name = field.safe_file_name().unwrap();
                            let n = field.save_to(target.join(&name)).await?;
                            out.push(format!("saved {name} ({n})"));
                        }
                        _ => {
                            let mut n = 0;
                            while let Some(chunk) = field.chunk().await? {
                                n += chunk.len();
                            }
                            out.push(format!("chunked={n}"));
                        }
                    }
                }
                Ok::<_, Error>(out.join(","))
            }
        })
        .test();

    let big = vec![b'z'; 100_000];
    let payload = body(&[
        ("meta", None, br#"{"id": 7}"#),
        ("text", None, b"hello"),
        (
            "upload",
            Some(("../../evil/report.txt", "text/plain")),
            b"file body",
        ),
        ("blob", Some(("blob.bin", "application/octet-stream")), &big),
    ]);
    let r = c
        .post("/")
        .header("content-type", content_type())
        .body(payload)
        .send()
        .await;
    assert_eq!(r.status(), 200, "{}", r.text());
    assert_eq!(
        r.text(),
        "meta.id=7,text=hello,saved report.txt (9),chunked=100000"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("report.txt")).unwrap(),
        "file body"
    );

    // Invalid JSON in a field is a 422.
    let r = c
        .post("/")
        .header("content-type", content_type())
        .body(body(&[("meta", None, b"{nope")]))
        .send()
        .await;
    assert_eq!(r.status(), 422);
}

#[tokio::test]
async fn content_type_errors() {
    let c = App::new().post("/", describe).test();

    let r = c
        .post("/")
        .header("content-type", "multipart/form-data")
        .body(body(&[("a", None, b"1")]))
        .send()
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(error_code(&r), "multipart_missing_boundary");

    for ct in [
        None,
        Some("application/json"),
        Some("text/plain; boundary=x"),
        Some("garbage"),
    ] {
        let mut req = c.post("/").body(body(&[("a", None, b"1")]));
        if let Some(ct) = ct {
            req = req.header("content-type", ct);
        }
        let r = req.send().await;
        assert_eq!(r.status(), 415, "{ct:?}");
        assert_eq!(error_code(&r), "multipart_unsupported_media_type");
    }
}

#[tokio::test]
async fn malformed_bodies_are_400() {
    let c = App::new().post("/", describe).test();
    for bad in [
        // No closing boundary.
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nvalue"),
        // Wrong boundary entirely.
        "--other\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nv\r\n--other--\r\n"
            .to_owned(),
        // Headers never terminated.
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"a\""),
    ] {
        let r = c
            .post("/")
            .header("content-type", content_type())
            .body(bad.clone())
            .send()
            .await;
        assert_eq!(r.status(), 400, "{bad:?} -> {}", r.text());
    }
}

#[tokio::test]
async fn body_limit_is_enforced() {
    let c = App::new().body_limit(1024).post("/", describe).test();
    let ok = body(&[("a", None, &[b'x'; 500])]);
    let r = c
        .post("/")
        .header("content-type", content_type())
        .body(ok)
        .send()
        .await;
    assert_eq!(r.status(), 200);

    // Streaming past the limit.
    let big = body(&[("a", None, &[b'x'; 2000])]);
    let r = c
        .post("/")
        .header("content-type", content_type())
        .body(big)
        .send()
        .await;
    assert_eq!(r.status(), 413);

    // A declared Content-Length over the limit is rejected up front.
    let r = c
        .post("/")
        .header("content-type", content_type())
        .header("content-length", "999999")
        .body(body(&[("a", None, b"1")]))
        .send()
        .await;
    assert_eq!(r.status(), 413);
}

#[tokio::test]
async fn config_from_state_and_route_meta() {
    let c = App::new()
        .body_limit(1024)
        .state(
            MultipartConfig::new()
                .max_field_size(100)
                .field_limit("bio", 10)
                .max_fields(3),
        )
        .post("/", describe)
        .post("/strict", describe)
        .meta(MultipartConfig::new().allowed_fields(["name"]))
        .post("/big", describe)
        .meta(MultipartConfig::new().body_limit(64 * 1024))
        .test();
    let send = |path: &'static str, parts: Vec<u8>| {
        c.post(path)
            .header("content-type", content_type())
            .body(parts)
    };

    let r = send("/", body(&[("a", None, &[b'x'; 101])])).send().await;
    assert_eq!(r.status(), 413);
    assert_eq!(error_code(&r), "multipart_field_too_large");

    let r = send("/", body(&[("bio", None, b"0123456789!")]))
        .send()
        .await;
    assert_eq!(r.status(), 413);
    assert!(r.text().contains("`bio`"), "{}", r.text());
    let r = send("/", body(&[("bio", None, b"short")])).send().await;
    assert_eq!(r.status(), 200);

    let r = send(
        "/",
        body(&[
            ("a", None, b"1"),
            ("b", None, b"2"),
            ("c", None, b"3"),
            ("d", None, b"4"),
        ]),
    )
    .send()
    .await;
    assert_eq!(r.status(), 413);
    assert_eq!(error_code(&r), "multipart_too_many_fields");

    // Route metadata takes precedence over app state.
    let r = send("/strict", body(&[("name", None, b"ok")])).send().await;
    assert_eq!(r.status(), 200);
    let r = send("/strict", body(&[("other", None, b"no")]))
        .send()
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(error_code(&r), "multipart_unknown_field");

    // A per-route body limit above the app's.
    let r = send(
        "/big",
        body(&[("file", Some(("a.bin", "x/y")), &[b'x'; 10_000])]),
    )
    .send()
    .await;
    assert_eq!(r.status(), 200, "{}", r.text());
}

#[tokio::test]
async fn upload_over_a_real_socket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(
        App::new()
            .body_limit(4096)
            .post("/", describe)
            .serve(listener),
    );

    async fn exchange(addr: SocketAddr, head: String, payload: &[u8]) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(head.as_bytes()).await.unwrap();
        // Write the body in small pieces so the parser sees several chunks.
        for piece in payload.chunks(7) {
            s.write_all(piece).await.unwrap();
        }
        let mut out = String::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut out))
            .await
            .unwrap()
            .ok();
        out
    }
    let head = |len: usize| {
        format!(
            "POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: {}\r\nContent-Length: {len}\r\n\r\n",
            content_type()
        )
    };

    let payload = body(&[
        ("a", None, b"hello"),
        ("f", Some(("f.txt", "text/plain")), b"data"),
    ]);
    let out = exchange(addr, head(payload.len()), &payload).await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(
        out.ends_with("0:a|-|-|5|hello\n1:f|f.txt|text/plain|4|data"),
        "{out}"
    );

    // Rejected from the declared length alone, before any body is read.
    let out = exchange(addr, head(5000), b"").await;
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
}
