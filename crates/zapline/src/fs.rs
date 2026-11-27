//! Static file serving.
//!
//! Mount a directory with [`App::static_files`] / [`Routes::static_files`],
//! or serve one file with [`ServeFile`] as a regular handler:
//!
//! ```no_run
//! use zapline::prelude::*;
//! use zapline::fs::{ServeDir, ServeFile};
//!
//! App::new()
//!     .static_files("/assets", ServeDir::new("public").cache_control("public, max-age=3600"))
//!     .get("/favicon.ico", ServeFile::new("public/favicon.ico"))
//!     .run("0.0.0.0:8080")
//! # ;
//! ```
//!
//! What you get:
//!
//! - `Content-Type` from the file extension (`; charset=utf-8` for text).
//! - Conditional requests: weak `ETag` (size + mtime) with `If-None-Match`,
//!   and `Last-Modified` with `If-Modified-Since`, both answering `304`.
//! - Single byte ranges (`Range: bytes=a-b`, `a-`, `-n`, honoring `If-Range`)
//!   answering `206` with `Content-Range`, or `416` when unsatisfiable.
//!   Multi-range requests get the whole file (`200`).
//! - Optional precompressed variants (`file.br`, `file.gz`) chosen from
//!   `Accept-Encoding`.
//! - `HEAD` support (headers only), exact `Content-Length`, and files
//!   streamed in 64 KiB chunks instead of being buffered.
//!
//! # Security
//!
//! Request paths are percent-decoded once, then rejected if any segment is
//! `..`, contains a backslash or NUL (or `:` on Windows), or is not a plain
//! file name. The resolved path is canonicalized and must stay inside the
//! canonicalized root, so symlinks cannot escape it. Dotfiles (and anything
//! inside dot-directories) are refused unless
//! [`show_hidden(true)`](ServeDir::show_hidden). Directories are never
//! listed. Rejected paths answer `404`.

use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{ready, Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_core::Stream;
use http::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, ALLOW, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED,
    LOCATION, RANGE, VARY,
};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use http_body::{Frame, SizeHint};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::body::Body;
use crate::handler::{BoxFuture, Handler};
use crate::middleware::normalize_path::TrailingSlashTrimmed;
use crate::request::{decode, Request};
use crate::response::{IntoResponse, Response};
use crate::router::Routes;
use crate::{App, Error};

/// Files up to this size are read in one go; larger ones are streamed.
const SMALL_FILE: u64 = 64 * 1024;
/// Chunk size when streaming large files.
const CHUNK: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Public API

/// Serves files from a directory.
///
/// Mount it with [`App::static_files`] / [`Routes::static_files`], or use it
/// directly as a handler (e.g. `App::fallback(ServeDir::new("dist"))` to
/// serve every unmatched path from `dist`).
///
/// ```
/// use zapline::fs::ServeDir;
///
/// // A single-page app: unknown paths get index.html so the client router
/// // can handle them; assets are precompressed at build time.
/// let spa = ServeDir::new("dist")
///     .fallback_file("index.html")
///     .precompressed_br()
///     .precompressed_gzip()
///     .cache_control("public, max-age=600");
/// ```
#[derive(Clone)]
pub struct ServeDir {
    inner: Arc<DirConfig>,
}

#[derive(Clone)]
struct DirConfig {
    root: PathBuf,
    canonical_root: OnceLock<PathBuf>,
    index_file: Option<String>,
    fallback_file: Option<PathBuf>,
    show_hidden: bool,
    opts: Options,
    /// Also answer `name.{hash}.ext` with `name.ext` (see [`App::assets`]).
    fingerprints: Option<Assets>,
}

#[derive(Clone, Default)]
struct Options {
    cache_control: Option<HeaderValue>,
    gzip: bool,
    br: bool,
}

