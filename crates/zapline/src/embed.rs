//! Single-binary apps: templates, static files, migrations and translations
//! compiled into the executable, so deploying is copying one file.
//!
//! `embed!` reads a directory at compile time and gives a
//! [`Dir`]. Everything that takes a directory takes a `Dir` too:
//!
//! ```no_run
//! # #[cfg(all(feature = "macros", feature = "views", feature = "fs"))] {
//! use zapline::prelude::*;
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         // Relative to Cargo.toml; usually just "templates" and "static".
//!         .views(zapline::embed!("tests/fixtures/embed/templates"))
//!         .assets("/static", zapline::embed!("tests/fixtures/embed/static"))
//!         .run("0.0.0.0:8080")
//! }
//! # }
//! ```
//!
//! The same calls with plain paths (`.views("templates")`) read the disk at
//! runtime instead; nothing else changes.
//!
//! | Takes a `Dir` | |
//! |---|---|
//! | [`App::views`](crate::App::views) | MiniJinja templates (`views`) |
//! | [`App::assets`](crate::App::assets), [`ServeDir::new`](crate::fs::ServeDir::new) | Static files with fingerprinted URLs (`fs`) |
//! | `Database::migrations` | SQL migrations (`postgres` / `sqlite`) |
//! | `I18n::load` | Translations (`i18n`) |
//!
//! # Debug and release
//!
//! Release builds read only the embedded copy: the files on disk are not
//! needed (or looked at) on the server. Debug builds read the directory on
//! disk while it exists, so editing a template or a stylesheet still shows
//! up on the next request without a rebuild; a debug binary copied to a
//! machine without the sources falls back to the embedded copy.
//! [`Dir::in_memory`] forces the embedded copy, for a test that wants to
//! see exactly what will ship.
//!
//! Paths are relative to the crate's `Cargo.toml`. Hidden files (`.gitkeep`,
//! `.DS_Store`, anything in a dot-directory) are left out. Changing a file
//! rebuilds the crate; cargo does not notice a file *added* to the
//! directory until something else changes, which is why `zapline ship`
//! passes `ZAPLINE_EMBED_STAMP` (a hash of the project's file list) to the
//! build. Set it yourself, or `touch src/lib.rs`, when building otherwise.
//!
//! Content hashes for [`Assets`](crate::fs::Assets) URLs are computed at
//! compile time, with the same function as for files on disk, so an asset
//! URL is the same whether it is served from memory or from disk.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The environment variable `embed!` lists as a dependency of the build:
/// changing its value makes cargo recompile the crate and so re-read the
/// embedded directories (picking up added and removed files).
pub const ENV_EMBED_STAMP: &str = "ZAPLINE_EMBED_STAMP";

/// A directory of files: on disk, or compiled into the binary by
/// `embed!`.
///
/// Build one from a path (`"templates".into()`, [`Dir::disk`]) or with
/// `embed!`; every API that takes a directory accepts either.
///
/// ```
/// use zapline::embed::Dir;
///
/// let dir = Dir::disk("templates");
/// assert!(!dir.is_embedded());
/// let same: Dir = "templates".into();
/// assert_eq!(same.path(), dir.path());
/// ```
#[derive(Clone)]
pub struct Dir(Arc<Inner>);

struct Inner {
    root: PathBuf,
    files: &'static [EmbeddedFile],
    /// Built by `embed!` (`files` is the compiled-in copy).
    embedded: bool,
    /// Reads come from `files` (else from `root` on disk).
    memory: bool,
    index: OnceLock<HashMap<&'static str, &'static EmbeddedFile>>,
}

/// One file compiled into the binary. Built by `embed!`; not meant to be
/// written by hand.
#[doc(hidden)]
#[derive(Debug)]
pub struct EmbeddedFile {
    /// Relative path with `/` separators.
    pub path: &'static str,
    pub bytes: &'static [u8],
    /// [`content_hash`] of `bytes`.
    pub hash: &'static str,
}

