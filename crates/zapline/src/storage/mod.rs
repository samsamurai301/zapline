//! File storage: one API over a local directory, memory (tests) and S3
//! (AWS, Cloudflare R2, MinIO, ... with the `s3` feature).
//!
//! ```no_run
//! use zapline::multipart::Multipart;
//! use zapline::prelude::*;
//! use zapline::storage::{unique_key, Storage};
//! use std::time::Duration;
//!
//! #[post("/avatar")]
//! async fn upload(storage: Storage, mut form: Multipart) -> Result<String> {
//!     let Some(file) = form.next_field().await? else {
//!         return Err(Error::bad_request("no file"));
//!     };
//!     let key = unique_key("avatars", file.file_name().unwrap_or("upload"));
//!     storage.put(&key, file.bytes().await?).await?;
//!     // A link that works for an hour, whichever backend is in use.
//!     Ok(storage.url(&key, Duration::from_secs(3600)))
//! }
//!
//! fn main() -> zapline::Result<()> {
//!     App::new()
//!         .storage(Storage::from_env()?)
//!         .mount(upload)
//!         .run("0.0.0.0:8080")?;
//!     Ok(())
//! }
//! ```
//!
//! Files are private: [`Storage::url`] makes a link that expires. For S3
//! it is a presigned URL; for a local directory (and memory) the app serves
//! it at [`FILES_PATH`] after checking its signature.
//!
//! Large files don't need to pass through memory: [`Storage::get_stream`]
//! and [`Storage::put_stream`] move them chunk by chunk, and
//! [`Storage::upload_url`] lets the browser upload straight to the bucket
//! (or to a signed endpoint of the app for local storage).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_core::Stream;
use hmac::{Hmac, Mac};
use http_body::{Frame, SizeHint};
use serde::Serialize;
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};

use crate::body::BoxError;
use crate::dev::internal;
use crate::extract::FromRequestParts;
use crate::response::{IntoResponse, Response};
use crate::router::Routes;
use crate::stream::BodyStream;
use crate::{header, App, Body, Error, HeaderValue, Request, Result};

#[cfg(feature = "s3")]
mod s3;
#[cfg(feature = "s3")]
pub use s3::S3;

/// Where [`App::storage`] serves files of a local or memory storage (and
/// takes [direct uploads](Storage::upload_url)).
pub const FILES_PATH: &str = "/files";

/// Longest key accepted, in bytes (S3's limit).
const MAX_KEY: usize = 1024;

/// Key segments starting with this are the local backend's own files
/// (temporary writes, metadata), so no key may use it.
const RESERVED: &str = ".zl-";

/// Files under string keys like `avatars/42.png`. Cheap to clone; a handler
/// argument once registered with [`App::storage`].
#[derive(Clone)]
pub struct Storage {
    backend: Arc<Backend>,
    /// Signs local links.
    secret: Arc<[u8]>,
}

enum Backend {
    Local(PathBuf),
    Memory(Mutex<HashMap<String, MemFile>>),
    #[cfg(feature = "s3")]
    S3(Box<S3>),
}

#[derive(Clone)]
struct MemFile {
    bytes: Bytes,
    options: PutOptions,
    modified: SystemTime,
}

/// What [`Storage::list`] and [`Storage::metadata`] say about a file.
///
/// ```
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
/// let storage = zapline::storage::Storage::memory();
/// storage.put("a.txt", "hello").await?;
/// let info = storage.metadata("a.txt").await?;
/// assert_eq!((info.key.as_str(), info.size), ("a.txt", 5));
/// assert_eq!(info.content_type.as_deref(), Some("text/plain"));
/// # Ok(()) }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObjectInfo {
    /// The file's key.
    pub key: String,
    /// Its size in bytes.
    pub size: u64,
    /// When it was last written.
    pub modified: SystemTime,
    /// Its content type: the one given to [`Storage::put_with`], else
    /// guessed from the key. Only [`Storage::metadata`] fills it in
    /// (listings leave it `None`).
    pub content_type: Option<String>,
    /// The `Cache-Control` given to [`Storage::put_with`], if any (from
    /// [`Storage::metadata`] only).
    pub cache_control: Option<String>,
}

/// How [`Storage::put_with`] and [`Storage::put_stream`] store a file.
///
/// ```
/// use zapline::storage::PutOptions;
///
/// let options = PutOptions::new()
///     .content_type("application/pdf")
///     .cache_control("private, max-age=86400");
/// assert_eq!(options.content_type.as_deref(), Some("application/pdf"));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PutOptions {
    /// The content type (default: guessed from the key's extension).
    pub content_type: Option<String>,
    /// The `Cache-Control` header sent with the file.
    pub cache_control: Option<String>,
}

impl PutOptions {
    /// No options: the content type comes from the key.
    ///
    /// ```
    /// let options = zapline::storage::PutOptions::new();
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the content type (`image/png`, `text/csv; charset=utf-8`, ...).
    ///
    /// ```
    /// let options = zapline::storage::PutOptions::new().content_type("text/csv");
    /// ```
    #[must_use]
    pub fn content_type(mut self, value: impl Into<String>) -> Self {
        self.content_type = Some(value.into());
        self
    }

    /// Set the `Cache-Control` header the file is served with.
    ///
    /// ```
    /// let options = zapline::storage::PutOptions::new().cache_control("public, max-age=31536000");
    /// ```
    #[must_use]
    pub fn cache_control(mut self, value: impl Into<String>) -> Self {
        self.cache_control = Some(value.into());
        self
    }