impl ServeDir {
    /// Serve files under `root` (relative paths are resolved against the
    /// working directory). The directory may be created after startup.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ServeDir {
            inner: Arc::new(DirConfig {
                root: root.into(),
                canonical_root: OnceLock::new(),
                index_file: Some("index.html".to_owned()),
                fallback_file: None,
                show_hidden: false,
                opts: Options::default(),
                fingerprints: None,
            }),
        }
    }

    fn config(&mut self) -> &mut DirConfig {
        Arc::make_mut(&mut self.inner)
    }

    /// The file served for directory requests (default `index.html`).
    /// A request for a directory without a trailing slash is redirected
    /// (`307`) to add one, so relative links in the page resolve correctly.
    /// Behind [`NormalizePath::trim_trailing_slash`](crate::middleware::NormalizePath::trim_trailing_slash),
    /// which would undo that redirect, the index is served directly instead.
    pub fn index_file(mut self, name: impl Into<String>) -> Self {
        self.config().index_file = Some(name.into());
        self
    }

    /// Don't serve index files: directory requests answer `404`.
    pub fn no_index_file(mut self) -> Self {
        self.config().index_file = None;
        self
    }

    /// Serve this file (with `200`) when the requested file doesn't exist,
    /// e.g. `index.html` for single-page apps. Relative paths are resolved
    /// against the root. Rejected paths (traversal, hidden files) still
    /// answer `404`.
    pub fn fallback_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config().fallback_file = Some(path.into());
        self
    }

    /// Set `Cache-Control` on file responses.
    ///
    /// # Panics
    /// If `value` is not a valid header value (a startup configuration error).
    pub fn cache_control(mut self, value: &str) -> Self {
        self.config().opts.cache_control =
            Some(HeaderValue::from_str(value).expect("invalid Cache-Control value"));
        self
    }

    /// Serve `file.gz` instead of `file` when the client accepts gzip and
    /// the compressed file exists.
    pub fn precompressed_gzip(mut self) -> Self {
        self.config().opts.gzip = true;
        self
    }

    /// Serve `file.br` instead of `file` when the client accepts Brotli and
    /// the compressed file exists. Preferred over gzip at equal `q`.
    pub fn precompressed_br(mut self) -> Self {
        self.config().opts.br = true;
        self
    }

    /// Serve dotfiles and files in dot-directories (default `false`:
    /// refused with `404`, so `.env`, `.git/` etc. never leak).
    pub fn show_hidden(mut self, show: bool) -> Self {
        self.config().show_hidden = show;
        self
    }

    /// The configured root directory.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }
}

impl fmt::Debug for ServeDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = &self.inner;
        f.debug_struct("ServeDir")
            .field("root", &c.root)
            .field("index_file", &c.index_file)
            .field("fallback_file", &c.fallback_file)
            .field("show_hidden", &c.show_hidden)
            .field("cache_control", &c.opts.cache_control)
            .field("gzip", &c.opts.gzip)
            .field("br", &c.opts.br)
            .finish()
    }
}

impl Handler<ServeDir> for ServeDir {
    fn call(&self, req: Request) -> BoxFuture<Response> {
        let cfg = self.inner.clone();
        Box::pin(serve_dir(cfg, req))
    }
}

/// Serves a single file, whatever the request path. Usable directly as a
/// handler.
///
/// ```
/// use zapline::prelude::*;
/// use zapline::fs::ServeFile;
///
/// let app = App::new()
///     .get("/favicon.ico", ServeFile::new("public/favicon.ico"))
///     .get("/robots.txt", ServeFile::new("public/robots.txt").cache_control("public, max-age=86400"));
/// ```
#[derive(Clone)]
pub struct ServeFile {
    inner: Arc<FileConfig>,
}

#[derive(Clone)]
struct FileConfig {
    path: PathBuf,
    opts: Options,
}

impl ServeFile {
    /// Serve the file at `path` (resolved against the working directory if
    /// relative). A missing file answers `404`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ServeFile {
            inner: Arc::new(FileConfig {
                path: path.into(),
                opts: Options::default(),
            }),
        }
    }

    /// Set `Cache-Control` on the response.
    ///
    /// # Panics
    /// If `value` is not a valid header value (a startup configuration error).
    pub fn cache_control(mut self, value: &str) -> Self {
        Arc::make_mut(&mut self.inner).opts.cache_control =
            Some(HeaderValue::from_str(value).expect("invalid Cache-Control value"));
        self
    }

    /// Serve `file.gz` when the client accepts gzip and it exists.
    pub fn precompressed_gzip(mut self) -> Self {
        Arc::make_mut(&mut self.inner).opts.gzip = true;
        self
    }

    /// Serve `file.br` when the client accepts Brotli and it exists.
    pub fn precompressed_br(mut self) -> Self {
        Arc::make_mut(&mut self.inner).opts.br = true;
        self
    }

    /// The file being served.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
}

impl fmt::Debug for ServeFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServeFile")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

impl Handler<ServeFile> for ServeFile {
    fn call(&self, req: Request) -> BoxFuture<Response> {
        let cfg = self.inner.clone();
        Box::pin(async move {
            if let Some(res) = reject_method(&req) {
                return res;
            }
            let encodings = cfg.opts.encodings(req.headers());
            let path = cfg.path.clone();
            let found = blocking(move || open_variant(&path, &encodings, None)).await;
            match found {
                Lookup::Found(f) => respond(&req, f, &cfg.opts).await,
                other => other.into_error_response(),
            }
        })
    }
}

/// Route metadata attached to routes registered by
/// [`App::static_files`] / [`Routes::static_files`], so plugins (e.g. API
/// docs) can recognize and skip them.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct StaticFilesRoute {
    /// The directory being served.
    pub root: PathBuf,
}

impl Routes {
    /// Serve `dir` under `prefix`: registers `GET` and `HEAD` on `prefix`
    /// (the directory's index) and on `prefix/{*path}`.
    ///
    /// ```
    /// use zapline::prelude::*;
    /// use zapline::fs::ServeDir;
    ///
    /// let docs = Routes::new().static_files("/docs", ServeDir::new("site"));
    /// let app = App::new().nest("/v1", docs); // serves /v1/docs/...
    /// ```
    pub fn static_files(self, prefix: &str, dir: ServeDir) -> Self {
        let base = prefix.trim().trim_end_matches('/');
        let index = if base.is_empty() { "/" } else { base };
        let wildcard = format!("{base}/{{*path}}");
        let meta = StaticFilesRoute {
            root: dir.inner.root.clone(),
        };
        let mut routes = self;
        for path in [index, wildcard.as_str()] {
            for method in [Method::GET, Method::HEAD] {
                routes = routes.route(method, path, dir.clone()).meta(meta.clone());
            }
        }
        routes
    }
}