impl fmt::Debug for Dir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dir")
            .field("root", &self.0.root)
            .field("embedded_files", &self.0.files.len())
            .field("in_memory", &self.0.memory)
            .finish()
    }
}

impl Dir {
    /// The directory at `path` on disk, read at runtime (relative paths are
    /// resolved against the working directory).
    ///
    /// ```
    /// let dir = zapline::embed::Dir::disk("static");
    /// assert_eq!(dir.path(), std::path::Path::new("static"));
    /// ```
    pub fn disk(path: impl Into<PathBuf>) -> Self {
        Dir(Arc::new(Inner {
            root: path.into(),
            files: &[],
            embedded: false,
            memory: false,
            index: OnceLock::new(),
        }))
    }

    /// Used by `embed!`: `root` is the absolute source directory.
    #[doc(hidden)]
    pub fn __from_embedded(root: &str, files: &'static [EmbeddedFile]) -> Self {
        let root = PathBuf::from(root);
        // Debug builds edit-and-refresh from the sources while they exist.
        let memory = !cfg!(debug_assertions) || !root.is_dir();
        Dir(Arc::new(Inner {
            root,
            files,
            embedded: true,
            memory,
            index: OnceLock::new(),
        }))
    }

    /// Always read the embedded copy, even in a debug build next to the
    /// sources. No effect on a [`disk`](Self::disk) directory.
    ///
    /// ```
    /// # #[cfg(feature = "macros")] {
    /// let dir = zapline::embed!("src").in_memory();
    /// assert!(dir.is_embedded());
    /// assert!(dir.read("lib.rs").is_ok());
    /// # }
    /// ```
    pub fn in_memory(self) -> Self {
        if self.0.memory || !self.0.embedded {
            return self;
        }
        Dir(Arc::new(Inner {
            root: self.0.root.clone(),
            files: self.0.files,
            embedded: true,
            memory: true,
            index: OnceLock::new(),
        }))
    }

    /// Whether reads are served from the copy compiled into the binary.
    ///
    /// ```
    /// assert!(!zapline::embed::Dir::disk("static").is_embedded());
    /// ```
    pub fn is_embedded(&self) -> bool {
        self.0.memory
    }

    /// The directory on disk: the source directory for `embed!` (an
    /// absolute path), or the path given to [`disk`](Self::disk).
    ///
    /// ```
    /// assert_eq!(zapline::embed::Dir::disk("a/b").path(), std::path::Path::new("a/b"));
    /// ```
    pub fn path(&self) -> &Path {
        &self.0.root
    }

    /// The contents of `path` (relative, `/`-separated). Embedded files are
    /// borrowed from the binary; files on disk are read.
    ///
    /// # Errors
    ///
    /// `NotFound` when there is no such file, or `path` tries to leave the
    /// directory (`..`, an absolute path); any I/O error from the disk.
    ///
    /// ```
    /// # #[cfg(feature = "macros")] {
    /// let src = zapline::embed!("src");
    /// assert!(src.read("lib.rs")?.starts_with(b"//!"));
    /// assert!(src.read("../Cargo.toml").is_err());
    /// # }
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn read(&self, path: &str) -> io::Result<Cow<'static, [u8]>> {
        if self.0.memory {
            return self
                .file(path)
                .map(|f| Cow::Borrowed(f.bytes))
                .ok_or_else(|| io::ErrorKind::NotFound.into());
        }
        let rel = relative(path).ok_or(io::ErrorKind::NotFound)?;
        std::fs::read(self.0.root.join(rel)).map(Cow::Owned)
    }

    /// The contents of `path` as UTF-8 text.
    ///
    /// # Errors
    ///
    /// As [`read`](Self::read), and `InvalidData` when the file is not UTF-8.
    ///
    /// ```
    /// # #[cfg(feature = "macros")] {
    /// let text = zapline::embed!("src").read_to_string("lib.rs")?;
    /// assert!(text.contains("zapline"));
    /// # }
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn read_to_string(&self, path: &str) -> io::Result<String> {
        match self.read(path)? {
            Cow::Borrowed(b) => std::str::from_utf8(b)
                .map(str::to_owned)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Cow::Owned(v) => {
                String::from_utf8(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
        }
    }

    /// Every file, as sorted relative `/`-separated paths (hidden files
    /// skipped, as `embed!` does).
    ///
    /// # Errors
    ///
    /// When the directory on disk can't be read (never for embedded files).
    ///
    /// ```
    /// # #[cfg(feature = "macros")] {
    /// let files = zapline::embed!("src").files()?;
    /// assert!(files.iter().any(|f| f == "lib.rs"));
    /// # }
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn files(&self) -> io::Result<Vec<String>> {
        if self.0.memory {
            let mut out: Vec<String> = self.0.files.iter().map(|f| f.path.to_owned()).collect();
            out.sort();
            return Ok(out);
        }
        let mut out = Vec::new();
        walk(&self.0.root, "", &mut out)?;
        out.sort();
        Ok(out)
    }

    /// Whether the directory exists: always for embedded files, else
    /// whether the path is a directory on disk.
    ///
    /// ```
    /// assert!(!zapline::embed::Dir::disk("no/such/dir").exists());
    /// ```
    pub fn exists(&self) -> bool {
        self.0.memory || self.0.root.is_dir()
    }

    /// The embedded file at `path`, when reads come from memory.
    pub(crate) fn file(&self, path: &str) -> Option<&'static EmbeddedFile> {
        if !self.0.memory {
            return None;
        }
        let index = self
            .0
            .index
            .get_or_init(|| self.0.files.iter().map(|f| (f.path, f)).collect());
        index.get(path.trim_start_matches('/')).copied()
    }
}