    fn check(&self) -> Result<()> {
        let header_safe = |v: &str| !v.trim().is_empty() && HeaderValue::from_str(v).is_ok();
        if let Some(ct) = &self.content_type {
            if !header_safe(ct) || !ct.contains('/') {
                return Err(Error::bad_request(format!("invalid content type {ct:?}"))
                    .with_code("invalid_content_type"));
            }
        }
        if let Some(cc) = &self.cache_control {
            if !header_safe(cc) {
                return Err(Error::bad_request(format!("invalid cache control {cc:?}")));
            }
        }
        Ok(())
    }
}

/// A presigned upload from [`Storage::upload_url`]: send the file as the
/// body of a `method` request to `url` with `headers`. Serializes to the
/// JSON a browser needs.
///
/// ```js
/// const up = await (await fetch("/uploads", { method: "POST" })).json();
/// await fetch(up.url, { method: up.method, headers: up.headers, body: file });
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct UploadUrl {
    /// Where to send the file.
    pub url: String,
    /// The HTTP method: `PUT`.
    pub method: &'static str,
    /// Headers the request must carry (`Content-Type`).
    pub headers: BTreeMap<String, String>,
    /// The key the file will be stored under.
    pub key: String,
    /// The most bytes accepted.
    pub max_bytes: u64,
    /// When the URL stops working, in seconds since the Unix epoch.
    pub expires_at: u64,
}

impl fmt::Debug for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backend = match &*self.backend {
            Backend::Local(dir) => format!("local({})", dir.display()),
            Backend::Memory(_) => "memory".to_owned(),
            #[cfg(feature = "s3")]
            Backend::S3(s3) => format!("s3({})", s3.bucket()),
        };
        f.debug_struct("Storage")
            .field("backend", &backend)
            .finish()
    }
}

impl Storage {
    fn with(backend: Backend) -> Self {
        Storage {
            backend: Arc::new(backend),
            secret: random_secret().into(),
        }
    }

    /// Files in a directory on this machine (created when needed).
    ///
    /// ```
    /// let storage = zapline::storage::Storage::local("uploads");
    /// ```
    pub fn local(dir: impl Into<PathBuf>) -> Self {
        Self::with(Backend::Local(dir.into()))
    }

    /// Files in memory, gone at exit: for tests.
    ///
    /// ```
    /// let storage = zapline::storage::Storage::memory();
    /// ```
    pub fn memory() -> Self {
        Self::with(Backend::Memory(Mutex::default()))
    }

    /// Files in an S3 bucket.
    ///
    /// ```
    /// use zapline::storage::{Storage, S3};
    ///
    /// let storage = Storage::s3(S3::new("my-bucket", "eu-west-1").credentials("AKIA...", "secret"));
    /// ```
    #[cfg(feature = "s3")]
    pub fn s3(s3: S3) -> Self {
        Self::with(Backend::S3(Box::new(s3)))
    }