impl App {
    /// Serve `dir` under `prefix`. See [`Routes::static_files`].
    ///
    /// ```no_run
    /// use zapline::prelude::*;
    /// use zapline::fs::ServeDir;
    ///
    /// App::new().static_files("/", ServeDir::new("public")).run("0.0.0.0:8080")
    /// # ;
    /// ```
    pub fn static_files(mut self, prefix: &str, dir: ServeDir) -> Self {
        let routes = std::mem::take(&mut self.routes);
        self.routes = routes.static_files(prefix, dir);
        self
    }
}

// ---------------------------------------------------------------------------
// Lookup (blocking filesystem work, one thread hop per request)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Br,
    Gzip,
}

impl Encoding {
    fn suffix(self) -> &'static str {
        match self {
            Encoding::Br => ".br",
            Encoding::Gzip => ".gz",
        }
    }

    fn token(self) -> &'static str {
        match self {
            Encoding::Br => "br",
            Encoding::Gzip => "gzip",
        }
    }
}

struct Resolved {
    file: File,
    meta: Metadata,
    /// The uncompressed file's path (drives the Content-Type).
    path: PathBuf,
    encoding: Option<Encoding>,
}

enum Lookup {
    Found(Resolved),
    RedirectSlash,
    NotFound,
    Forbidden,
    Failed(io::Error),
}

impl Lookup {
    fn from_io(e: io::Error) -> Lookup {
        match e.kind() {
            io::ErrorKind::NotFound
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::IsADirectory
            | io::ErrorKind::InvalidFilename
            | io::ErrorKind::InvalidInput => Lookup::NotFound,
            io::ErrorKind::PermissionDenied => Lookup::Forbidden,
            _ => Lookup::Failed(e),
        }
    }

    fn into_error_response(self) -> Response {
        match self {
            Lookup::Forbidden => Error::forbidden().into_response(),
            Lookup::Failed(e) => Error::internal("failed to read file")
                .with_source(e)
                .into_response(),
            _ => Error::not_found().into_response(),
        }
    }
}

async fn blocking<F: FnOnce() -> Lookup + Send + 'static>(f: F) -> Lookup {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Lookup::Failed(io::Error::other(e)))
}

impl DirConfig {
    fn canonical_root(&self) -> io::Result<&Path> {
        if let Some(root) = self.canonical_root.get() {
            return Ok(root);
        }
        let root = std::fs::canonicalize(&self.root)?;
        Ok(self.canonical_root.get_or_init(|| root))
    }

    /// Canonicalize `candidate` and make sure it stays inside `root` (and
    /// outside dot-directories unless hidden files are allowed). Escapes
    /// report `NotFound`.
    fn within(&self, root: &Path, candidate: &Path) -> io::Result<PathBuf> {
        let canon = std::fs::canonicalize(candidate)?;
        match canon.strip_prefix(root) {
            Ok(rel) if self.show_hidden || !rel.components().any(is_hidden) => Ok(canon),
            _ => Err(io::ErrorKind::NotFound.into()),
        }
    }

    fn lookup(&self, rel: &Path, dir_needs_slash: bool, encodings: &[Encoding]) -> Lookup {
        let root = match self.canonical_root() {
            Ok(r) => r,
            Err(e) => return Lookup::from_io(e),
        };
        let mut path = match self.within(root, &root.join(rel)) {
            Ok(p) => p,
            Err(e) => return Lookup::from_io(e),
        };
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return Lookup::from_io(e),
        };
        if meta.is_dir() {
            let Some(index) = &self.index_file else {
                return Lookup::NotFound;
            };
            path = match self.within(root, &path.join(index)) {
                Ok(p) => p,
                Err(e) => return Lookup::from_io(e),
            };
            if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                return Lookup::NotFound;
            }
            if dir_needs_slash {
                return Lookup::RedirectSlash;
            }
        } else if !meta.is_file() {
            return Lookup::NotFound;
        }
        open_variant(&path, encodings, Some(root))
    }

    fn lookup_fallback(&self, encodings: &[Encoding]) -> Lookup {
        let Some(fallback) = &self.fallback_file else {
            return Lookup::NotFound;
        };
        let path = match self.canonical_root() {
            Ok(root) => root.join(fallback),
            Err(e) => return Lookup::from_io(e),
        };
        open_variant(&path, encodings, None)
    }
}

fn is_hidden(c: Component<'_>) -> bool {
    matches!(c, Component::Normal(s) if s.as_encoded_bytes().starts_with(b"."))
}

