//! Layered configuration: TOML files, a `.env` file, environment variables
//! and programmatic overrides, merged into one tree and deserialized into
//! your own typed settings.
//!
//! [`Config::load`] uses the standard layers, lowest precedence first:
//!
//! | Layer | Example |
//! |---|---|
//! | `config/default.toml` (optional) | `[database]`<br>`url = "postgres://localhost/app"` |
//! | `config/{profile}.toml` (optional) | `config/production.toml` |
//! | `.env` (optional, never overrides real env vars) | `APP__DATABASE__POOL=20` |
//! | environment variables `APP__*` | `APP__DATABASE__URL` → `database.url` |
//!
//! The profile comes from `APP_ENV`, then `ZAPLINE_ENV`, and defaults to
//! `development`. Tables are deep-merged; later layers override single keys.
//! Env var names are split on `__` and lowercased (an existing key with
//! other casing is matched case-insensitively); values are read as TOML
//! scalars or arrays where they round-trip exactly (`8080`, `true`,
//! `[1, 2]`) and as strings otherwise. Typed extraction is lenient where env
//! vars need it: `"8080"` reads as a number, `8080` as a string, `"a,b"` as a
//! list, `"yes"`/`"on"`/`"1"` as `true`.
//!
//! ```no_run
//! use serde::Deserialize;
//! use zapline::config::{Config, Secret};
//! use zapline::prelude::*;
//!
//! #[derive(Clone, Deserialize)]
//! struct Settings {
//!     server: Server,
//!     database: Database,
//! }
//!
//! #[derive(Clone, Deserialize)]
//! struct Server { port: u16 }
//!
//! #[derive(Clone, Deserialize)]
//! struct Database { url: Secret<String>, pool: u32 }
//!
//! fn main() -> std::io::Result<()> {
//!     let config = Config::load()?;
//!     let settings: Settings = config.extract()?;
//!     let addr = ("0.0.0.0", settings.server.port);
//!     App::new()
//!         .config(settings) // State<Settings> and Dep<Settings>
//!         .run(addr)
//! }
//! ```
//!
//! Use [`Config::builder`] for full control (other files, prefixes, inline
//! TOML, overrides, an injected environment for tests).

mod de;
mod dotenv;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

pub use toml::{Table, Value};

use self::de::{DeError, MissingDe, Seg, ValueDe};
use crate::App;

/// Default prefix for environment variables (`APP__DATABASE__URL`).
pub const DEFAULT_ENV_PREFIX: &str = "APP";
/// Default nesting separator in environment variable names.
pub const DEFAULT_SEPARATOR: &str = "__";
/// Profile used when none is set.
pub const DEFAULT_PROFILE: &str = "development";

// ---------------------------------------------------------------------------
// Config

/// A merged configuration tree. Cheap to clone.
///
/// ```
/// use zapline::config::Config;
///
/// let config = Config::builder()
///     .toml_str("[server]\nport = 8080\nhosts = [\"a\", \"b\"]")
///     .set("server.port", 9090)
///     .build()
///     .unwrap();
/// assert_eq!(config.get::<u16>("server.port").unwrap(), 9090);
/// assert_eq!(config.get::<String>("server.hosts[1]").unwrap(), "b");
/// assert_eq!(config.get::<Option<u32>>("server.workers").unwrap(), None);
/// ```
#[derive(Clone)]
pub struct Config {
    inner: Arc<Inner>,
}

struct Inner {
    root: Value,
    profile: String,
    origins: BTreeMap<String, String>,
}

impl Config {
    /// Load the standard layers (see the [module docs](self)).
    pub fn load() -> Result<Config, ConfigError> {
        Config::builder().standard().build()
    }

