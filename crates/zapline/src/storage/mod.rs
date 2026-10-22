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

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::extract::FromRequestParts;
use crate::response::{IntoResponse, Response};
use crate::{header, App, Error, HeaderValue, Request, Result};

#[cfg(feature = "s3")]
mod s3;
#[cfg(feature = "s3")]
pub use s3::S3;

/// Where [`App::storage`] serves files of a local or memory storage.
pub const FILES_PATH: &str = "/files";

/// Longest key accepted, in bytes (S3's limit).
const MAX_KEY: usize = 1024;

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
    Memory(Mutex<HashMap<String, Bytes>>),
    #[cfg(feature = "s3")]
    S3(Box<S3>),
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

    /// The key that signs local links (default: random per process, so
    /// links stop working at restart).
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
    /// comes from the key's extension.
    ///
    /// # Errors
    ///
    /// `400` for an invalid key (empty segments, `..`, a leading `/`, a
    /// backslash, longer than 1024 bytes); `503` when the backend fails.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// let storage = zapline::storage::Storage::memory();
    /// storage.put("notes/a.txt", "hello").await?;
    /// assert_eq!(storage.get("notes/a.txt").await?, "hello");
    /// # Ok(()) }
    /// ```
    pub async fn put(&self, key: &str, bytes: impl Into<Bytes>) -> Result<()> {
        check_key(key)?;
        let bytes = bytes.into();
        match &*self.backend {
            Backend::Local(dir) => {
                let path = dir.join(key);
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(io_error)?;
                }
                // Write then rename, so readers never see half a file.
                let tmp = path.with_file_name(format!(
                    ".{}.{}.tmp",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
                    fastrand::u64(..)
                ));
                tokio::fs::write(&tmp, &bytes).await.map_err(io_error)?;
                if let Err(e) = tokio::fs::rename(&tmp, &path).await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(io_error(e));
                }
            }
            Backend::Memory(files) => {
                lock(files).insert(key.to_owned(), bytes);
            }
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.put(key, bytes, content_type(key)).await?,
        }
        Ok(())
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
            Backend::Memory(files) => lock(files).get(key).cloned().ok_or_else(Error::not_found),
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.get(key).await,
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
            Backend::Local(dir) => match tokio::fs::remove_file(dir.join(key)).await {
                Ok(()) => Ok(()),
                Err(e) if missing(&e) => Ok(()),
                Err(e) => Err(io_error(e)),
            },
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
                let sig = self.sign(key, expires);
                format!(
                    "{FILES_PATH}/{}?expires={expires}&sig={sig}",
                    encode_path(key)
                )
            }
        }
    }

    fn sign(&self, key: &str, expires: u64) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC takes keys of any length");
        mac.update(key.as_bytes());
        mac.update(b"\n");
        mac.update(expires.to_string().as_bytes());
        hex(&mac.finalize().into_bytes())
    }

    fn verify(&self, key: &str, expires: u64, sig: &str) -> bool {
        if expires < unix_now() {
            return false;
        }
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC takes keys of any length");
        mac.update(key.as_bytes());
        mac.update(b"\n");
        mac.update(expires.to_string().as_bytes());
        match unhex(sig) {
            Some(sig) => mac.verify_slice(&sig).is_ok(),
            None => false,
        }
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
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..");
    if ok {
        Ok(())
    } else {
        Err(Error::bad_request(format!("invalid storage key {key:?}")).with_code("invalid_key"))
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

fn content_type(key: &str) -> &'static str {
    mime_guess::from_path(Path::new(key))
        .first_raw()
        .unwrap_or("application/octet-stream")
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
    /// ([`Storage::url`]) at [`FILES_PATH`].
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
            app.get(&format!("{FILES_PATH}/{{*key}}"), serve_file)
        } else {
            app
        }
    }
}

async fn serve_file(req: Request) -> Response {
    let Some(storage) = req.state::<Storage>().cloned() else {
        return Error::not_found().into_response();
    };
    let Some(key) = req.param("key").map(|k| k.into_owned()) else {
        return Error::not_found().into_response();
    };
    let (mut expires, mut sig) = (None, None);
    for (name, value) in form_pairs(req.query_string().unwrap_or("")) {
        match name.as_str() {
            "expires" => expires = value.parse::<u64>().ok(),
            "sig" => sig = Some(value),
            _ => {}
        }
    }
    let (Some(expires), Some(sig)) = (expires, sig) else {
        return Error::forbidden().into_response();
    };
    if !storage.verify(&key, expires, &sig) {
        return Error::forbidden().into_response();
    }
    match storage.get(&key).await {
        Ok(bytes) => {
            let mut res = bytes.into_response();
            let headers = res.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(&key)),
            );
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=300"),
            );
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            res
        }
        Err(e) => e.into_response(),
    }
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