/// Open `path`, or its best precompressed sibling. When `root` is given the
/// sibling must also resolve inside it.
fn open_variant(path: &Path, encodings: &[Encoding], root: Option<&Path>) -> Lookup {
    for &enc in encodings {
        let mut name = path.as_os_str().to_owned();
        name.push(enc.suffix());
        let candidate = PathBuf::from(name);
        let candidate = match root {
            Some(root) => match std::fs::canonicalize(&candidate) {
                Ok(c) if c.starts_with(root) => c,
                _ => continue,
            },
            None => candidate,
        };
        if let Some((file, meta)) = open_file(&candidate) {
            return Lookup::Found(Resolved {
                file,
                meta,
                path: path.to_owned(),
                encoding: Some(enc),
            });
        }
    }
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return Lookup::from_io(e),
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() => Lookup::Found(Resolved {
            file,
            meta,
            path: path.to_owned(),
            encoding: None,
        }),
        Ok(_) => Lookup::NotFound,
        Err(e) => Lookup::from_io(e),
    }
}

fn open_file(path: &Path) -> Option<(File, Metadata)> {
    let file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    meta.is_file().then_some((file, meta))
}

/// Turn the (percent-decoded) request tail into a safe relative path, or
/// `None` if any segment is suspicious.
fn sanitize(tail: &str, show_hidden: bool) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for seg in tail.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains(['\\', '\0']) || (!show_hidden && seg.starts_with('.')) {
            return None;
        }
        if cfg!(windows) && !windows_safe_name(seg) {
            return None;
        }
        let mut comps = Path::new(seg).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(_)), None) => out.push(seg),
            _ => return None,
        }
    }
    Some(out)
}

/// Whether `seg` names a regular file the same way on Windows. Rejects drive
/// and stream separators (`:`), DOS device names (`CON`, `NUL`, `COM1`,
/// `lpt1.txt`, ...) which open devices instead of files, and names ending in
/// `.` or space, which Win32 silently strips (so `secret.txt.` would open
/// `secret.txt`). Compiled everywhere so it is unit-tested on every OS.
fn windows_safe_name(seg: &str) -> bool {
    if seg.contains(':') || seg.ends_with(['.', ' ']) {
        return false;
    }
    let stem = seg.split('.').next().unwrap_or(seg).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    let device = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ((upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.len() == 4
        && upper.as_bytes()[3].is_ascii_digit())
        || ((stem.starts_with(['C', 'c']) || stem.starts_with(['L', 'l']))
            && stem.chars().count() == 4
            && stem.ends_with(['\u{b9}', '\u{b2}', '\u{b3}'])
            && matches!(upper.get(..3), Some("COM" | "LPT")));
    !device
}

// ---------------------------------------------------------------------------
// Request handling

fn reject_method(req: &Request) -> Option<Response> {
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return None;
    }
    Some(
        Error::method_not_allowed()
            .with_header(ALLOW, HeaderValue::from_static("GET, HEAD"))
            .into_response(),
    )
}

async fn serve_dir(cfg: Arc<DirConfig>, req: Request) -> Response {
    if let Some(res) = reject_method(&req) {
        return res;
    }
    // Mounted: the `{*path}` tail (empty at the mount root). As a fallback
    // handler: the whole request path.
    let tail = match req.param("path") {
        Some(p) => p.into_owned(),
        None if req.route().is_none() => decode(req.path()).into_owned(),
        None => String::new(),
    };
    let Some(rel) = sanitize(&tail, cfg.show_hidden) else {
        return Error::not_found().into_response();
    };
    // Behind `NormalizePath::trim_trailing_slash`, `/dir/` would be trimmed
    // back to `/dir`: serve the index there instead of redirecting forever.
    let dir_needs_slash = !rel.as_os_str().is_empty()
        && !tail.ends_with('/')
        && req.extensions().get::<TrailingSlashTrimmed>().is_none();
    let encodings = cfg.opts.encodings(req.headers());

    let c = cfg.clone();
    let enc = encodings.clone();
    let found = match blocking(move || c.lookup(&rel, dir_needs_slash, &enc)).await {
        Lookup::NotFound if cfg.fallback_file.is_some() => {
            let c = cfg.clone();
            blocking(move || c.lookup_fallback(&encodings)).await
        }
        other => other,
    };
    let found = match (found, &cfg.fingerprints) {
        (Lookup::NotFound, Some(assets)) => match assets.unfingerprint(&tail, cfg.show_hidden) {
            Some((rel, hash)) => {
                let (c, a) = (cfg.clone(), assets.clone());
                let enc = cfg.opts.encodings(req.headers());
                let looked = tokio::task::spawn_blocking(move || {
                    let found = c.lookup(&rel, false, &enc);
                    // A current fingerprint is forever; a stale one gets
                    // today's file, but only for this visit.
                    let current = a.hash_of(&rel).as_deref() == Some(hash.as_str());
                    (found, current)
                })
                .await;
                match looked {
                    Ok((Lookup::Found(f), current)) => {
                        let mut opts = cfg.opts.clone();
                        opts.cache_control = Some(HeaderValue::from_static(if current {
                            "public, max-age=31536000, immutable"
                        } else {
                            "no-cache"
                        }));
                        return respond(&req, f, &opts).await;
                    }
                    Ok((other, _)) => other,
                    Err(e) => Lookup::Failed(io::Error::other(e)),
                }
            }
            None => Lookup::NotFound,
        },
        (found, _) => found,
    };
    match found {
        Lookup::Found(f) => respond(&req, f, &cfg.opts).await,
        Lookup::RedirectSlash => redirect_slash(&req),
        other => other.into_error_response(),
    }
}