    /// Start a custom layered configuration.
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::default()
    }

    /// The active profile, e.g. `development` or `production`.
    pub fn profile(&self) -> &str {
        &self.inner.profile
    }

    /// Deserialize the whole tree into `T`.
    pub fn extract<T: DeserializeOwned>(&self) -> Result<T, ConfigError> {
        T::deserialize(ValueDe(&self.inner.root)).map_err(|e| self.error("", e))
    }

    /// Deserialize the value at a dotted `key` (`database.url`,
    /// `servers[0].host` or `servers.0.host`). A missing key is an error
    /// unless `T` is an `Option`.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<T, ConfigError> {
        match self.value(key) {
            Some(v) => T::deserialize(ValueDe(v)).map_err(|e| self.error(key, e)),
            None => T::deserialize(MissingDe).map_err(|e| self.error(key, e)),
        }
    }

    /// Like [`get`](Self::get), but `default` when the key is missing (a
    /// present but invalid value is still an error).
    pub fn get_or<T: DeserializeOwned>(&self, key: &str, default: T) -> Result<T, ConfigError> {
        match self.value(key) {
            Some(v) => T::deserialize(ValueDe(v)).map_err(|e| self.error(key, e)),
            None => Ok(default),
        }
    }

    /// Whether `key` is present.
    pub fn contains(&self, key: &str) -> bool {
        self.value(key).is_some()
    }

    /// The raw value at `key`.
    pub fn value(&self, key: &str) -> Option<&Value> {
        let mut cur = &self.inner.root;
        for seg in parse_key(key) {
            cur = match (cur, seg) {
                (Value::Table(t), Seg::Key(k)) => table_get(t, &k)?,
                (Value::Array(items), Seg::Index(i)) => items.get(i)?,
                (Value::Array(items), Seg::Key(k)) => items.get(k.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    /// The whole merged tree.
    pub fn table(&self) -> &Table {
        match &self.inner.root {
            Value::Table(t) => t,
            _ => unreachable!("config root is always a table"),
        }
    }

    /// Which layer set `key` (e.g. ``env `APP__SERVER__PORT` ``), for
    /// diagnostics.
    pub fn origin(&self, key: &str) -> Option<&str> {
        let mut key = key;
        loop {
            if let Some(o) = self.inner.origins.get(key) {
                return Some(o);
            }
            key = parent_key(key)?;
        }
    }

    fn error(&self, prefix: &str, e: DeError) -> ConfigError {
        let key = join_key(prefix, &e.path());
        if e.is_missing() {
            return ConfigError::Missing { key };
        }
        let origin = self.origin(&key).map(str::to_owned);
        ConfigError::Invalid {
            key,
            message: e.message().to_owned(),
            origin,
        }
    }
}

impl fmt::Debug for Config {
    /// Shows the profile and top-level keys only: values may be secrets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("profile", &self.inner.profile)
            .field("keys", &self.table().keys().collect::<Vec<_>>())
            .finish()
    }
}

fn table_get<'a>(t: &'a Table, key: &str) -> Option<&'a Value> {
    t.get(key).or_else(|| {
        t.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

/// `a.b[2].c` / `a.b.2.c` into segments. Numeric segments are resolved
/// against arrays at lookup time.
fn parse_key(key: &str) -> Vec<Seg> {
    let mut segs = Vec::new();
    for part in key.split('.').filter(|p| !p.is_empty()) {
        let (name, mut rest) = match part.find('[') {
            Some(i) => part.split_at(i),
            None => (part, ""),
        };
        if !name.is_empty() {
            segs.push(Seg::Key(name.to_owned()));
        }
        while let Some(inner) = rest.strip_prefix('[') {
            let Some((index, tail)) = inner.split_once(']') else {
                segs.push(Seg::Key(rest.to_owned()));
                break;
            };
            match index.parse() {
                Ok(i) => segs.push(Seg::Index(i)),
                Err(_) => segs.push(Seg::Key(index.to_owned())),
            }
            rest = tail;
        }
    }
    segs
}

fn join_key(prefix: &str, key: &str) -> String {
    match (prefix.is_empty(), key.is_empty()) {
        (true, _) => key.to_owned(),
        (_, true) => prefix.to_owned(),
        _ if key.starts_with('[') => format!("{prefix}{key}"),
        _ => format!("{prefix}.{key}"),
    }
}

fn parent_key(key: &str) -> Option<&str> {
    if key.is_empty() {
        return None;
    }
    Some(key.rfind(['.', '[']).map_or("", |i| &key[..i]))
}

// ---------------------------------------------------------------------------
// Errors

/// Why configuration could not be loaded or extracted. Converts into
/// [`zapline::Error`](crate::Error) (a 500) and `std::io::Error` with `?`.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigError {
    /// A required file could not be read (or an optional one exists but is
    /// unreadable).
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A TOML source or `.env` file is malformed.
    Parse { origin: String, message: String },
    /// A required key is missing.
    Missing { key: String },
    /// A value could not be deserialized into the expected type.
    Invalid {
        key: String,
        message: String,
        /// The layer that set the value, e.g. ``env `APP__SERVER__PORT` ``.
        origin: Option<String>,
    },
    /// The profile name contains characters other than ASCII letters,
    /// digits, `-` and `_`.
    InvalidProfile { profile: String },
}

impl ConfigError {
    /// The key path the error refers to, if any.
    pub fn key(&self) -> Option<&str> {
        match self {
            ConfigError::Missing { key } | ConfigError::Invalid { key, .. } => Some(key),
            _ => None,
        }
    }

    /// Its [diagnostic code](crate::codes), e.g. `"ZL0032"` for a missing
    /// key.
    ///
    /// ```
    /// use zapline::config::Config;
    ///
    /// let config = Config::builder().toml_str("name = \"demo\"").build().unwrap();
    /// let err = config.get::<u16>("server.port").unwrap_err();
    /// assert_eq!(err.diagnostic_code(), "ZL0032");
    /// ```
    pub fn diagnostic_code(&self) -> &'static str {
        match self {
            ConfigError::Io { .. } => "ZL0030",
            ConfigError::Parse { .. } => "ZL0031",
            ConfigError::Missing { .. } => "ZL0032",
            ConfigError::Invalid { .. } => "ZL0033",
            ConfigError::InvalidProfile { .. } => "ZL0034",
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, source } => {
                write!(f, "config file `{}`: {source}", path.display())
            }
            ConfigError::Parse { origin, message } => {
                write!(f, "config {origin} is invalid: {}", message.trim_end())
            }
            ConfigError::Missing { key } => write!(f, "missing config key `{key}`"),
            ConfigError::Invalid {
                key,
                message,
                origin,
            } => {
                if key.is_empty() {
                    write!(f, "invalid configuration: {message}")?;
                } else {
                    write!(f, "invalid config key `{key}`: {message}")?;
                }
                if let Some(origin) = origin {
                    write!(f, " (set by {origin})")?;
                }
                Ok(())
            }
            ConfigError::InvalidProfile { profile } => write!(
                f,
                "invalid config profile {profile:?}: use ASCII letters, digits, `-` and `_`"
            ),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<ConfigError> for std::io::Error {
    fn from(e: ConfigError) -> Self {
        let kind = match &e {
            ConfigError::Io { source, .. } => source.kind(),
            _ => std::io::ErrorKind::InvalidData,
        };
        std::io::Error::new(kind, e)
    }
}

// ---------------------------------------------------------------------------
// Builder

enum Source {
    File { path: PathBuf, required: bool },
    Toml { name: String, text: String },
}

type Assignment = (String, Result<Value, String>);

/// Builds a [`Config`] from layers. Files and TOML strings merge in the
/// order added; then the `.env` file and environment variables; then
/// [`set`](Self::set) overrides. [`set_default`](Self::set_default) values
/// sit below everything.
///
/// ```
/// use zapline::config::Config;
///
/// let config = Config::builder()
///     .toml_str("[database]\nurl = \"postgres://localhost/dev\"\npool = 5")
///     .env_prefix("MYAPP")
///     .env_source([("MYAPP__DATABASE__POOL", "20")]) // instead of the process env
///     .build()
///     .unwrap();
/// assert_eq!(config.get::<u32>("database.pool").unwrap(), 20);
/// ```
pub struct ConfigBuilder {
    sources: Vec<Source>,
    defaults: Vec<Assignment>,
    overrides: Vec<Assignment>,
    env_prefix: Option<String>,
    separator: String,
    dotenv: Option<PathBuf>,
    env: Option<Vec<(String, String)>>,
    profile: Option<String>,
    base_dir: Option<PathBuf>,
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        ConfigBuilder {
            sources: Vec::new(),
            defaults: Vec::new(),
            overrides: Vec::new(),
            env_prefix: None,
            separator: DEFAULT_SEPARATOR.to_owned(),
            dotenv: None,
            env: None,
            profile: None,
            base_dir: None,
        }
    }
}

impl ConfigBuilder {
    /// The layers [`Config::load`] uses: `config/default.toml`,
    /// `config/{profile}.toml`, `.env` and `APP__*` variables (keeping any
    /// prefix or `.env` path already set).
    pub fn standard(mut self) -> Self {
        if self.env_prefix.is_none() {
            self.env_prefix = Some(DEFAULT_ENV_PREFIX.to_owned());
        }
        if self.dotenv.is_none() {
            self.dotenv = Some(PathBuf::from(".env"));
        }
        self.file_optional("config/default.toml")
            .file_optional("config/{profile}.toml")
    }

    /// A required TOML file. `{profile}` in the path is replaced by the
    /// active profile.
    pub fn file(mut self, path: impl AsRef<Path>) -> Self {
        self.sources.push(Source::File {
            path: path.as_ref().to_owned(),
            required: true,
        });
        self
    }

    /// A TOML file that is skipped if it does not exist.
    pub fn file_optional(mut self, path: impl AsRef<Path>) -> Self {
        self.sources.push(Source::File {
            path: path.as_ref().to_owned(),
            required: false,
        });
        self
    }

    /// An inline TOML document (e.g. embedded defaults via `include_str!`).
    pub fn toml_str(mut self, toml: impl Into<String>) -> Self {
        let name = format!("string #{}", self.sources.len() + 1);
        self.sources.push(Source::Toml {
            name,
            text: toml.into(),
        });
        self
    }

    /// Enable the environment layer: `{prefix}{separator}A{separator}B=v`
    /// sets `a.b`. Also makes `{prefix}_ENV` select the profile.
    pub fn env_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.env_prefix = Some(prefix.into());
        self
    }

    /// Nesting separator in env var names (default `__`).
    ///
    /// # Panics
    /// If `separator` is empty.
    pub fn separator(mut self, separator: impl Into<String>) -> Self {
        let separator = separator.into();
        assert!(!separator.is_empty(), "config separator must not be empty");
        self.separator = separator;
        self
    }

    /// Read variables from a `.env` file (skipped if missing). They feed the
    /// environment layer and profile selection, never override variables
    /// that are already set, and are not exported to the process.
    pub fn dotenv(mut self, path: impl AsRef<Path>) -> Self {
        self.dotenv = Some(path.as_ref().to_owned());
        self
    }

    /// Use these variables instead of the process environment (tests,
    /// sandboxes).
    pub fn env_source<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env = Some(
            vars.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        );
        self
    }

    /// Force the profile instead of reading `APP_ENV` / `ZAPLINE_ENV`.
    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Resolve relative file and `.env` paths against `dir` instead of the
    /// working directory.
    pub fn base_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.base_dir = Some(dir.as_ref().to_owned());
        self
    }

    /// Override `key` (dotted path) with any serializable value. Highest
    /// precedence.
    pub fn set(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        self.overrides.push(assignment(key.into(), value));
        self
    }

    /// A fallback for `key`, below every other layer.
    pub fn set_default(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        self.defaults.push(assignment(key.into(), value));
        self
    }

    /// Read and merge every layer.
    pub fn build(self) -> Result<Config, ConfigError> {
        let mut env: HashMap<String, String> = match self.env {
            Some(vars) => vars.into_iter().collect(),
            None => std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
        };
        if let Some(path) = &self.dotenv {
            let path = resolve(self.base_dir.as_deref(), path);
            if let Some(text) = read_optional(&path)? {
                let vars = dotenv::parse(&text).map_err(|message| ConfigError::Parse {
                    origin: format!("file `{}`", path.display()),
                    message,
                })?;
                for (k, v) in vars {
                    env.entry(k).or_insert(v);
                }
            }
        }

        let profile = select_profile(self.profile, self.env_prefix.as_deref(), &env)?;
        let mut merger = Merger::default();

        for (key, value) in self.defaults {
            merger.assign(&key, value, "default")?;
        }
        for source in self.sources {
            let (origin, text) = match source {
                Source::File { path, required } => {
                    let path = resolve(self.base_dir.as_deref(), &with_profile(&path, &profile));
                    let text = if required {
                        std::fs::read_to_string(&path)
                            .map_err(|source| ConfigError::Io {
                                path: path.clone(),
                                source,
                            })
                            .map(Some)?
                    } else {
                        read_optional(&path)?
                    };
                    let Some(text) = text else { continue };
                    (format!("file `{}`", path.display()), text)
                }
                Source::Toml { name, text } => (name, text),
            };
            let table = text.parse::<Table>().map_err(|e| ConfigError::Parse {
                origin: origin.clone(),
                message: e.to_string(),
            })?;
            merger.merge(table, &origin);
        }
        if let Some(prefix) = &self.env_prefix {
            let mut vars: Vec<(&String, Vec<String>, &String)> = env
                .iter()
                .filter_map(|(name, raw)| {
                    Some((name, env_path(name, prefix, &self.separator)?, raw))
                })
                .collect();
            vars.sort_by(|a, b| a.0.cmp(b.0));
            for (name, path, raw) in vars {
                let origin = format!("env `{name}`");
                merger.set(&path, parse_env_value(raw), &origin, true);
            }
        }
        for (key, value) in self.overrides {
            merger.assign(&key, value, "override")?;
        }

        Ok(Config {
            inner: Arc::new(Inner {
                root: Value::Table(merger.root),
                profile,
                origins: merger.origins,
            }),
        })
    }
}

