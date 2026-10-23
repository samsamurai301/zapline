# Static files & uploads

Serve a directory with one line, and accept `multipart/form-data` uploads with an extractor:

```rust
use zapline::fs::{ServeDir, ServeFile};
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .static_files("/assets", ServeDir::new("public").cache_control("public, max-age=3600"))
        .get("/favicon.ico", ServeFile::new("public/favicon.ico"))
}
```

Static files need the Cargo feature `fs` (it adds `mime_guess` and `tokio-util`). Uploads need `multipart` (it adds `multer`). `BodyStream` for raw streamed uploads is always available; see [Server-Sent Events & streaming](streaming.md#bodystream-large-uploads).

## `ServeDir`

`App::static_files(prefix, ServeDir)` (also `Routes::static_files`) mounts a directory under a prefix. It registers `GET` and `HEAD` on the prefix itself (the directory's index) and on `prefix/{*path}`. Mounted on a `Routes` group, the prefix nests like any route. `ServeDir` is also a plain handler, so it can be the app's fallback and serve every unmatched path:

```rust
use zapline::fs::ServeDir;
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .get("/api/ping", || async { "pong" })
        .fallback(ServeDir::new("dist"))              // every other path comes from dist/
}
```

| Method | Default | Effect |
|---|---|---|
| `ServeDir::new(root)` | | Serve files under `root` (relative to the working directory). The directory may be created after startup. |
| `.index_file(name)` | `index.html` | The file served for directory requests. A directory requested without a trailing slash is redirected (`307`) to add one, so relative links in the page resolve correctly. Behind `NormalizePath::trim_trailing_slash()` the index is served at `/dir` directly instead (see below). |
| `.no_index_file()` | | Directory requests answer `404` |
| `.fallback_file(path)` | none | Serve this file with `200` when the requested file doesn't exist. This is how single-page apps work. Relative to the root. |
| `.cache_control(value)` | none | Sets `Cache-Control` on file responses. Panics on an invalid value. |
| `.precompressed_gzip()` | off | Serve `file.gz` instead of `file` when the client accepts gzip and the compressed file exists |
| `.precompressed_br()` | off | The same for `file.br` and Brotli. Preferred over gzip at equal `q`. |
| `.show_hidden(bool)` | `false` | Serve dotfiles and files in dot-directories |

### With `NormalizePath`

[`NormalizePath`](hardening.md#normalizepath) and the directory redirect disagree about trailing slashes, so `ServeDir` defers to it:

- **`trim_trailing_slash()`** (the default) would turn the redirect target `/guide/` back into `/guide`, forever. Behind it, `ServeDir` serves `guide/index.html` at `/guide` directly, and `/guide/` gets the same page (rewritten, or redirected once to `/guide` in `redirect` mode). Relative links in such an index page resolve against the parent directory, so use absolute links there (or a `<base href="/guide/">`).
- **`append_trailing_slash()`** already agrees with `ServeDir`: `/guide` becomes `/guide/`, which serves the index.

### Single-page apps

Build assets with a bundler that writes `.br` and `.gz` siblings, then serve them as they are, with a fallback to `index.html` for client-side routes:

```rust
use zapline::fs::ServeDir;
use zapline::prelude::*;

fn app() -> App {
    let spa = ServeDir::new("dist")
        .fallback_file("index.html")
        .precompressed_br()
        .precompressed_gzip()
        .cache_control("public, max-age=600");

    App::new()
        .nest("/api", Routes::new().get("/ping", || async { "pong" }))
        .fallback(spa)
}
```

Unknown paths, `/api/...` included, now answer `index.html` with `200`. If your API should keep returning JSON `404`s, mount the SPA under its own prefix instead (`static_files("/app", ..)`).

### What you get

- **`Content-Type`** from the file extension (`; charset=utf-8` is added for text), `application/octet-stream` for unknown ones.
- **Conditional requests.** A weak `ETag` (size and modification time) with `If-None-Match`, and `Last-Modified` with `If-Modified-Since`, both answering `304`.
- **Ranges.** A single byte range (`bytes=a-b`, `a-`, `-n`) answers `206` with `Content-Range`, or `416` when unsatisfiable. `If-Range` is honored by date; since the `ETag` is weak, an `If-Range` with an entity tag returns the whole file. Requests with several ranges also get the whole file (`200`).
- **Precompressed variants** chosen from `Accept-Encoding` q-values, with `Content-Encoding`, `Vary: accept-encoding` and an `ETag` that differs per encoding. The `Content-Type` comes from the original file.
- **`HEAD`** support with exact `Content-Length`. Files are streamed in 64 KiB chunks (small ones are read in one go), never buffered whole. Other methods answer `405` with `Allow: GET, HEAD`.

```text
GET /app.js            -> 200  ETag: W/"578-6ad7d21e.1e722142"   Cache-Control: public, max-age=60
GET /app.js (If-None-Match: that ETag) -> 304
GET /app.js (Range: bytes=0-9)         -> 206  Content-Range: bytes 0-9/1400
GET /app.js (Range: bytes=99999-)      -> 416  Content-Range: bytes */1400
GET /sub                               -> 307  Location: /sub/
GET /.env                              -> 404
```

### Security guarantees

Static file serving is a classic source of vulnerabilities, so `ServeDir` is conservative:

- Request paths are percent-decoded **once**, then rejected if any segment is `..`, contains a backslash or NUL (or `:` on Windows), or is not a plain file name.
- The resolved path is **canonicalized and must stay inside the canonicalized root**, so symlinks cannot escape it. The same applies to precompressed siblings.
- **Dotfiles** (and anything inside dot-directories) are refused unless `show_hidden(true)`. `.env` and `.git/` never leak.
- Directories are never listed.
- All rejected paths answer an ordinary `404`, so probing reveals nothing. Redirects are built so they can never turn into `//host` URLs.

Things the framework cannot decide for you:

- Everything under the root is public. Don't point `ServeDir` at a directory that mixes public and private files.
- If users can upload `.html` or `.svg` files and you serve them back from your own origin, a script in the file runs with your origin's privileges (stored XSS). Serve user uploads from a separate domain, restrict the extensions you accept, or store files under generated names and send a download `Content-Disposition` through your own handler. `SecurityHeaders` already adds `X-Content-Type-Options: nosniff`.
- File serving does a short blocking filesystem lookup on a worker thread (`spawn_blocking`), once per request. Put a CDN or reverse proxy in front for hot assets.

## Fingerprinted assets

`App::assets(prefix, dir)` serves a directory like `static_files`, and also gives every file a URL with a hash of its content, so browsers can cache it forever and still get the new version the moment it changes:

```rust
use zapline::fs::Assets;
use zapline::prelude::*;

#[get("/")]
async fn home(assets: Assets) -> Html<String> {
    // "/static/app.3f2a1b9c.css"
    Html(format!(r#"<link rel="stylesheet" href="{}">"#, assets.url("app.css")))
}

fn app() -> App {
    App::new().assets("/static", "static").mount(home)
}
```

In templates (feature `views`) it is a function, whether `.views(..)` comes before or after `.assets(..)`:

```html
<link rel="stylesheet" href="{{ asset('app.css') }}">
<script src="{{ asset('js/app.js') }}" defer></script>
```

- `/static/app.3f2a1b9c.css` is served with `Cache-Control: public, max-age=31536000, immutable`.
- An old hash still serves today's file, with `no-cache`, so pages cached before a deploy keep working.
- `/static/app.css` (no hash) is still served, without the long cache.
- A file that does not exist keeps its plain URL; hidden files stay hidden.
- Hashes are 8 hex digits of the content. In debug builds (and under `zapline dev`, even with `--release`) they are recomputed when a file's size or modification time changes, so edits show up at once; release builds hash each file once.

No build step, bundler or manifest file: the hash is computed by the server on first use. The `fullstack` template uses it for `app.css`.

## Tailwind CSS without Node

```sh
zapline g tailwind
```

This writes `styles/app.css` and compiles it to `static/app.css` with Tailwind's official standalone binary. zapline downloads it once into your user cache (`~/.cache/zapline`), pinned to one version and checked against its SHA-256 before it runs; set `ZAPLINE_TAILWIND=/path/to/tailwindcss` to use your own. No Node, npm or `package.json`.

- `zapline dev` runs Tailwind's watcher alongside the app: add a class in `templates/` and open pages reload, then get the rebuilt `static/app.css` swapped in without another reload ([the dev loop](cli.md#the-dev-loop-zapline-dev)).
- `zapline css` builds once; `zapline css --minify` for a smaller file before you deploy.
- Commit `static/app.css`. It is what the server (and the Docker image) serves, so deploys need no Tailwind.

What the generator does with what you already had:

- The CSS that was in `static/app.css` moves into `styles/app.css`, in `@layer components`, so utility classes can override it.
- Stylesheets your `templates/base.html` loaded from a CDN (Pico, in the `fullstack` template) move into `styles/app.css` as `@import url(..) layer(base)`. They keep styling plain elements in place of Tailwind's reset, and utilities win over them. For Tailwind alone, replace the lines above `@source` with `@import "tailwindcss";`.

## `ServeFile`

`ServeFile::new(path)` serves one file, whatever the request path. It is a handler, so it goes anywhere a handler goes. It has the same `ETag`, range, `HEAD` and precompression behavior, and `.cache_control(..)`, `.precompressed_gzip()` and `.precompressed_br()`. A missing file answers `404`.

```rust
use zapline::fs::ServeFile;
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .get("/favicon.ico", ServeFile::new("public/favicon.ico"))
        .get("/robots.txt", ServeFile::new("public/robots.txt").cache_control("public, max-age=86400"))
}
```

## Uploads with `Multipart`

`Multipart` is the extractor for `multipart/form-data` bodies, the format HTML file forms send. It parses the body as a stream of fields, **without ever buffering it whole**, and must be the last handler argument:

```rust
use zapline::multipart::Multipart;
use zapline::prelude::*;

#[post("/upload")]
async fn upload(mut form: Multipart) -> Result<String> {
    let mut summary = Vec::new();
    while let Some(field) = form.next_field().await? {
        let name = field.name().unwrap_or("").to_owned();
        match field.file_name().map(str::to_owned) {
            Some(file) => {
                let data = field.bytes().await?;
                summary.push(format!("{name}: {file} ({} bytes)", data.len()));
            }
            None => summary.push(format!("{name} = {}", field.text().await?)),
        }
    }
    Ok(summary.join("\n"))
}
```

Only one field can be read at a time: a `Field` borrows the `Multipart`, so consume or drop it before calling `next_field` again. Read a field whole, chunk by chunk, or straight to disk:

| `Field` method | Returns |
|---|---|
| `name()` | The form field name |
| `file_name()` | The client-supplied file name. **Untrusted input:** never use it as a path directly. |
| `safe_file_name()` | The file name reduced to a single safe path component: directories, control characters and `\ / : * ? " < > \|` removed, at most 255 bytes. `None` if nothing usable remains. |
| `content_type()`, `headers()`, `index()` | The part's `Content-Type`, all its headers, its position in the body |
| `bytes()`, `text()`, `json::<T>()` | The whole field, buffered (`text` decodes with the part's charset, UTF-8 by default; `json` needs the `json` feature and fails with `422`) |
| `chunk()` | The next `Bytes`, or `None` at the end (a `Field` is also a `Stream`) |
| `save_to(path)` | Streams the field into a new file at `path` (created or truncated) and returns the number of bytes written |

A complete handler that stores files safely:

```rust
use std::path::PathBuf;
use serde::Serialize;
use zapline::multipart::{Multipart, MultipartConfig};
use zapline::prelude::*;

#[derive(Clone)]
struct UploadDir(PathBuf);

#[derive(Serialize)]
struct Uploaded {
    fields: Vec<(String, String)>,
    files: Vec<(String, u64)>,
}

async fn upload(State(dir): State<UploadDir>, mut form: Multipart) -> Result<Json<Uploaded>> {
    let mut out = Uploaded { fields: Vec::new(), files: Vec::new() };
    while let Some(field) = form.next_field().await? {
        let name = field.name().unwrap_or_default().to_owned();
        if field.file_name().is_some() {
            // Never trust the client's file name as a path.
            let Some(file_name) = field.safe_file_name() else {
                return Err(Error::bad_request("invalid file name"));
            };
            let size = field.save_to(dir.0.join(&file_name)).await?;
            out.files.push((file_name, size));
        } else {
            out.fields.push((name, field.text().await?));
        }
    }
    Ok(Json(out))
}

fn app() -> App {
    App::new()
        .state(UploadDir(PathBuf::from("uploads")))
        .post("/upload", upload)
        .meta(MultipartConfig::new().body_limit(50 * 1024 * 1024).max_fields(20))
}
```

Even a safe name can collide with an existing upload. For user content, prefer generating the stored name yourself (a UUID or a hash) and keeping the original name as metadata.

### Limits

By default the whole body is bounded by the app's body limit (`App::body_limit`, 2 MiB), which is far too small for uploads. A `MultipartConfig` sets the limits, and it is looked up first from the matched route's metadata (`.meta(..)` after the route), then from app state (`.state(..)`). Put app-wide defaults in state and override them per route:

| Method | Effect |
|---|---|
| `.body_limit(bytes)` | Maximum size of the whole request body (default: the app's body limit) |
| `.max_field_size(bytes)` | Maximum size of any single field |
| `.field_limit(name, bytes)` | Maximum size of the field called `name`. Overrides `max_field_size`. |
| `.allowed_fields([..])` | Only accept these field names; others fail with `400` |
| `.max_fields(n)` | Maximum number of fields; more fail with `413` |

```rust
use zapline::multipart::{Multipart, MultipartConfig};
use zapline::prelude::*;

async fn upload(_form: Multipart) {}

fn app() -> App {
    App::new()
        // App-wide defaults...
        .state(MultipartConfig::new().max_field_size(256 * 1024))
        // ...and a per-route override for the big upload endpoint.
        .post("/videos", upload)
        .meta(
            MultipartConfig::new()
                .body_limit(512 * 1024 * 1024)
                .field_limit("video", 512 * 1024 * 1024)
                .allowed_fields(["title", "video"])
                .max_fields(4),
        )
}
```

A route-level `MultipartConfig` **replaces** the app-level one completely; it doesn't merge with it.

Errors are ordinary `zapline::Error`s with stable codes:

| Problem | Status | Code |
|---|---|---|
| Not `multipart/form-data` | `415` | `multipart_unsupported_media_type` |
| Missing boundary | `400` | `multipart_missing_boundary` |
| Malformed body | `400` | `multipart_invalid` |
| Field not in `allowed_fields` | `400` | `multipart_unknown_field` |
| Body too large | `413` | none |
| A field exceeds its limit | `413` | `multipart_field_too_large` |
| Too many fields | `413` | `multipart_too_many_fields` |

A `Content-Length` larger than the body limit is rejected up front, before any byte is read. Behind a reverse proxy, remember its own body-size limit too (nginx's `client_max_body_size` defaults to 1 MB).

### Testing uploads

The test client sends raw bodies, so build the multipart payload by hand:

```rust
use zapline::multipart::Multipart;
use zapline::prelude::*;

async fn upload(mut form: Multipart) -> Result<String> {
    let mut names = Vec::new();
    while let Some(field) = form.next_field().await? {
        names.push(field.name().unwrap_or("").to_owned());
    }
    Ok(names.join(","))
}

#[tokio::test]
async fn upload_reads_all_fields() {
    let body = "--X\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhi\r\n\
                --X\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\ndata\r\n--X--\r\n";
    let res = App::new()
        .post("/upload", upload)
        .test()
        .post("/upload")
        .header("content-type", "multipart/form-data; boundary=X")
        .body(body)
        .send()
        .await;
    assert_eq!(res.text(), "note,file");
}
```