    /// S3 when `S3_BUCKET` is set (see [`S3::from_env`], `s3` feature),
    /// otherwise the local directory `STORAGE_DIR` (default `storage`).
    /// `STORAGE_SECRET` keeps local links valid across restarts.
    ///
    /// # Errors
    ///
    /// When `S3_BUCKET` is set but the S3 settings are incomplete, or the
    /// `s3` feature is off.
    ///
    /// ```no_run
    /// let storage = zapline::storage::Storage::from_env()?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn from_env() -> Result<Self> {
        let storage = if std::env::var_os("S3_BUCKET").is_some() {
            #[cfg(feature = "s3")]
            {
                Self::s3(S3::from_env()?)
            }
            #[cfg(not(feature = "s3"))]
            return Err(Error::internal(
                "S3_BUCKET is set but zapline's `s3` feature is off",
            ));
        } else {
            let dir =
                std::env::var_os("STORAGE_DIR").map_or_else(|| "storage".into(), PathBuf::from);
            Self::local(dir)
        };
        Ok(match std::env::var("STORAGE_SECRET") {
            Ok(secret) if !secret.is_empty() => storage.secret(secret),
            _ => storage,
        })
    }

    /// The key that signs local links and uploads (default: random per
    /// process, so links stop working at restart).
    ///
    /// ```
    /// let storage = zapline::storage::Storage::local("uploads").secret("long random string");
    /// ```
    #[must_use]
    pub fn secret(mut self, secret: impl AsRef<[u8]>) -> Self {
        self.secret = secret.as_ref().into();
        self
    }

    /// Store `bytes` under `key`, replacing any file there. The content type
    /// comes from the key's extension; [`put_with`](Self::put_with) sets it.
    ///
    /// # Errors
    ///
    /// `400` for an invalid key (empty segments, `..`, a leading `/`, a
    /// backslash, a segment starting with `.zl-`, longer than 1024 bytes);
    /// `503` when the backend fails.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// let storage = zapline::storage::Storage::memory();
    /// storage.put("notes/a.txt", "hello").await?;
    /// assert_eq!(storage.get("notes/a.txt").await?, "hello");
    /// # Ok(()) }
    /// ```
    pub async fn put(&self, key: &str, bytes: impl Into<Bytes>) -> Result<()> {
        self.put_with(key, bytes, PutOptions::default()).await
    }

    /// Store `bytes` under `key` with a content type and `Cache-Control`
    /// of your choice. Files served at [`FILES_PATH`] keep their
    /// protections (sandboxed, and downloaded unless the type is a safe one
    /// to show), whatever the content type.
    ///
    /// # Errors
    ///
    /// As [`put`](Self::put), and `400` for options that aren't valid
    /// header values.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// use zapline::storage::{PutOptions, Storage};
    ///
    /// let storage = Storage::memory();
    /// let options = PutOptions::new().content_type("text/csv").cache_control("no-store");
    /// storage.put_with("exports/report", "id,total\n1,42\n", options).await?;
    /// let info = storage.metadata("exports/report").await?;
    /// assert_eq!(info.content_type.as_deref(), Some("text/csv"));
    /// # Ok(()) }
    /// ```
    pub async fn put_with(
        &self,
        key: &str,
        bytes: impl Into<Bytes>,
        options: PutOptions,
    ) -> Result<()> {
        check_key(key)?;
        options.check()?;
        let bytes = bytes.into();
        match &*self.backend {
            Backend::Local(dir) => {
                let path = dir.join(key);
                let tmp = temp_path(&path).await?;
                tokio::fs::write(&tmp, &bytes).await.map_err(io_error)?;
                commit(&tmp, &path, &options).await?;
            }
            Backend::Memory(files) => {
                let file = MemFile {
                    bytes,
                    options,
                    modified: SystemTime::now(),
                };
                lock(files).insert(key.to_owned(), file);
            }
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.put(key, bytes, &resolved(key, &options)).await?,
        }
        Ok(())
    }

    /// Store a stream of chunks under `key` without holding it all in
    /// memory: written to a temporary file then moved into place locally,
    /// sent as a multipart upload (8 MiB parts) to S3. Returns the number of
    /// bytes stored. An error from the stream (such as the `413` of a
    /// [`BodyStream`] over its limit) aborts the upload and is returned
    /// as is; nothing is stored.
    ///
    /// # Errors
    ///
    /// As [`put_with`](Self::put_with), and the stream's own errors.
    ///
    /// ```
    /// use zapline::prelude::*;
    /// use zapline::storage::{PutOptions, Storage};
    /// use zapline::stream::BodyStream;
    ///
    /// // Up to 1 GiB, straight to storage.
    /// #[put("/videos/{name}")]
    /// async fn upload(storage: Storage, Path(name): Path<String>, body: BodyStream) -> Result<String> {
    ///     let key = zapline::storage::unique_key("videos", &name);
    ///     let body = body.with_limit(1 << 30);
    ///     let size = storage.put_stream(&key, body, PutOptions::new()).await?;
    ///     Ok(format!("{key}: {size} bytes"))
    /// }
    /// ```
    pub async fn put_stream<S, B, E>(
        &self,
        key: &str,
        stream: S,
        options: PutOptions,
    ) -> Result<u64>
    where
        S: Stream<Item = std::result::Result<B, E>> + Send,
        B: Into<Bytes>,
        E: Into<BoxError>,
    {
        check_key(key)?;
        options.check()?;
        let mut stream = std::pin::pin!(stream);
        match &*self.backend {
            Backend::Local(dir) => {
                let path = dir.join(key);
                let tmp = temp_path(&path).await?;
                let written = async {
                    let mut file = tokio::fs::File::create(&tmp).await.map_err(io_error)?;
                    let mut size = 0u64;
                    while let Some(chunk) = next_chunk(&mut stream).await? {
                        size += chunk.len() as u64;
                        file.write_all(&chunk).await.map_err(io_error)?;
                    }
                    file.flush().await.map_err(io_error)?;
                    Ok::<_, Error>(size)
                }
                .await;
                match written {
                    Ok(size) => {
                        commit(&tmp, &path, &options).await?;
                        Ok(size)
                    }
                    Err(e) => {
                        let _ = tokio::fs::remove_file(&tmp).await;
                        Err(e)
                    }
                }
            }
            Backend::Memory(files) => {
                let mut all = Vec::new();
                while let Some(chunk) = next_chunk(&mut stream).await? {
                    all.extend_from_slice(&chunk);
                }
                let size = all.len() as u64;
                let file = MemFile {
                    bytes: all.into(),
                    options,
                    modified: SystemTime::now(),
                };
                lock(files).insert(key.to_owned(), file);
                Ok(size)
            }
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.put_stream(key, &resolved(key, &options), stream).await,
        }
    }

    /// The file under `key`.
    ///
    /// # Errors
    ///
    /// `404` when there is none, `400` for an invalid key, `503` when the
    /// backend fails.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let storage = zapline::storage::Storage::memory();
    /// assert_eq!(storage.get("missing.txt").await.unwrap_err().status(), 404);
    /// # }
    /// ```
    pub async fn get(&self, key: &str) -> Result<Bytes> {
        check_key(key)?;
        match &*self.backend {
            Backend::Local(dir) => match tokio::fs::read(dir.join(key)).await {
                Ok(bytes) => Ok(bytes.into()),
                Err(e) if missing(&e) => Err(Error::not_found()),
                Err(e) => Err(io_error(e)),
            },
            Backend::Memory(files) => lock(files)
                .get(key)
                .map(|f| f.bytes.clone())
                .ok_or_else(Error::not_found),
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.get(key).await,
        }
    }

    /// The file under `key` as a stream of chunks (read from disk, or
    /// straight from S3, 64 KiB at a time): return it from a handler to
    /// send a large file without loading it into memory.
    ///
    /// # Errors
    ///
    /// As [`get`](Self::get).
    ///
    /// ```
    /// use zapline::prelude::*;
    /// use zapline::storage::Storage;
    /// use zapline::Body;
    ///
    /// #[get("/exports/{name}")]
    /// async fn export(storage: Storage, Path(name): Path<String>) -> Result<Body> {
    ///     storage.get_stream(&format!("exports/{name}")).await
    /// }
    /// ```
    pub async fn get_stream(&self, key: &str) -> Result<Body> {
        check_key(key)?;
        match &*self.backend {
            Backend::Local(dir) => {
                let file = match tokio::fs::File::open(dir.join(key)).await {
                    Ok(file) => file,
                    Err(e) if missing(&e) => return Err(Error::not_found()),
                    Err(e) => return Err(io_error(e)),
                };
                let meta = file.metadata().await.map_err(io_error)?;
                if !meta.is_file() {
                    return Err(Error::not_found());
                }
                Ok(Body::stream(FileBody {
                    file: Some(file),
                    buf: vec![0; 64 * 1024].into_boxed_slice(),
                    remaining: meta.len(),
                }))
            }
            Backend::Memory(files) => lock(files)
                .get(key)
                .map(|f| Body::from(f.bytes.clone()))
                .ok_or_else(Error::not_found),
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.get_stream(key).await,
        }
    }

    /// Whether a file is stored under `key`.
    ///
    /// # Errors
    ///
    /// `400` for an invalid key, `503` when the backend fails.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// let storage = zapline::storage::Storage::memory();
    /// assert!(!storage.exists("a.txt").await?);
    /// # Ok(()) }
    /// ```
    pub async fn exists(&self, key: &str) -> Result<bool> {
        check_key(key)?;
        match &*self.backend {
            Backend::Local(dir) => match tokio::fs::metadata(dir.join(key)).await {
                Ok(meta) => Ok(meta.is_file()),
                Err(e) if missing(&e) => Ok(false),
                Err(e) => Err(io_error(e)),
            },
            Backend::Memory(files) => Ok(lock(files).contains_key(key)),
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.exists(key).await,
        }
    }

    /// Size, modification time, content type and `Cache-Control` of the
    /// file under `key`, without reading it.
    ///
    /// # Errors
    ///
    /// As [`get`](Self::get).
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// let storage = zapline::storage::Storage::memory();
    /// storage.put("photos/1.jpg", vec![0xff, 0xd8]).await?;
    /// let info = storage.metadata("photos/1.jpg").await?;
    /// assert_eq!(info.size, 2);
    /// assert_eq!(info.content_type.as_deref(), Some("image/jpeg"));
    /// # Ok(()) }
    /// ```
    pub async fn metadata(&self, key: &str) -> Result<ObjectInfo> {
        check_key(key)?;
        match &*self.backend {
            Backend::Local(dir) => {
                let path = dir.join(key);
                let meta = match tokio::fs::metadata(&path).await {
                    Ok(meta) if meta.is_file() => meta,
                    Ok(_) => return Err(Error::not_found()),
                    Err(e) if missing(&e) => return Err(Error::not_found()),
                    Err(e) => return Err(io_error(e)),
                };
                let options = read_sidecar(&path).await?;
                Ok(ObjectInfo {
                    key: key.to_owned(),
                    size: meta.len(),
                    modified: meta.modified().unwrap_or(UNIX_EPOCH),
                    content_type: Some(resolved(key, &options).0),
                    cache_control: options.cache_control,
                })
            }
            Backend::Memory(files) => {
                let files = lock(files);
                let file = files.get(key).ok_or_else(Error::not_found)?;
                Ok(ObjectInfo {
                    key: key.to_owned(),
                    size: file.bytes.len() as u64,
                    modified: file.modified,
                    content_type: Some(resolved(key, &file.options).0),
                    cache_control: file.options.cache_control.clone(),
                })
            }
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.metadata(key).await,
        }
    }

    /// The files whose keys start with `prefix` (`""` for all), sorted by
    /// key. A prefix needn't end at a `/`: `"avatars/4"` lists
    /// `avatars/4.png` and `avatars/42/me.png`.
    ///
    /// # Errors
    ///
    /// `400` for a prefix no key could have (`..`, a leading `/`, ...),
    /// `503` when the backend fails.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// let storage = zapline::storage::Storage::memory();
    /// storage.put("avatars/1.png", "a").await?;
    /// storage.put("avatars/2.png", "bb").await?;
    /// storage.put("docs/x.pdf", "c").await?;
    /// let keys: Vec<_> = storage.list("avatars/").await?.into_iter().map(|f| f.key).collect();
    /// assert_eq!(keys, ["avatars/1.png", "avatars/2.png"]);
    /// # Ok(()) }
    /// ```
    pub async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        check_prefix(prefix)?;
        let mut files = match &*self.backend {
            Backend::Local(dir) => walk(dir, prefix).await?,
            Backend::Memory(files) => lock(files)
                .iter()
                .filter(|(key, _)| key.starts_with(prefix))
                .map(|(key, file)| ObjectInfo {
                    key: key.clone(),
                    size: file.bytes.len() as u64,
                    modified: file.modified,
                    content_type: None,
                    cache_control: None,
                })
                .collect(),
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.list(prefix).await?,
        };
        files.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(files)
    }

    /// Copy the file under `from` to `to` (replacing any file there), with
    /// its content type and `Cache-Control`. On S3 the copy happens inside
    /// the bucket, without downloading the file (up to 5 GB).
    ///
    /// # Errors
    ///
    /// `404` when `from` doesn't exist; otherwise as [`put`](Self::put).
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// let storage = zapline::storage::Storage::memory();
    /// storage.put("drafts/a.txt", "hello").await?;
    /// storage.copy("drafts/a.txt", "published/a.txt").await?;
    /// assert_eq!(storage.get("published/a.txt").await?, "hello");
    /// # Ok(()) }
    /// ```
    pub async fn copy(&self, from: &str, to: &str) -> Result<()> {
        check_key(from)?;
        check_key(to)?;
        match &*self.backend {
            Backend::Local(dir) => {
                let (src, dst) = (dir.join(from), dir.join(to));
                match tokio::fs::metadata(&src).await {
                    Ok(meta) if meta.is_file() => {}
                    Ok(_) => return Err(Error::not_found()),
                    Err(e) if missing(&e) => return Err(Error::not_found()),
                    Err(e) => return Err(io_error(e)),
                }
                let options = read_sidecar(&src).await?;
                let tmp = temp_path(&dst).await?;
                if let Err(e) = tokio::fs::copy(&src, &tmp).await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(if missing(&e) {
                        Error::not_found()
                    } else {
                        io_error(e)
                    });
                }
                commit(&tmp, &dst, &options).await
            }
            Backend::Memory(files) => {
                let mut files = lock(files);
                let mut file = files.get(from).cloned().ok_or_else(Error::not_found)?;
                file.modified = SystemTime::now();
                files.insert(to.to_owned(), file);
                Ok(())
            }
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.copy(from, to).await,
        }
    }

    /// Remove the file under `key`; removing a missing file is not an error.
    ///
    /// # Errors
    ///
    /// `400` for an invalid key, `503` when the backend fails.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// let storage = zapline::storage::Storage::memory();
    /// storage.put("a.txt", "x").await?;
    /// storage.delete("a.txt").await?;
    /// storage.delete("a.txt").await?;
    /// # Ok(()) }
    /// ```
    pub async fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        match &*self.backend {
            Backend::Local(dir) => {
                let path = dir.join(key);
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => {}
                    Err(e) if missing(&e) => {}
                    Err(e) => return Err(io_error(e)),
                }
                remove_sidecar(&path).await
            }
            Backend::Memory(files) => {
                lock(files).remove(key);
                Ok(())
            }
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.delete(key).await,
        }
    }

    /// A link to the file under `key` that works for `ttl` (at most 7 days
    /// on S3): a presigned URL on S3, a signed path under [`FILES_PATH`]
    /// otherwise. It doesn't check that the file exists.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// let storage = zapline::storage::Storage::memory();
    /// let link = storage.url("avatars/1.png", Duration::from_secs(600));
    /// assert!(link.starts_with("/files/avatars/1.png?expires="));
    /// ```
    pub fn url(&self, key: &str, ttl: Duration) -> String {
        match &*self.backend {
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.presign(key, ttl, SystemTime::now()),
            _ => {
                let expires = unix_now().saturating_add(ttl.as_secs());
                let sig = self.sign(&[key.as_bytes(), expires.to_string().as_bytes()]);
                format!(
                    "{FILES_PATH}/{}?expires={expires}&sig={sig}",
                    encode_path(key)
                )
            }
        }
    }

    /// A URL the browser can upload one file to directly, for `ttl` (at
    /// most 7 days on S3): a presigned `PUT` to the bucket on S3, so large
    /// files never pass through the app; a signed `PUT` to [`FILES_PATH`]
    /// otherwise, which [`App::storage`] serves. The upload must send
    /// `Content-Type: content_type`.
    ///
    /// The app's endpoint also refuses bodies over `max_bytes` (`413`).
    /// S3 can't limit the size of a presigned `PUT`: check
    /// [`metadata`](Self::metadata) when the browser reports success, and
    /// delete files that are too large.
    ///
    /// # Errors
    ///
    /// `400` for an invalid key or content type.
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::prelude::*;
    /// use zapline::storage::{unique_key, Storage, UploadUrl};
    ///
    /// #[post("/avatar/upload-url")]
    /// async fn avatar_upload(storage: Storage) -> Result<Json<UploadUrl>> {
    ///     let key = unique_key("avatars", "me.png");
    ///     let ten_minutes = Duration::from_secs(600);
    ///     Ok(Json(storage.upload_url(&key, ten_minutes, 5 << 20, "image/png")?))
    /// }
    /// ```
    pub fn upload_url(
        &self,
        key: &str,
        ttl: Duration,
        max_bytes: u64,
        content_type: &str,
    ) -> Result<UploadUrl> {
        check_key(key)?;
        PutOptions::new().content_type(content_type).check()?;
        let headers = BTreeMap::from([("Content-Type".to_owned(), content_type.to_owned())]);
        let (url, expires_at) = match &*self.backend {
            #[cfg(feature = "s3")]
            Backend::S3(s3) => {
                let now = SystemTime::now();
                let (url, ttl) = s3.presign_put(key, ttl, content_type, now);
                let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                (url, now + ttl)
            }
            _ => {
                let expires = unix_now().saturating_add(ttl.as_secs());
                let max = max_bytes.to_string();
                let sig = self.upload_signature(key, &expires.to_string(), &max, content_type);
                let url = format!(
                    "{FILES_PATH}/{}?expires={expires}&max={max}&type={}&sig={sig}",
                    encode_path(key),
                    encode_query(content_type)
                );
                (url, expires)
            }
        };
        Ok(UploadUrl {
            url,
            method: "PUT",
            headers,
            key: key.to_owned(),
            max_bytes,
            expires_at,
        })
    }

    /// HMAC of `parts` joined with newlines (keys can't contain NUL, so
    /// the upload tag below can't be forged from a download link).
    fn sign(&self, parts: &[&[u8]]) -> String {
        hex(&self.mac(parts).finalize().into_bytes())
    }

    fn mac(&self, parts: &[&[u8]]) -> Hmac<Sha256> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC takes keys of any length");
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                mac.update(b"\n");
            }
            mac.update(part);
        }
        mac
    }

    fn verify(&self, parts: &[&[u8]], expires: u64, sig: &str) -> bool {
        if expires < unix_now() {
            return false;
        }
        match unhex(sig) {
            Some(sig) => self.mac(parts).verify_slice(&sig).is_ok(),
            None => false,
        }
    }

    fn upload_signature(&self, key: &str, expires: &str, max: &str, content_type: &str) -> String {
        self.sign(&[
            b"upload\0",
            key.as_bytes(),
            expires.as_bytes(),
            max.as_bytes(),
            content_type.as_bytes(),
        ])
    }

    fn served_by_app(&self) -> bool {
        match &*self.backend {
            Backend::Local(_) | Backend::Memory(_) => true,
            #[cfg(feature = "s3")]
            Backend::S3(_) => false,
        }
    }
}

