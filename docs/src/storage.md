# File storage

`zapline::storage` keeps files under string keys like `avatars/42/me.png`. The same code works with a local directory while developing, memory in tests, and S3 (AWS, Cloudflare R2, MinIO, Backblaze B2) in production.

```rust
use std::time::Duration;
use zapline::multipart::Multipart;
use zapline::prelude::*;
use zapline::storage::{unique_key, Storage};

#[post("/avatar")]
async fn upload(storage: Storage, mut form: Multipart) -> Result<String> {
    let Some(file) = form.next_field().await? else {
        return Err(Error::bad_request("no file"));
    };
    let key = unique_key("avatars", file.file_name().unwrap_or("upload"));
    storage.put(&key, file.bytes().await?).await?;
    Ok(storage.url(&key, Duration::from_secs(3600)))
}

fn main() -> zapline::Result<()> {
    App::new()
        .storage(Storage::from_env()?)
        .mount(upload)
        .run("0.0.0.0:8080")?;
    Ok(())
}
```

Requirements: the `storage` feature (included in `full`), plus `s3` for S3.

## The API

| Call | Does |
|---|---|
| `storage.put(key, bytes)` | Store (or replace) a file. The content type comes from the key's extension. Local writes are atomic. |
| `storage.put_with(key, bytes, PutOptions::new().content_type(..).cache_control(..))` | The same, with the content type and `Cache-Control` you choose. |
| `storage.put_stream(key, stream, options)` | Store a stream of chunks (for example a request's `BodyStream`) without holding it in memory. Returns the size. |
| `storage.get(key)` | The bytes, or a `404` error. |
| `storage.get_stream(key)` | A `Body` that streams the file (64 KiB chunks from disk, straight from S3): return it from a handler. |
| `storage.exists(key)` | `true` or `false`. |
| `storage.metadata(key)` | `ObjectInfo { key, size, modified, content_type, cache_control }`, without reading the file. |
| `storage.list(prefix)` | Every file whose key starts with `prefix`, sorted, as `ObjectInfo`s (`""` lists everything). |
| `storage.copy(from, to)` | Copy a file with its content type (inside the bucket on S3, nothing downloaded). |
| `storage.delete(key)` | Remove it. Removing a missing file is fine. |
| `storage.url(key, ttl)` | A link that works for `ttl`. |
| `storage.upload_url(key, ttl, max_bytes, content_type)` | A URL the browser uploads one file to directly ([below](#direct-uploads-from-the-browser)). |
| `unique_key(prefix, file_name)` | `prefix/<random>/<safe-name>`: a fresh key for an upload, with a name that can't escape the prefix. |

Invalid keys (empty segments, `..`, a leading `/`, backslashes, a segment starting with `.zl-`, over 1024 bytes) are a `400` with code `invalid_key`, so a key built from user input can't reach outside the storage. Prefixes given to `list` are checked the same way. The local backend keeps its temporary files and the options of `put_with` next to each file under `.zl-` names, which never appear in listings; listings don't follow symbolic links.

## Links are private

Files aren't public. `storage.url` makes a link that expires:

- **S3**: a presigned URL (at most 7 days), straight to the bucket.
- **Local and memory**: `/files/<key>?expires=…&sig=…`, served by the app (`App::storage` mounts it) only after the HMAC signature and the expiry check out. Anything else is a `403`. Files are streamed with the content type and `Cache-Control` they were stored with (default `private, max-age=300`), `X-Content-Type-Options: nosniff` and a sandboxing `Content-Security-Policy`; anything but images, PDFs, plain text, audio and video downloads instead of opening, whatever its declared type.

Local links are signed with a random key per process, so they stop working at restart. Set `STORAGE_SECRET` (or call `.secret(..)`) to keep them valid, and use the same value on every instance.

## Large files

`get_stream` and `put_stream` move a file without loading it into memory, so a handler can pass a multi-gigabyte upload straight through:

```rust
use zapline::storage::{unique_key, PutOptions, Storage};
use zapline::stream::BodyStream;
use zapline::Body;

#[put("/videos/{name}")]
async fn upload(storage: Storage, Path(name): Path<String>, body: BodyStream) -> Result<String> {
    let key = unique_key("videos", &name);
    let options = PutOptions::new().content_type("video/mp4");
    storage.put_stream(&key, body.with_limit(2 << 30), options).await?;
    Ok(key)
}

#[get("/exports/{name}")]
async fn export(storage: Storage, Path(name): Path<String>) -> Result<Body> {
    storage.get_stream(&format!("exports/{name}")).await
}
```

Locally the stream goes to a temporary file that replaces the old one only when complete. On S3 anything over 8 MiB becomes a multipart upload in 8 MiB parts; if the stream fails (a `BodyStream` over its limit, a dropped connection) the upload is aborted, nothing is stored, and the stream's error (the `413`, say) is what `put_stream` returns.

## Direct uploads from the browser

`upload_url` lets the browser send a file to storage without it passing through your handlers. The handler only decides the key, the size limit and the type:

```rust
use zapline::storage::{unique_key, Storage, UploadUrl};

#[post("/avatar/upload-url")]
async fn avatar_upload_url(storage: Storage, user: CurrentUser) -> Result<Json<UploadUrl>> {
    let key = unique_key(&format!("avatars/{}", user.0.id), "avatar.png");
    let up = storage.upload_url(&key, Duration::from_secs(600), 5 << 20, "image/png")?;
    Ok(Json(up))
}
```

```js
const up = await (await fetch("/avatar/upload-url", { method: "POST" })).json();
await fetch(up.url, { method: up.method, headers: up.headers, body: file });
// then tell the app that up.key is ready
```

- **S3**: a presigned `PUT` (at most 7 days) that signs the content type, so S3 refuses any other. S3 can't cap the size of a presigned `PUT`: when the browser reports success, check `storage.metadata(&key)` and delete what is too large.
- **Local and memory**: a `PUT` to `/files/<key>?expires=…&max=…&type=…&sig=…`, which `App::storage` serves. The HMAC signature covers the key, the expiry, the size limit and the type, so none of them can be changed. It answers `403` for a bad or expired signature (download links don't work for uploads, nor the other way round), `415` for another `Content-Type`, and `413` beyond `max_bytes`, checked against `Content-Length` and again while the body streams to disk.

## Configuration

`Storage::from_env()` picks S3 when `S3_BUCKET` is set, otherwise a local directory:

| Variable | Default | |
|---|---|---|
| `STORAGE_DIR` | `storage` | Local directory (created as needed). |
| `STORAGE_SECRET` | random | Signs local links. |
| `S3_BUCKET` | | Turns on S3. |
| `S3_REGION` / `AWS_REGION` | `us-east-1` | Use `auto` for R2. |
| `S3_ENDPOINT` | AWS | For S3-compatible services, e.g. `https://<account>.r2.cloudflarestorage.com` or `http://localhost:9000`. Uses path-style URLs. |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | | Required for S3. `AWS_SESSION_TOKEN` is optional. |

Or build one in code: `Storage::s3(S3::new("bucket", "eu-west-1").credentials(id, secret))`.

The S3 client is built in (Signature V4 over hyper and rustls; the signing is tested against AWS's published examples and botocore), so it adds no AWS SDK to your build. It covers what the API needs: objects, ListObjectsV2 with continuation tokens, server-side copies, multipart uploads, and presigned `GET` and `PUT`.

## Testing

Use `Storage::memory()`:

```rust,ignore
let storage = Storage::memory();
let client = app().storage(storage.clone()).test();
client.post("/avatar").multipart(form).send().await.assert_status(200);
assert!(storage.exists("avatars/...").await?);
```