/// `307` to the same path with a trailing slash (leading slashes and
/// backslashes collapsed so the target can never become a protocol-relative
/// `//host` or `/\host` URL).
fn redirect_slash(req: &Request) -> Response {
    let mut location = format!("/{}/", req.path().trim_start_matches(['/', '\\']));
    if let Some(q) = req.query_string() {
        location.push('?');
        location.push_str(q);
    }
    match HeaderValue::from_str(&location) {
        Ok(v) => {
            let mut res = StatusCode::TEMPORARY_REDIRECT.into_response();
            res.headers_mut().insert(LOCATION, v);
            res
        }
        Err(_) => Error::not_found().into_response(),
    }
}

impl Options {
    /// Acceptable precompressed encodings, best first.
    fn encodings(&self, headers: &HeaderMap) -> Vec<Encoding> {
        if !self.gzip && !self.br {
            return Vec::new();
        }
        let accept: Vec<&str> = headers
            .get_all(ACCEPT_ENCODING)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        let mut out: Vec<(Encoding, f32)> = [(Encoding::Br, self.br), (Encoding::Gzip, self.gzip)]
            .into_iter()
            .filter(|&(_, on)| on)
            .map(|(e, _)| (e, quality(&accept, e.token())))
            .filter(|&(_, q)| q > 0.0)
            .collect();
        // Stable sort keeps br ahead of gzip at equal quality.
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out.into_iter().map(|(e, _)| e).collect()
    }
}

/// The `q` value `Accept-Encoding` gives `coding` (explicitly or via `*`).
fn quality(accept: &[&str], coding: &str) -> f32 {
    let mut star = None;
    for item in accept.iter().flat_map(|v| v.split(',')) {
        let mut parts = item.split(';');
        let name = parts.next().unwrap_or("").trim();
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q="))
            .find_map(|q| q.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        if name.eq_ignore_ascii_case(coding) {
            return q;
        }
        if name == "*" {
            star = Some(q);
        }
    }
    star.unwrap_or(0.0)
}

async fn respond(req: &Request, found: Resolved, opts: &Options) -> Response {
    let Resolved {
        file,
        meta,
        path,
        encoding,
    } = found;
    let len = meta.len();
    let mtime = meta.modified().ok().filter(|t| http_date(*t).is_some());
    let etag = mtime.map(|t| etag(len, t, encoding));
    let last_modified = mtime.and_then(http_date);

    let mut headers = HeaderMap::new();
    if let Some(e) = &etag {
        headers.insert(ETAG, header(e));
    }
    if let Some(lm) = &last_modified {
        headers.insert(LAST_MODIFIED, header(lm));
    }
    if let Some(cc) = &opts.cache_control {
        headers.insert(CACHE_CONTROL, cc.clone());
    }
    if opts.gzip || opts.br {
        headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
    }

    let h = req.headers();
    if not_modified(h, etag.as_deref(), mtime) {
        let mut res = StatusCode::NOT_MODIFIED.into_response();
        *res.headers_mut() = headers;
        return res;
    }

    headers.insert(CONTENT_TYPE, content_type(&path));
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(enc) = encoding {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static(enc.token()));
    }

    let mut status = StatusCode::OK;
    let (mut start, mut count) = (0, len);
    let range = h.get(RANGE).and_then(|v| v.to_str().ok());
    if let (Some(range), true) = (range, req.method() == Method::GET) {
        if if_range_ok(h, mtime) {
            match parse_range(range, len) {
                ByteRange::Full => {}
                ByteRange::Unsatisfiable => {
                    let mut err =
                        Error::new(StatusCode::RANGE_NOT_SATISFIABLE, "range not satisfiable")
                            .with_code("range_not_satisfiable");
                    // Validators and caching headers, but not the file's
                    // content type: the error body is JSON.
                    for (name, value) in &headers {
                        if name != CONTENT_TYPE && name != CONTENT_ENCODING {
                            err = err.with_header(name.clone(), value.clone());
                        }
                    }
                    return err
                        .with_header(CONTENT_RANGE, header(&format!("bytes */{len}")))
                        .into_response();
                }
                ByteRange::Partial(a, b) => {
                    status = StatusCode::PARTIAL_CONTENT;
                    (start, count) = (a, b - a + 1);
                    headers.insert(CONTENT_RANGE, header(&format!("bytes {a}-{b}/{len}")));
                }
            }
        }
    }
    headers.insert(CONTENT_LENGTH, HeaderValue::from(count));

    let body = if req.method() == Method::HEAD || count == 0 {
        Body::Empty
    } else {
        match file_body(file, start, count).await {
            Ok(b) => b,
            Err(e) => {
                return Error::internal("failed to read file")
                    .with_source(e)
                    .into_response()
            }
        }
    };
    let mut res = Response::new(body);
    *res.status_mut() = status;
    *res.headers_mut() = headers;
    res
}