/// A fresh key for an uploaded file: `{prefix}/{random}/{name}`, with the
/// name reduced to safe characters so it can't escape the prefix.
///
/// ```
/// let key = zapline::storage::unique_key("avatars", "../My Photo.PNG");
/// assert!(key.starts_with("avatars/"));
/// assert!(key.ends_with("/My-Photo.PNG"));
/// ```
pub fn unique_key(prefix: &str, file_name: &str) -> String {
    let name: String = file_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_start_matches('.');
    let name = if name.is_empty() { "file" } else { name };
    let prefix = prefix.trim_matches('/');
    let id = format!("{:016x}", fastrand::u64(..));
    if prefix.is_empty() {
        format!("{id}/{name}")
    } else {
        format!("{prefix}/{id}/{name}")
    }
}

fn check_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key.len() <= MAX_KEY
        && !key.contains(['\\', '\0'])
        && key
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && !seg.starts_with(RESERVED));
    if ok {
        Ok(())
    } else {
        Err(Error::bad_request(format!("invalid storage key {key:?}")).with_code("invalid_key"))
    }
}

/// A prefix of valid keys: like a key, but it may be empty or end in `/`.
fn check_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    let mut segments: Vec<&str> = prefix.split('/').collect();
    let last = segments.pop().unwrap_or_default();
    let ok = prefix.len() <= MAX_KEY
        && !prefix.contains(['\\', '\0'])
        && !last.starts_with(RESERVED)
        && segments.iter().all(|seg| {
            !seg.is_empty() && *seg != "." && *seg != ".." && !seg.starts_with(RESERVED)
        });
    if ok {
        Ok(())
    } else {
        Err(
            Error::bad_request(format!("invalid storage prefix {prefix:?}"))
                .with_code("invalid_key"),
        )
    }
}