impl From<PathBuf> for Dir {
    fn from(path: PathBuf) -> Self {
        Dir::disk(path)
    }
}

impl From<String> for Dir {
    fn from(path: String) -> Self {
        Dir::disk(path)
    }
}

impl From<std::ffi::OsString> for Dir {
    fn from(path: std::ffi::OsString) -> Self {
        Dir::disk(path)
    }
}

impl<T: AsRef<OsStr> + ?Sized> From<&T> for Dir {
    fn from(path: &T) -> Self {
        Dir::disk(PathBuf::from(path.as_ref()))
    }
}

/// `path` as a relative path inside the directory, or `None` when it would
/// leave it.
fn relative(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in Path::new(path.trim_start_matches('/')).components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let meta = std::fs::metadata(entry.path())?;
        if meta.is_dir() {
            walk(&entry.path(), &rel, out)?;
        } else if meta.is_file() {
            out.push(rel);
        }
    }
    Ok(())
}

/// Hex digits in a content hash.
pub(crate) const HASH_LEN: usize = 8;

/// The content hash used in fingerprinted asset URLs: the first 8 hex
/// digits of FNV-1a (64-bit), stable across builds and platforms. `embed!`
/// computes the same value at compile time.
///
/// ```
/// assert_eq!(zapline::embed::content_hash(b"body { color: red }").len(), 8);
/// ```
pub fn content_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")[..HASH_LEN].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    static FILES: &[EmbeddedFile] = &[
        EmbeddedFile {
            path: "a.txt",
            bytes: b"alpha",
            hash: "00000000",
        },
        EmbeddedFile {
            path: "sub/b.txt",
            bytes: b"beta",
            hash: "00000000",
        },
    ];

    #[test]
    fn memory_reads_and_lists() {
        let dir = Dir::__from_embedded("/definitely/not/here", FILES);
        assert!(dir.is_embedded());
        assert_eq!(&*dir.read("a.txt").unwrap(), b"alpha");
        assert_eq!(&*dir.read("/sub/b.txt").unwrap(), b"beta");
        assert!(dir.read("../a.txt").is_err());
        assert_eq!(dir.files().unwrap(), ["a.txt", "sub/b.txt"]);
    }

    #[test]
    fn relative_paths_stay_inside() {
        assert_eq!(relative("a/./b"), Some(PathBuf::from("a/b")));
        assert_eq!(relative("/a"), Some(PathBuf::from("a")));
        assert_eq!(relative("a/../b"), None);
    }
}