fn assignment(key: String, value: impl Serialize) -> Assignment {
    let value = Value::try_from(value).map_err(|e| e.to_string());
    (key, value)
}

fn resolve(base: Option<&Path>, path: &Path) -> PathBuf {
    match base {
        Some(base) if path.is_relative() => base.join(path),
        _ => path.to_owned(),
    }
}

fn with_profile(path: &Path, profile: &str) -> PathBuf {
    match path.to_str() {
        Some(s) if s.contains("{profile}") => PathBuf::from(s.replace("{profile}", profile)),
        _ => path.to_owned(),
    }
}

/// `Ok(None)` if the file does not exist.
fn read_optional(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn select_profile(
    explicit: Option<String>,
    prefix: Option<&str>,
    env: &HashMap<String, String>,
) -> Result<String, ConfigError> {
    let prefix = prefix
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_ENV_PREFIX);
    let profile = explicit
        .or_else(|| env.get(&format!("{prefix}_ENV")).cloned())
        .filter(|p| !p.is_empty())
        .or_else(|| env.get("ZAPLINE_ENV").cloned())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let valid = profile.len() <= 64
        && profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(profile)
    } else {
        Err(ConfigError::InvalidProfile { profile })
    }
}

/// `APP__DATABASE__URL` -> `["database", "url"]`.
fn env_path(name: &str, prefix: &str, separator: &str) -> Option<Vec<String>> {
    let head = name.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = name[prefix.len()..].strip_prefix(separator)?;
    let path: Vec<String> = rest.split(separator).map(str::to_ascii_lowercase).collect();
    if path.iter().any(String::is_empty) {
        return None;
    }
    Some(path)
}