/// The content type and cache control a file is stored with.
fn resolved(key: &str, options: &PutOptions) -> (String, Option<String>) {
    let content_type = options
        .content_type
        .clone()
        .unwrap_or_else(|| content_type(key).to_owned());
    (content_type, options.cache_control.clone())
}

// ---------------------------------------------------------------------------
// The local backend's files

/// A fresh temporary path next to `path` (whose directory is created).
async fn temp_path(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(io_error)?;
    }
    Ok(path.with_file_name(format!(
        "{RESERVED}tmp.{}.{:016x}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        fastrand::u64(..)
    )))
}

/// Where the content type and cache control of `path` are kept.
fn sidecar(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{RESERVED}meta.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ))
}

/// Move the written `tmp` to `path` (so readers never see half a file) and
/// record or clear its options.
async fn commit(tmp: &Path, path: &Path, options: &PutOptions) -> Result<()> {
    if options.content_type.is_some() || options.cache_control.is_some() {
        let mut meta = String::new();
        if let Some(ct) = &options.content_type {
            meta.push_str(&format!("content-type: {ct}\n"));
        }
        if let Some(cc) = &options.cache_control {
            meta.push_str(&format!("cache-control: {cc}\n"));
        }
        if let Err(e) = tokio::fs::write(sidecar(path), meta).await {
            let _ = tokio::fs::remove_file(tmp).await;
            return Err(io_error(e));
        }
    } else {
        remove_sidecar(path).await?;
    }
    if let Err(e) = tokio::fs::rename(tmp, path).await {
        let _ = tokio::fs::remove_file(tmp).await;
        return Err(io_error(e));
    }
    Ok(())
}

