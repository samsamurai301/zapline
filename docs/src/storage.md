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
| `storage.get(key)` | The bytes, or a `404` error. |
| `storage.exists(key)` | `true` or `false`. |
| `storage.delete(key)` | Remove it. Removing a missing file is fine. |
| `storage.url(key, ttl)` | A link that works for `ttl`. |
| `unique_key(prefix, file_name)` | `prefix/<random>/<safe-name>`: a fresh key for an upload, with a name that can't escape the prefix. |

Invalid keys (empty segments, `..`, a leading `/`, backslashes, over 1024 bytes) are a `400` with code `invalid_key`, so a key built from user input can't reach outside the storage.

## Links are private

Files aren't public. `storage.url` makes a link that expires:

- **S3**: a presigned URL (at most 7 days), straight to the bucket.
- **Local and memory**: `/files/<key>?expires=…&sig=…`, served by the app (`App::storage` mounts it) only after the HMAC signature and the expiry check out. Anything else is a `403`. Responses are sent with `X-Content-Type-Options: nosniff`.

Local links are signed with a random key per process, so they stop working at restart. Set `STORAGE_SECRET` (or call `.secret(..)`) to keep them valid, and use the same value on every instance.

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

The S3 client is built in (Signature V4 over hyper and rustls; the signing is tested against AWS's published examples), so it adds no AWS SDK to your build.

## Testing

Use `Storage::memory()`:

```rust,ignore
let storage = Storage::memory();
let client = app().storage(storage.clone()).test();
client.post("/avatar").multipart(form).send().await.assert_status(200);
assert!(storage.exists("avatars/...").await?);
```