/// Env values become TOML scalars/arrays only when that round-trips exactly
/// (so `007`, `0x1F` or `1.10` stay strings), else strings.
fn parse_env_value(raw: &str) -> Value {
    let t = raw.trim();
    if t.starts_with('[') || t.starts_with('{') {
        return match t.parse::<Value>() {
            Ok(v @ (Value::Array(_) | Value::Table(_))) => v,
            _ => Value::String(raw.to_owned()),
        };
    }
    match t {
        "true" => return Value::Boolean(true),
        "false" => return Value::Boolean(false),
        _ => {}
    }
    if let Ok(i) = t.parse::<i64>() {
        if i.to_string() == t {
            return Value::Integer(i);
        }
    }
    if let Ok(f) = t.parse::<f64>() {
        if f.is_finite() && f.to_string() == t {
            return Value::Float(f);
        }
    }
    Value::String(raw.to_owned())
}

/// Deep-merges layers and remembers which layer set each key.
#[derive(Default)]
struct Merger {
    root: Table,
    origins: BTreeMap<String, String>,
}

impl Merger {
    fn assign(
        &mut self,
        key: &str,
        value: Result<Value, String>,
        origin: &str,
    ) -> Result<(), ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            key: key.to_owned(),
            message,
            origin: Some(origin.to_owned()),
        };
        let value = value.map_err(|e| invalid(format!("cannot convert value: {e}")))?;
        let path: Vec<String> = key.split('.').map(str::to_owned).collect();
        if path.iter().any(String::is_empty) {
            return Err(invalid("empty key segment".to_owned()));
        }
        self.set(&path, value, origin, false);
        Ok(())
    }

    fn merge(&mut self, table: Table, origin: &str) {
        merge_into(&mut self.root, table, "", origin, &mut self.origins);
    }

    /// Set `path` to `value`, creating (or replacing non-table values with)
    /// intermediate tables. With `fold_case`, path segments reuse an existing
    /// key that differs only in ASCII case.
    fn set(&mut self, path: &[String], value: Value, origin: &str, fold_case: bool) {
        set_in(
            &mut self.root,
            path,
            value,
            "",
            origin,
            fold_case,
            &mut self.origins,
        );
    }
}