async fn read_sidecar(path: &Path) -> Result<PutOptions> {
    let text = match tokio::fs::read_to_string(sidecar(path)).await {
        Ok(text) => text,
        Err(e) if missing(&e) => return Ok(PutOptions::default()),
        Err(e) => return Err(io_error(e)),
    };
    let mut options = PutOptions::default();
    for line in text.lines() {
        match line.split_once(": ") {
            Some(("content-type", v)) => options.content_type = Some(v.to_owned()),
            Some(("cache-control", v)) => options.cache_control = Some(v.to_owned()),
            _ => {}
        }
    }
    Ok(options)
}

async fn remove_sidecar(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(sidecar(path)).await {
        Ok(()) => Ok(()),
        Err(e) if missing(&e) => Ok(()),
        Err(e) => Err(io_error(e)),
    }
}

/// The files under `root` whose keys start with `prefix`. Symbolic links
/// are not followed, and names that aren't valid keys are skipped.
async fn walk(root: &Path, prefix: &str) -> Result<Vec<ObjectInfo>> {
    // Start in the deepest directory the prefix names.
    let base = prefix.rfind('/').map_or("", |i| &prefix[..i]);
    let start = if base.is_empty() {
        root.to_path_buf()
    } else {
        root.join(base)
    };
    let mut out = Vec::new();
    let mut dirs = vec![(start, base.to_owned())];
    while let Some((dir, rel)) = dirs.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(e) if missing(&e) => continue,
            Err(e) => return Err(io_error(e)),
        };
        while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with(RESERVED) {
                continue;
            }
            let key = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let kind = entry.file_type().await.map_err(io_error)?;
            if kind.is_dir() {
                // Only where keys can still match the prefix.
                if key.starts_with(prefix) || prefix.starts_with(&format!("{key}/")) {
                    dirs.push((entry.path(), key));
                }
            } else if kind.is_file() && key.starts_with(prefix) && check_key(&key).is_ok() {
                let meta = entry.metadata().await.map_err(io_error)?;
                out.push(ObjectInfo {
                    key,
                    size: meta.len(),
                    modified: meta.modified().unwrap_or(UNIX_EPOCH),
                    content_type: None,
                    cache_control: None,
                });
            }
        }
    }
    Ok(out)
}