fn header(s: &str) -> HeaderValue {
    // Only called with values built from digits, hex and HTTP dates.
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn content_type(path: &Path) -> HeaderValue {
    let mime = mime_guess::from_path(path)
        .first_raw()
        .unwrap_or("application/octet-stream");
    if mime.starts_with("text/") || mime == "application/javascript" {
        header(&format!("{mime}; charset=utf-8"))
    } else {
        HeaderValue::from_static(mime)
    }
}

/// HTTP dates only cover 1970..=9999; other mtimes get no validators.
fn http_date(t: SystemTime) -> Option<String> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    (secs < 253_402_300_800).then(|| httpdate::fmt_http_date(t))
}

fn etag(len: u64, mtime: SystemTime, encoding: Option<Encoding>) -> String {
    let d = mtime.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let suffix = encoding.map_or("", |e| e.suffix());
    format!(
        "W/\"{len:x}-{:x}.{:x}{suffix}\"",
        d.as_secs(),
        d.subsec_nanos()
    )
}

fn not_modified(h: &HeaderMap, etag: Option<&str>, mtime: Option<SystemTime>) -> bool {
    if h.contains_key(IF_NONE_MATCH) {
        let Some(etag) = etag else { return false };
        return h
            .get_all(IF_NONE_MATCH)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .any(|t| t == "*" || weak_eq(t, etag));
    }
    match (h.get(IF_MODIFIED_SINCE), mtime) {
        (Some(ims), Some(mtime)) => parse_date(ims).is_some_and(|ims| secs(mtime) <= secs(ims)),
        _ => false,
    }
}

/// `If-Range` with a date must equal `Last-Modified`; with an entity tag it
/// needs a strong match, which our weak tags never give, so the full file
/// is sent.
fn if_range_ok(h: &HeaderMap, mtime: Option<SystemTime>) -> bool {
    let Some(v) = h.get(IF_RANGE) else {
        return true;
    };
    match (parse_date(v), mtime) {
        (Some(date), Some(mtime)) => secs(date) == secs(mtime),
        _ => false,
    }
}

fn weak_eq(a: &str, b: &str) -> bool {
    a.trim_start_matches("W/") == b.trim_start_matches("W/")
}

fn parse_date(v: &HeaderValue) -> Option<SystemTime> {
    httpdate::parse_http_date(v.to_str().ok()?).ok()
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[derive(Debug, PartialEq, Eq)]
enum ByteRange {
    /// No usable range (absent, malformed, or multiple): send everything.
    Full,
    /// Inclusive byte offsets.
    Partial(u64, u64),
    Unsatisfiable,
}

fn parse_range(header: &str, len: u64) -> ByteRange {
    let header = header.trim();
    let Some(spec) = header
        .get(..6)
        .filter(|unit| unit.eq_ignore_ascii_case("bytes="))
        .map(|_| &header[6..])
    else {
        return ByteRange::Full;
    };
    if spec.contains(',') {
        return ByteRange::Full;
    }
    let Some((a, b)) = spec.split_once('-') else {
        return ByteRange::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    let num = |s: &str| {
        if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
            None
        } else {
            s.parse::<u64>().ok()
        }
    };
    if a.is_empty() {
        // Suffix range: the last `n` bytes.
        let Some(n) = num(b) else {
            return ByteRange::Full;
        };
        if n == 0 || len == 0 {
            return ByteRange::Unsatisfiable;
        }
        return ByteRange::Partial(len.saturating_sub(n), len - 1);
    }
    let Some(start) = num(a) else {
        return ByteRange::Full;
    };
    let end = if b.is_empty() {
        u64::MAX
    } else {
        match num(b) {
            Some(end) if end >= start => end,
            _ => return ByteRange::Full,
        }
    };
    if start >= len {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Partial(start, end.min(len - 1))
}

/// Read `count` bytes at `start`: buffered for small files, streamed for
/// large ones.
async fn file_body(file: File, start: u64, count: u64) -> io::Result<Body> {
    if count <= SMALL_FILE {
        let buf = tokio::task::spawn_blocking(move || -> io::Result<Vec<u8>> {
            let mut file = file;
            file.seek(SeekFrom::Start(start))?;
            let mut buf = Vec::with_capacity(count as usize);
            file.take(count).read_to_end(&mut buf)?;
            Ok(buf)
        })
        .await
        .map_err(io::Error::other)??;
        if buf.len() as u64 != count {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file changed while being read",
            ));
        }
        return Ok(Body::from(buf));
    }
    let mut file = tokio::fs::File::from_std(file);
    file.seek(SeekFrom::Start(start)).await?;
    Ok(Body::stream(FileBody {
        reader: ReaderStream::with_capacity(file.take(count), CHUNK),
        remaining: count,
    }))
}