fn set_in(
    table: &mut Table,
    path: &[String],
    value: Value,
    prefix: &str,
    origin: &str,
    fold_case: bool,
    origins: &mut BTreeMap<String, String>,
) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    let key = if fold_case && !table.contains_key(first) {
        table
            .keys()
            .find(|k| k.eq_ignore_ascii_case(first))
            .cloned()
            .unwrap_or_else(|| first.clone())
    } else {
        first.clone()
    };
    let full = join_key(prefix, &key);
    if rest.is_empty() {
        match (table.get_mut(&key), value) {
            (Some(Value::Table(existing)), Value::Table(new)) => {
                merge_into(existing, new, &full, origin, origins);
            }
            (_, value) => {
                record(origins, &full, &value, origin);
                table.insert(key, value);
            }
        }
        return;
    }
    if !matches!(table.get(&key), Some(Value::Table(_))) {
        table.insert(key.clone(), Value::Table(Table::new()));
    }
    if let Some(Value::Table(child)) = table.get_mut(&key) {
        set_in(child, rest, value, &full, origin, fold_case, origins);
    }
}

fn merge_into(
    dst: &mut Table,
    src: Table,
    prefix: &str,
    origin: &str,
    origins: &mut BTreeMap<String, String>,
) {
    for (key, value) in src {
        let full = join_key(prefix, &key);
        match (dst.get_mut(&key), value) {
            (Some(Value::Table(existing)), Value::Table(new)) => {
                merge_into(existing, new, &full, origin, origins);
            }
            (_, value) => {
                record(origins, &full, &value, origin);
                dst.insert(key, value);
            }
        }
    }
}