/// A local file as a response body, read in chunks.
struct FileBody {
    file: Option<tokio::fs::File>,
    buf: Box<[u8]>,
    /// Bytes still to send: the size when opened (writes replace the file,
    /// so it doesn't change under an open handle).
    remaining: u64,
}

impl http_body::Body for FileBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        let Some(file) = this.file.as_mut() else {
            return Poll::Ready(None);
        };
        if this.remaining == 0 {
            this.file = None;
            return Poll::Ready(None);
        }
        let want = usize::try_from(this.remaining)
            .unwrap_or(usize::MAX)
            .min(this.buf.len());
        let mut buf = ReadBuf::new(&mut this.buf[..want]);
        match ready!(Pin::new(file).poll_read(cx, &mut buf)) {
            Err(e) => {
                this.file = None;
                Poll::Ready(Some(Err(e)))
            }
            Ok(()) if buf.filled().is_empty() => {
                this.file = None;
                Poll::Ready(Some(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the stored file got shorter while it was read",
                ))))
            }
            Ok(()) => {
                let chunk = Bytes::copy_from_slice(buf.filled());
                this.remaining -= chunk.len() as u64;
                Poll::Ready(Some(Ok(Frame::data(chunk))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.file.is_none() || self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn missing(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn io_error(e: std::io::Error) -> Error {
    Error::service_unavailable("file storage failed").with_source(e)
}

/// The next chunk of a stream given to `put_stream`.
pub(crate) async fn next_chunk<S, B, E>(stream: &mut Pin<&mut S>) -> Result<Option<Bytes>>
where
    S: Stream<Item = std::result::Result<B, E>>,
    B: Into<Bytes>,
    E: Into<BoxError>,
{
    match std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        None => Ok(None),
        Some(Ok(chunk)) => Ok(Some(chunk.into())),
        Some(Err(e)) => Err(stream_error(e.into())),
    }
}

/// An error from a stream given to `put_stream`: a zapline error (like a
/// `413` from [`BodyStream`]) as it was, anything else a `400`.
fn stream_error(e: BoxError) -> Error {
    match e.downcast::<crate::error::CompatError>() {
        Ok(compat) => compat.0,
        Err(e) => Error::bad_request("the upload stream failed").with_source(e),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn random_secret() -> [u8; 32] {
    let mut secret = [0; 32];
    if getrandom::getrandom(&mut secret).is_err() {
        // No OS randomness: still unpredictable enough across processes,
        // and `STORAGE_SECRET` is the fix for anything serious.
        for chunk in secret.chunks_mut(8) {
            chunk.copy_from_slice(&fastrand::u64(..).to_le_bytes());
        }
    }
    secret
}

/// Types served inline at [`FILES_PATH`]; everything else downloads.
const INLINE_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "application/pdf",
    "text/plain",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "video/mp4",
    "video/webm",
];

fn content_type(key: &str) -> &'static str {
    mime_guess::from_path(Path::new(key))
        .first_raw()
        .unwrap_or("application/octet-stream")
}

/// `text/plain; charset=utf-8` -> `text/plain`.
fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0xf)] as char);
    }
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Percent-encode a key for a URL path, keeping `/`.
pub(crate) fn encode_path(key: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const PATH: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~')
        .remove(b'/');
    utf8_percent_encode(key, PATH).to_string()
}