/// A file streamed in chunks with an exact size hint (so `Content-Length`
/// is known and truncation is detected).
struct FileBody {
    reader: ReaderStream<tokio::io::Take<tokio::fs::File>>,
    remaining: u64,
}

impl http_body::Body for FileBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match ready!(Pin::new(&mut self.reader).poll_next(cx)) {
            Some(Ok(chunk)) => {
                self.remaining = self.remaining.saturating_sub(chunk.len() as u64);
                Poll::Ready(Some(Ok(Frame::data(chunk))))
            }
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            None if self.remaining > 0 => {
                self.remaining = 0;
                Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file shrank while being served",
                ))))
            }
            None => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_names_reject_devices_streams_and_trailing_dots() {
        for bad in [
            "CON",
            "con",
            "nul.txt",
            "Aux.html",
            "COM1",
            "lpt9.log",
            "com\u{b9}",
            "CONIN$",
            "file.txt:stream",
            "C:",
            "secret.txt.",
            "secret.txt ",
            "index.html. ",
        ] {
            assert!(!windows_safe_name(bad), "{bad:?} should be rejected");
        }
        for good in [
            "index.html",
            "console.log",
            "com10",
            "lpt",
            "nullable.js",
            "a.b.c",
            "COMMIT",
            "configure",
        ] {
            assert!(windows_safe_name(good), "{good:?} should be allowed");
        }
    }

    #[test]
    fn ranges() {
        use ByteRange::*;
        assert_eq!(parse_range("bytes=0-4", 10), Partial(0, 4));
        assert_eq!(parse_range("bytes=5-", 10), Partial(5, 9));
        assert_eq!(parse_range("bytes=-3", 10), Partial(7, 9));
        assert_eq!(parse_range("bytes=-30", 10), Partial(0, 9));
        assert_eq!(parse_range("bytes=2-100", 10), Partial(2, 9));
        assert_eq!(parse_range("BYTES=1-1", 10), Partial(1, 1));
        assert_eq!(parse_range("bytes=10-", 10), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 10), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=5-2", 10), Full);
        assert_eq!(parse_range("bytes=0-1,3-4", 10), Full);
        assert_eq!(parse_range("bytes=+1-2", 10), Full);
        assert_eq!(parse_range("items=0-1", 10), Full);
        assert_eq!(parse_range("bytes=x", 10), Full);
        assert_eq!(parse_range("bytes=99999999999999999999-", 10), Full);
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("a/b.txt", false), Some(PathBuf::from("a/b.txt")));
        assert_eq!(sanitize("/a//./b/", false), Some(PathBuf::from("a/b")));
        assert_eq!(sanitize("", false), Some(PathBuf::new()));
        assert_eq!(sanitize("../x", false), None);
        assert_eq!(sanitize("a/../../x", false), None);
        assert_eq!(sanitize(".env", false), None);
        assert_eq!(sanitize(".env", true), Some(PathBuf::from(".env")));
        assert_eq!(sanitize("..", true), None);
        assert_eq!(sanitize("a\\..\\x", true), None);
        assert_eq!(sanitize("a\0b", true), None);
    }

    #[test]
    fn accept_encoding_quality() {
        let opts = Options {
            gzip: true,
            br: true,
            ..Options::default()
        };
        let enc = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(ACCEPT_ENCODING, HeaderValue::from_str(v).unwrap());
            opts.encodings(&h)
        };
        assert_eq!(enc("gzip, br"), vec![Encoding::Br, Encoding::Gzip]);
        assert_eq!(
            enc("gzip;q=1, br;q=0.5"),
            vec![Encoding::Gzip, Encoding::Br]
        );
        assert_eq!(enc("br;q=0, gzip"), vec![Encoding::Gzip]);
        assert_eq!(enc("*"), vec![Encoding::Br, Encoding::Gzip]);
        assert_eq!(enc("identity"), vec![]);
        assert_eq!(opts.encodings(&HeaderMap::new()), vec![]);
    }
}

// ---------------------------------------------------------------------------
// Fingerprinted assets

/// URLs with a content hash for the files of an [`App::assets`]
/// directory, so browsers and CDNs can cache them forever:
/// `assets.url("app.css")` is `/static/app.3f2a1b9c.css`, and changes when
/// the file does. A handler argument, and `asset("app.css")` in templates.
///
/// ```no_run
/// use zapline::fs::Assets;
/// use zapline::prelude::*;
///
/// #[get("/")]
/// async fn home(assets: Assets) -> Html<String> {
///     Html(format!("<link rel=stylesheet href=\"{}\">", assets.url("app.css")))
/// }
///
/// App::new().assets("/static", "static").mount(home).run("0.0.0.0:8080")
/// # ;
/// ```
#[derive(Clone)]
pub struct Assets(Arc<AssetsInner>);

struct AssetsInner {
    prefix: String,
    root: PathBuf,
    /// Relative path -> (size, mtime, hash).
    hashes: std::sync::Mutex<std::collections::HashMap<PathBuf, Fingerprint>>,
}