fn record(origins: &mut BTreeMap<String, String>, key: &str, value: &Value, origin: &str) {
    origins.insert(key.to_owned(), origin.to_owned());
    if let Value::Table(t) = value {
        for (k, v) in t {
            record(origins, &join_key(key, k), v, origin);
        }
    }
}

// ---------------------------------------------------------------------------
// App integration and secrets

impl App {
    /// Register typed settings, available as `State<T>` and [`Dep<T>`].
    ///
    /// ```no_run
    /// # use serde::Deserialize;
    /// # use zapline::prelude::*;
    /// # use zapline::config::Config;
    /// #[derive(Clone, Deserialize)]
    /// struct Settings { greeting: String }
    ///
    /// async fn hello(State(s): State<Settings>) -> String { s.greeting }
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let settings: Settings = Config::load()?.extract()?;
    /// App::new().config(settings).get("/", hello).run("0.0.0.0:8080")
    /// # }
    /// ```
    ///
    /// [`Dep<T>`]: crate::di::Dep
    pub fn config<T: Clone + Send + Sync + 'static>(self, settings: T) -> Self {
        self.state(settings.clone()).provide(settings)
    }
}

/// A value that must not end up in logs: `Debug` and `Display` print
/// `[REDACTED]`. Deserializes transparently; read it with
/// [`expose`](Self::expose).
///
/// ```
/// use zapline::config::Secret;
///
/// let key = Secret::new("hunter2".to_string());
/// assert_eq!(format!("{key:?}"), "[REDACTED]");
/// assert_eq!(key.expose(), "hunter2");
/// ```
#[derive(Clone, Default)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wrap a sensitive value.
    pub fn new(value: T) -> Self {
        Secret(value)
    }

    /// The secret value. Grep-able on purpose.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Unwrap the secret value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Secret(value)
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Secret)
    }
}