/// Percent-encode a query value (everything but unreserved characters).
pub(crate) fn encode_query(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    utf8_percent_encode(s, SET).to_string()
}

impl FromRequestParts for Storage {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Storage>().cloned().ok_or_else(|| {
            Error::internal(
                "no storage registered: add `.storage(Storage::from_env()?)` to the app",
            )
        })
    }
}

impl App {
    /// Register `storage` for handlers ([`Storage`] extractor) and jobs. A
    /// local or memory storage also serves its signed links
    /// ([`Storage::url`]) and takes its signed uploads
    /// ([`Storage::upload_url`]) at [`FILES_PATH`].
    ///
    /// ```
    /// use zapline::storage::Storage;
    ///
    /// let app = zapline::App::new().storage(Storage::local("uploads"));
    /// ```
    pub fn storage(self, storage: Storage) -> Self {
        let serve = storage.served_by_app();
        let app = self.state(storage);
        if serve {
            let path = format!("{FILES_PATH}/{{*key}}");
            app.mount(internal(
                Routes::new()
                    .get(&path, serve_file)
                    .put(&path, receive_upload),
            ))
        } else {
            app
        }
    }
}

/// `expires`, `sig` and the other parameters of a signed URL.
fn signed_params(req: &Request) -> HashMap<String, String> {
    form_pairs(req.query_string().unwrap_or("")).collect()
}

async fn serve_file(req: Request) -> Response {
    let Some(storage) = req.state::<Storage>().cloned() else {
        return Error::not_found().into_response();
    };
    let Some(key) = req.param("key").map(|k| k.into_owned()) else {
        return Error::not_found().into_response();
    };
    let params = signed_params(&req);
    let expires = params.get("expires").and_then(|v| v.parse::<u64>().ok());
    let (Some(expires), Some(sig)) = (expires, params.get("sig")) else {
        return Error::forbidden().into_response();
    };
    // Signed `key \n expires`; an upload signature has more parts.
    if !storage.verify(
        &[key.as_bytes(), expires.to_string().as_bytes()],
        expires,
        sig,
    ) {
        return Error::forbidden().into_response();
    }
    let info = match storage.metadata(&key).await {
        Ok(info) => info,
        Err(e) => return e.into_response(),
    };
    let body = match storage.get_stream(&key).await {
        Ok(body) => body,
        Err(e) => return e.into_response(),
    };
    let mut res = Response::new(body);
    let headers = res.headers_mut();
    let kind = info
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    if let Ok(value) = HeaderValue::from_str(&kind) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    // Uploads are user content on the app's origin: never let one run
    // script (HTML, SVG, XML), and download anything not known safe to
    // show inline.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    if !INLINE_TYPES.contains(&essence(&kind).as_str()) {
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    let cache = info
        .cache_control
        .and_then(|cc| HeaderValue::from_str(&cc).ok())
        .unwrap_or(HeaderValue::from_static("private, max-age=300"));
    headers.insert(header::CACHE_CONTROL, cache);
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}

/// `PUT /files/{key}?expires=..&max=..&type=..&sig=..`: a direct upload
/// from [`Storage::upload_url`].
async fn receive_upload(mut req: Request) -> Response {
    let Some(storage) = req.state::<Storage>().cloned() else {
        return Error::not_found().into_response();
    };
    let Some(key) = req.param("key").map(|k| k.into_owned()) else {
        return Error::not_found().into_response();
    };
    let params = signed_params(&req);
    let field = |name: &str| params.get(name).map(String::as_str);
    let (Some(expires), Some(max), Some(kind), Some(sig)) =
        (field("expires"), field("max"), field("type"), field("sig"))
    else {
        return Error::forbidden().into_response();
    };
    let (Ok(expires_at), Ok(max_bytes)) = (expires.parse::<u64>(), max.parse::<u64>()) else {
        return Error::forbidden().into_response();
    };
    let parts: [&[u8]; 5] = [
        b"upload\0",
        key.as_bytes(),
        expires.as_bytes(),
        max.as_bytes(),
        kind.as_bytes(),
    ];
    if !storage.verify(&parts, expires_at, sig) {
        return Error::forbidden()
            .with_code("invalid_upload_url")
            .into_response();
    }
    let sent_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if essence(sent_type) != essence(kind) {
        return Error::unsupported_media_type(format!("this upload must be sent as {kind}"))
            .into_response();
    }
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > max_bytes) {
        return too_large(max_bytes).into_response();
    }
    let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let body = BodyStream::new(req.take_body(), limit);
    let options = PutOptions::new().content_type(kind);
    match storage.put_stream(&key, body, options).await {
        Ok(_) => http::StatusCode::OK.into_response(),
        Err(e) if e.status() == http::StatusCode::PAYLOAD_TOO_LARGE => {
            too_large(max_bytes).into_response()
        }
        Err(e) => e.into_response(),
    }
}

fn too_large(max_bytes: u64) -> Error {
    Error::payload_too_large()
        .with_source(std::io::Error::other(format!(
            "this upload takes at most {max_bytes} bytes"
        )))
        .with_code("upload_too_large")
}

fn form_pairs(query: &str) -> impl Iterator<Item = (String, String)> + '_ {
    query.split('&').filter_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        };
        Some((decode(k), decode(v)))
    })
}