/// A file's size and modification time when it was hashed, and the hash.
type Fingerprint = (u64, Option<SystemTime>, String);

impl fmt::Debug for Assets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Assets")
            .field("prefix", &self.0.prefix)
            .field("root", &self.0.root)
            .finish()
    }
}

/// Hex digits in a fingerprint.
const HASH_LEN: usize = 8;

impl Assets {
    fn new(prefix: &str, root: PathBuf) -> Self {
        Assets(Arc::new(AssetsInner {
            prefix: prefix.trim().trim_end_matches('/').to_owned(),
            root,
            hashes: std::sync::Mutex::default(),
        }))
    }

    /// The URL of `path` (relative to the assets directory) with its
    /// content hash, or without one if the file doesn't exist.
    pub fn url(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        let Some(rel) = sanitize(path, false) else {
            return format!("{}/{path}", self.0.prefix);
        };
        let Some(hash) = self.hash_of(&rel) else {
            return format!("{}/{path}", self.0.prefix);
        };
        let (dir, file) = path.rsplit_once('/').map_or(("", path), |(d, f)| (d, f));
        let named = match file.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => format!("{stem}.{hash}.{ext}"),
            _ => format!("{file}.{hash}"),
        };
        if dir.is_empty() {
            format!("{}/{named}", self.0.prefix)
        } else {
            format!("{}/{dir}/{named}", self.0.prefix)
        }
    }

    /// The content hash of `rel`, recomputed when its size or mtime changes
    /// (in debug builds; release builds hash each file once).
    fn hash_of(&self, rel: &Path) -> Option<String> {
        let path = self.0.root.join(rel);
        let lock = || self.0.hashes.lock().unwrap_or_else(|e| e.into_inner());
        if !cfg!(debug_assertions) {
            if let Some((_, _, hash)) = lock().get(rel) {
                return Some(hash.clone());
            }
        }
        let meta = std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
        let (len, mtime) = (meta.len(), meta.modified().ok());
        if let Some((l, m, hash)) = lock().get(rel) {
            if *l == len && *m == mtime {
                return Some(hash.clone());
            }
        }
        let bytes = std::fs::read(&path).ok()?;
        // FNV-1a: stable across builds and platforms, and plenty for naming.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in &bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        let hash = format!("{:016x}", h)[..HASH_LEN].to_owned();
        lock().insert(rel.to_owned(), (len, mtime, hash.clone()));
        Some(hash)
    }

    /// `css/app.3f2a1b9c.css` -> (`css/app.css`, `3f2a1b9c`).
    fn unfingerprint(&self, tail: &str, show_hidden: bool) -> Option<(PathBuf, String)> {
        let (dir, file) = tail.rsplit_once('/').map_or(("", tail), |(d, f)| (d, f));
        let is_hash = |s: &str| s.len() == HASH_LEN && s.bytes().all(|b| b.is_ascii_hexdigit());
        let (plain, hash) = match file.rsplitn(3, '.').collect::<Vec<_>>()[..] {
            [ext, hash, stem] if is_hash(hash) && !stem.is_empty() => {
                (format!("{stem}.{ext}"), hash)
            }
            [hash, stem] if is_hash(hash) && !stem.is_empty() => (stem.to_owned(), hash),
            _ => return None,
        };
        let joined = if dir.is_empty() {
            plain
        } else {
            format!("{dir}/{plain}")
        };
        Some((sanitize(&joined, show_hidden)?, hash.to_ascii_lowercase()))
    }
}

impl crate::extract::FromRequestParts for Assets {
    async fn from_request_parts(req: &mut Request) -> crate::Result<Self> {
        req.state::<Assets>().cloned().ok_or_else(|| {
            Error::internal(
                "no assets registered: add `.assets(\"/static\", \"static\")` to the app",
            )
        })
    }
}

impl App {
    /// Serve `dir` under `prefix` like [`static_files`](Self::static_files),
    /// plus fingerprinted URLs from [`Assets::url`] (or `asset("app.css")`
    /// in templates) that browsers and CDNs may cache for a year: a new
    /// version of a file gets a new URL.
    ///
    /// ```
    /// let app = zapline::App::new().assets("/static", "static");
    /// ```
    pub fn assets(self, prefix: &str, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let assets = Assets::new(prefix, dir.clone());
        let mut serve = ServeDir::new(dir);
        serve.config().fingerprints = Some(assets.clone());
        let app = self.static_files(prefix, serve).state(assets.clone());
        #[cfg(feature = "views")]
        let app = app.on_startup(move |res: crate::lifecycle::Resources| async move {
            if let Some(views) = res.get::<crate::view::Views>() {
                res.insert(views.extended(move |env| {
                    let assets = assets.clone();
                    // Escaped here so `/` isn't turned into `&#x2f;`.
                    env.add_function("asset", move |path: &str| {
                        minijinja::Value::from_safe_string(crate::dev::escape(&assets.url(path)))
                    });
                }));
            }
            Ok(())
        });
        app
    }
}
