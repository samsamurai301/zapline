//! zapline's Cargo features: what each one unlocks, `zapline add`, and the
//! manifest editing behind it (also used by `zapline g auth`).
//!
//! Most of zapline is opt-in. Code copied from the guide that uses a module
//! behind a feature that is off fails with a bare `E0432 could not find `ws`
//! in `zapline``. [`FEATURES`] maps every feature to the paths, types and
//! builder methods it unlocks, so `zapline check` / `zapline test` can say
//! which feature to enable (see [`missing_for`]), and `zapline add` turns it
//! on without hand-editing `Cargo.toml` (see [`add_features`]).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};

/// One of zapline's Cargo features.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feature {
    /// The feature name, as in `features = ["ws"]`.
    pub name: &'static str,
    /// One line for `zapline add --list`.
    pub description: &'static str,
    /// Paths under `zapline::` that need it: modules (`ws`, `auth::jwt`) or
    /// items (`Json`, `middleware::Compression`).
    pub paths: &'static [&'static str],
    /// Type, trait and macro names that need it, as rustc prints them in
    /// "cannot find type `WebSocketUpgrade`".
    pub items: &'static [&'static str],
    /// Methods and associated functions it adds, as `Type::method`
    /// (`App::views`, `Storage::s3`).
    pub methods: &'static [&'static str],
    /// The other public features it turns on (`default` for `full`).
    pub implies: &'static [&'static str],
    /// How to start using it, printed after `zapline add`.
    pub usage: &'static str,
}

/// The features in `default` (on unless `default-features = false`).
pub const DEFAULT_FEATURES: &[&str] = &[
    "macros", "json", "query", "form", "openapi", "validate", "config", "tracing",
];

const DB_PATHS: &[&str] = &["db", "admin", "jobs"];
const DB_ITEMS: &[&str] = &[
    "Db",
    "Database",
    "DbCheck",
    "Factory",
    "Listing",
    "ListParams",
    "Paged",
    "Jobs",
    "JobQueue",
    "Admin",
];
const DB_METHODS: &[&str] = &[
    "App::database",
    "App::seed",
    "App::jobs",
    "App::jobs_in",
    "App::admin",
    "App::admin_in",
];

/// Every public zapline feature (not `default`, not the `_internal` ones).
pub const FEATURES: &[Feature] = &[
    Feature {
        name: "macros",
        description: "#[get], #[post], ... route macros and #[model] (default)",
        paths: &[
            "get", "post", "put", "patch", "delete", "head", "options", "route", "model",
        ],
        items: &[],
        methods: &[],
        implies: &[],
        usage: "#[get(\"/\")] async fn index() -> &'static str { \"hi\" }",
    },
    Feature {
        name: "json",
        description: "Json<T> extractor and response (default)",
        paths: &["Json", "extract::Json"],
        items: &["Json"],
        methods: &[],
        implies: &[],
        usage: "Json(body): Json<T>",
    },
    Feature {
        name: "query",
        description: "Query<T> extractor for the query string (default)",
        paths: &["extract::Query"],
        items: &["Query"],
        methods: &[],
        implies: &[],
        usage: "Query(q): Query<T>",
    },
    Feature {
        name: "form",
        description: "Form<T> extractor for urlencoded bodies (default)",
        paths: &[
            "Form",
            "extract::Form",
            "extract::checkbox",
            "extract::empty_as_none",
        ],
        items: &["Form"],
        methods: &[],
        implies: &[],
        usage: "Form(f): Form<T>",
    },
    Feature {
        name: "openapi",
        description: "OpenAPI 3.1 document and docs UI generated from handlers (default)",
        paths: &["openapi", "JsonSchema"],
        items: &["OpenApi", "DocsUi", "JsonSchema"],
        methods: &["App::openapi"],
        implies: &["json"],
        usage: "App::new().openapi(OpenApi::new(\"My API\", \"1.0\"))",
    },
    Feature {
        name: "validate",
        description: "Valid<T> extractor wrapper with #[validate] rules (default)",
        paths: &["validate"],
        items: &["Valid", "Validate"],
        methods: &[],
        implies: &["json"],
        usage: "Valid(Json(body)): Valid<Json<T>>",
    },
    Feature {
        name: "config",
        description: "Layered config: config/*.toml, APP__* env and .env (default)",
        paths: &["config"],
        items: &["ConfigBuilder"],
        methods: &[],
        implies: &[],
        usage: "let config = zapline::config::Config::load()?;",
    },
    Feature {
        name: "tracing",
        description: "Request tracing and log setup with tracing-subscriber (default)",
        paths: &["telemetry"],
        items: &["Trace", "Telemetry"],
        methods: &[],
        implies: &[],
        usage: "App::new().plugin(Trace::new())",
    },
    Feature {
        name: "http2",
        description: "HTTP/2: h2c and ALPN over TLS, next to HTTP/1.1",
        paths: &[],
        items: &[],
        methods: &[],
        implies: &[],
        usage: "on by default once enabled; turn off with App::http2(false)",
    },
    Feature {
        name: "tls",
        description: "HTTPS with rustls (TlsConfig, App::tls)",
        paths: &["tls", "TlsConfig"],
        items: &["TlsConfig"],
        methods: &["App::tls"],
        implies: &[],
        usage: "App::new().tls(TlsConfig::from_pem_files(\"cert.pem\", \"key.pem\")?)",
    },
    Feature {
        name: "http3",
        description: "Experimental HTTP/3 over QUIC next to HTTPS (needs tls; not in full)",
        paths: &["http3"],
        items: &["Http3Config"],
        methods: &["App::http3"],
        implies: &["tls"],
        usage: "App::new().tls(..).http3(Http3Config::new())",
    },
    Feature {
        name: "ws",
        description: "WebSockets: WebSocketUpgrade extractor and WebSocket",
        paths: &["ws"],
        items: &["WebSocketUpgrade", "WebSocket"],
        methods: &[],
        implies: &[],
        usage: "use zapline::ws::{Message, WebSocket, WebSocketUpgrade};",
    },
    Feature {
        name: "fs",
        description: "Static files: ServeDir, ServeFile and hashed App::assets",
        paths: &["fs"],
        items: &["ServeDir", "ServeFile", "Assets"],
        methods: &["App::static_files", "App::assets"],
        implies: &[],
        usage: "App::new().assets(\"/static\", \"static\")",
    },
    Feature {
        name: "multipart",
        description: "multipart/form-data uploads: the Multipart extractor",
        paths: &["multipart"],
        items: &["Multipart", "MultipartConfig"],
        methods: &[],
        implies: &[],
        usage: "mut form: Multipart, then form.next_field().await?",
    },
    Feature {
        name: "compression",
        description: "gzip, brotli and zstd response compression middleware",
        paths: &["middleware::compression", "middleware::Compression"],
        items: &["Compression", "CompressionLevel"],
        methods: &[],
        implies: &[],
        usage: "App::new().wrap(Compression::new())",
    },
    Feature {
        name: "jwt",
        description: "JWT bearer authentication: JwtConfig, Jwt<Claims> extractor",
        paths: &[
            "auth::jwt",
            "auth::Jwt",
            "auth::JwtConfig",
            "auth::Claims",
            "auth::JwtAuthenticator",
        ],
        items: &["Jwt", "JwtConfig", "JwtAuthenticator"],
        methods: &["App::jwt", "Authenticate::jwt"],
        implies: &["json"],
        usage: "App::new().jwt(JwtConfig::hs256(secret)), then Jwt(claims): Jwt",
    },
    Feature {
        name: "jwks",
        description: "JWT keys fetched from an identity provider's JWKS URL (RS256, ES256...)",
        paths: &["auth::Jwks"],
        items: &["Jwks"],
        methods: &["JwtConfig::jwks"],
        implies: &["jwt"],
        usage: "JwtConfig::hs256(secret).jwks(Jwks::new(\"https://idp.example.com/.well-known/jwks.json\"))",
    },
    Feature {
        name: "cookies",
        description: "Cookies (plain, signed, private), sessions and CSRF protection",
        paths: &["cookies", "session", "auth::Csrf", "auth::CsrfToken"],
        items: &[
            "CookieJar",
            "SignedCookieJar",
            "PrivateCookieJar",
            "SessionLayer",
            "SessionStore",
            "Csrf",
            "CsrfToken",
        ],
        methods: &["App::cookie_key"],
        implies: &["json"],
        usage: "jar: CookieJar; sessions: App::wrap(SessionLayer::new(MemoryStore::new()))",
    },
    // SQLite first: a hint about `Db` suggests the one that needs no server.
    Feature {
        name: "sqlite",
        description: "SQLite with sqlx: Database, Db, migrations, jobs, admin",
        paths: DB_PATHS,
        items: DB_ITEMS,
        methods: DB_METHODS,
        implies: &[],
        usage: "App::new().database(Database::from_env()), then db: Db",
    },
    Feature {
        name: "postgres",
        description: "PostgreSQL with sqlx: Database, Db, migrations, jobs, admin",
        paths: DB_PATHS,
        items: DB_ITEMS,
        methods: DB_METHODS,
        implies: &[],
        usage: "App::new().database(Database::from_env()), then db: Db",
    },
    Feature {
        name: "views",
        description: "HTML templates (minijinja) and htmx helpers: Views, HxRequest",
        paths: &["view"],
        items: &["Views", "HxRequest", "Hx"],
        methods: &["App::views"],
        implies: &[],
        usage: "App::new().views(\"templates\"), then views: Views, hx: HxRequest",
    },
    Feature {
        name: "password",
        description: "Argon2id password hashing and reset tokens",
        paths: &["auth::password"],
        items: &[],
        methods: &[],
        implies: &[],
        usage: "zapline::auth::password::hash(&password).await?",
    },
    Feature {
        name: "mail",
        description: "Email over SMTP (Mailer, Email); dev builds show it at /_dev/mail",
        paths: &["mail"],
        items: &["Mailer"],
        methods: &["App::mailer"],
        implies: &[],
        usage: "App::new().mailer(Mailer::from_env()?), then mailer: Mailer",
    },
    Feature {
        name: "storage",
        description: "File storage on local disk (or S3 with `s3`) with signed URLs",
        paths: &["storage"],
        items: &["Storage"],
        methods: &["App::storage"],
        implies: &[],
        usage: "App::new().storage(Storage::from_env()?), then storage: Storage",
    },
    Feature {
        name: "s3",
        description: "S3-compatible backend for Storage (AWS, R2, MinIO)",
        paths: &["storage::S3"],
        items: &["S3"],
        methods: &["Storage::s3"],
        implies: &["storage"],
        usage: "Storage::from_env()? picks S3 when S3_BUCKET is set",
    },
    Feature {
        name: "cache",
        description: "Key-value cache (memory or Redis), ResponseCache and ETag",
        paths: &["cache"],
        items: &["Cache", "ResponseCache", "ETag"],
        methods: &["App::cache"],
        implies: &["json"],
        usage: "App::new().cache(Cache::memory()), then cache: Cache",
    },
    Feature {
        name: "oauth",
        description: "\"Log in with GitHub/Google\": OAuth 2.0 with PKCE",
        paths: &["auth::oauth"],
        items: &["OAuth", "OAuthUser", "OAuthLogin"],
        methods: &["App::oauth"],
        implies: &["json"],
        usage: "App::new().oauth(OAuth::from_env(\"github\")?)",
    },
    Feature {
        name: "i18n",
        description: "Translations from locales/*.toml: I18n, the Locale extractor",
        paths: &["i18n"],
        items: &["I18n", "Locale"],
        methods: &["App::i18n"],
        implies: &[],
        usage: "App::new().i18n(I18n::load(\"locales\")?), then t: Locale",
    },
    Feature {
        name: "redis",
        description: "Redis client; shares cache, live updates, rate limits and sessions",
        paths: &["redis"],
        items: &["Redis", "RedisStore"],
        methods: &[
            "App::redis",
            "Cache::redis",
            "Live::redis",
            "RateLimit::redis",
            "RateLimitPolicy::redis",
        ],
        implies: &["json"],
        usage: "let redis = Redis::from_env()?; App::new().redis(redis.clone())",
    },
    Feature {
        name: "redis-tls",
        description: "rediss:// for the Redis client (TLS with Mozilla roots or your own CA)",
        paths: &[],
        items: &[],
        methods: &["Redis::with_ca_pem", "Redis::with_client_cert_pem"],
        implies: &["redis"],
        usage: "REDIS_URL=rediss://:password@redis.internal:6380",
    },
    Feature {
        name: "otlp",
        description: "Export traces to OpenTelemetry collectors (OTLP over HTTP)",
        paths: &[],
        items: &[],
        methods: &["Telemetry::otlp", "Telemetry::service_name"],
        implies: &["tracing", "json"],
        usage: "set OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4318",
    },
    Feature {
        name: "tz",
        description: "IANA time zones (Europe/Paris) in cron schedules",
        paths: &[],
        items: &[],
        methods: &[],
        implies: &[],
        usage: "Cron::parse(\"CRON_TZ=Europe/Paris 0 9 * * MON-FRI\")?",
    },
    Feature {
        name: "chrono",
        description: "zapline::chrono dates and times, wired into serde, sqlx and OpenAPI",
        paths: &["chrono"],
        items: &[],
        methods: &[],
        implies: &[],
        usage: "use zapline::chrono::{DateTime, Utc};",
    },
    Feature {
        name: "uuid",
        description: "zapline::uuid ids, wired into serde, sqlx and OpenAPI",
        paths: &["uuid"],
        items: &[],
        methods: &[],
        implies: &[],
        usage: "use zapline::uuid::Uuid;",
    },
    Feature {
        name: "full",
        description: "Everything above except the database drivers (postgres, sqlite) and http3",
        paths: &[],
        items: &[],
        methods: &[],
        implies: &[
            "default",
            "http2",
            "tls",
            "ws",
            "fs",
            "multipart",
            "compression",
            "jwt",
            "jwks",
            "cookies",
            "views",
            "password",
            "mail",
            "storage",
            "cache",
            "oauth",
            "i18n",
            "redis",
            "redis-tls",
            "otlp",
            "tz",
            "chrono",
            "uuid",
        ],
        usage: "",
    },
];

/// Looks a feature up by name.
///
/// ```
/// assert!(zapline_cli::features::find("ws").is_some());
/// assert!(zapline_cli::features::find("websockets").is_none());
/// ```
pub fn find(name: &str) -> Option<&'static Feature> {
    FEATURES.iter().find(|f| f.name == name)
}

/// Other words people use for a feature.
const ALIASES: &[(&str, &str)] = &[
    ("websocket", "ws"),
    ("websockets", "ws"),
    ("db", "sqlite"),
    ("database", "sqlite"),
    ("sql", "sqlite"),
    ("sqlx", "sqlite"),
    ("pg", "postgres"),
    ("postgresql", "postgres"),
    ("template", "views"),
    ("templates", "views"),
    ("htmx", "views"),
    ("html", "views"),
    ("minijinja", "views"),
    ("h2", "http2"),
    ("https", "tls"),
    ("ssl", "tls"),
    ("rustls", "tls"),
    ("email", "mail"),
    ("smtp", "mail"),
    ("upload", "multipart"),
    ("uploads", "multipart"),
    ("gzip", "compression"),
    ("brotli", "compression"),
    ("session", "cookies"),
    ("sessions", "cookies"),
    ("csrf", "cookies"),
    ("argon2", "password"),
    ("static", "fs"),
    ("assets", "fs"),
    ("opentelemetry", "otlp"),
    ("otel", "otlp"),
    ("translations", "i18n"),
    ("l10n", "i18n"),
    ("files", "storage"),
    ("oauth2", "oauth"),
    ("github", "oauth"),
    ("google", "oauth"),
];

/// The feature someone probably meant by an unknown name: an alias
/// (`websockets`), a type or module it unlocks (`WebSocketUpgrade`), or the
/// closest name by edit distance (`wss`).
///
/// ```
/// use zapline_cli::features::suggest;
/// assert_eq!(suggest("redsi"), Some("redis"));
/// assert_eq!(suggest("WebSocketUpgrade"), Some("ws"));
/// assert_eq!(suggest("templates"), Some("views"));
/// assert_eq!(suggest("kubernetes"), None);
/// ```
pub fn suggest(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if let Some(f) = find(&lower) {
        return Some(f.name);
    }
    if let Some((_, f)) = ALIASES.iter().find(|(alias, _)| *alias == lower) {
        return Some(f);
    }
    if let Some(f) = FEATURES.iter().find(|f| {
        f.items.iter().any(|i| i.eq_ignore_ascii_case(name))
            || f.paths.iter().any(|p| p.eq_ignore_ascii_case(name))
    }) {
        return Some(f.name);
    }
    let candidates = FEATURES
        .iter()
        .map(|f| f.name)
        .chain(ALIASES.iter().map(|(a, _)| *a));
    let (best, distance) = candidates
        .map(|c| (c, edit_distance(&lower, c)))
        .min_by_key(|&(_, d)| d)?;
    // One typo per three letters, at most two: `wss` -> `ws`, not `db` -> `ws`.
    let allowed = (lower.chars().count() / 3).clamp(1, 2);
    if distance > allowed {
        return None;
    }
    find(best)
        .map(|f| f.name)
        .or_else(|| ALIASES.iter().find(|(a, _)| *a == best).map(|(_, f)| *f))
}

/// Edit distance between two strings, by characters: insertions, deletions,
/// substitutions and swaps of neighbours (`redsi` is one edit from `redis`).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    // d[i][j]: distance between a[..i] and b[..j].
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

/// The features a diagnostic points at: those whose module, type or method
/// it names. Only for the "not found" codes (E0432, E0433, E0412, E0425,
/// E0599); `text` is the message plus its labels.
///
/// ```
/// use zapline_cli::features::missing_for;
/// let found = missing_for("E0432", "unresolved import `zapline::ws`");
/// assert_eq!(found.iter().map(|f| f.name).collect::<Vec<_>>(), ["ws"]);
/// assert!(missing_for("E0308", "mismatched types `zapline::ws`").is_empty());
/// ```
pub fn missing_for(code: &str, text: &str) -> Vec<&'static Feature> {
    let ticks = backticked(text);
    let mut found: Vec<&'static Feature> = Vec::new();
    let mut add = |f: &'static Feature| {
        if !found.contains(&f) {
            found.push(f);
        }
    };
    match code {
        // "no method named `views` found for struct `App`", "no function or
        // associated item named `s3` found for struct `Storage`".
        "E0599" => {
            if let [method, ty, ..] = ticks.as_slice() {
                let ty = last_segment(ty);
                let wanted = format!("{ty}::{method}");
                for f in FEATURES {
                    if f.methods.iter().any(|m| *m == wanted) {
                        add(f);
                    }
                }
            }
        }
        "E0432" | "E0433" | "E0412" | "E0425" => {
            // "could not find `ws` in `zapline`", "no `Jwt` in `auth`":
            // the item name followed by where rustc looked for it.
            let mut paths: Vec<String> = ticks
                .iter()
                .filter_map(|t| t.strip_prefix("zapline::").map(str::to_string))
                .collect();
            for pair in ticks.windows(2) {
                if pair[1] == "zapline" {
                    paths.push(pair[0].to_string());
                } else if let Some(parent) = pair[1].strip_prefix("zapline::") {
                    paths.push(format!("{parent}::{}", pair[0]));
                }
            }
            for path in &paths {
                for f in FEATURES {
                    let module = f.paths.iter().any(|p| {
                        path == p
                            || path
                                .strip_prefix(p)
                                .is_some_and(|rest| rest.starts_with("::"))
                    });
                    if module || f.items.contains(&last_segment(path)) {
                        add(f);
                    }
                }
            }
            // A bare name: "cannot find type `WebSocketUpgrade` in this scope".
            if let Some(first) = ticks.first().filter(|t| !t.contains("::")) {
                for f in FEATURES {
                    if f.items.contains(first) {
                        add(f);
                    }
                }
            }
        }
        _ => {}
    }
    found
}

/// The text between each pair of backticks.
fn backticked(text: &str) -> Vec<&str> {
    text.split('`').skip(1).step_by(2).collect()
}

/// `zapline::ws::WebSocket` -> `WebSocket`, `Db<Sqlite>` -> `Db`.
fn last_segment(path: &str) -> &str {
    let path = path.split('<').next().unwrap_or(path);
    path.rsplit("::").next().unwrap_or(path)
}

/// The features a manifest enables on its `zapline` dependency, with the
/// defaults and everything they imply. `None` when there is no `zapline`
/// dependency, or when it comes from the workspace (its features are
/// declared elsewhere).
///
/// ```
/// let on = zapline_cli::features::enabled(
///     "[dependencies]\nzapline = { version = \"0.1\", features = [\"s3\"] }\n",
/// )
/// .unwrap();
/// assert!(on.contains("s3") && on.contains("storage") && on.contains("json"));
/// assert!(!on.contains("ws"));
/// ```
pub fn enabled(manifest: &str) -> Option<BTreeSet<String>> {
    let doc: toml::Table = toml::from_str(manifest).ok()?;
    let dep = doc.get("dependencies")?.get("zapline")?;
    let mut on: Vec<String> = Vec::new();
    match dep {
        toml::Value::String(_) => on.push("default".into()),
        toml::Value::Table(t) => {
            if t.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                return None;
            }
            let defaults = t
                .get("default-features")
                .or_else(|| t.get("default_features"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(true);
            if defaults {
                on.push("default".into());
            }
            if let Some(list) = t.get("features").and_then(toml::Value::as_array) {
                on.extend(list.iter().filter_map(|v| v.as_str().map(str::to_string)));
            }
        }
        _ => return None,
    }
    let mut seen = BTreeSet::new();
    while let Some(name) = on.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if name == "default" {
            on.extend(DEFAULT_FEATURES.iter().map(|f| f.to_string()));
        } else if let Some(f) = find(&name) {
            on.extend(f.implies.iter().map(|f| f.to_string()));
        }
    }
    seen.remove("default");
    Some(seen)
}

// ---------------------------------------------------------------------------
// Manifest editing

/// Adds `feats` to the `zapline` dependency in `[dependencies]` and returns
/// the new manifest (the same text when nothing was missing).
///
/// Handles `zapline = "0.1"`, inline tables (`{ version = .. }`,
/// `{ path = .. }`, `{ workspace = true }`), dotted keys
/// (`zapline.version = ..`) and a `[dependencies.zapline]` table. The feature
/// list ends up sorted and without duplicates; everything else in the file
/// is left byte for byte.
///
/// ```
/// use zapline_cli::features::add_features;
/// let m = "[dependencies]\nzapline = \"0.1\"\n";
/// assert_eq!(
///     add_features(m, &["ws", "redis"]).unwrap(),
///     "[dependencies]\nzapline = { version = \"0.1\", features = [\"redis\", \"ws\"] }\n"
/// );
/// ```
pub fn add_features(manifest: &str, feats: &[&str]) -> Result<String> {
    if feats.is_empty() {
        return Ok(manifest.to_string());
    }
    let lines = line_spans(manifest);
    let mut section = String::new();
    // `[dependencies.zapline]`: the line range of its body.
    let mut dep_table: Option<(usize, usize)> = None;
    // `zapline.x = ..` lines in `[dependencies]`.
    let mut dotted: Vec<usize> = Vec::new();
    for (i, &(start, end)) in lines.iter().enumerate() {
        let line = &manifest[start..end];
        let trimmed = line.trim();
        if let Some(header) = table_header(trimmed) {
            if let Some((_, last)) = dep_table
                .as_mut()
                .filter(|_| section == "dependencies.zapline")
            {
                *last = i;
            }
            section = header;
            if section == "dependencies.zapline" {
                dep_table = Some((i + 1, lines.len()));
            }
            continue;
        }
        if section != "dependencies" {
            continue;
        }
        let Some((key, value_at)) = key_of(line) else {
            continue;
        };
        if key == "zapline" {
            let at = start + value_at;
            return add_to_value(manifest, at, feats);
        }
        if key.starts_with("zapline.") {
            dotted.push(i);
        }
    }
    if !dotted.is_empty() {
        let found = dotted.iter().find_map(|&i| {
            let (start, end) = lines[i];
            let (key, value_at) = key_of(&manifest[start..end])?;
            (key == "zapline.features").then_some(start + value_at)
        });
        if let Some(at) = found {
            return merge_array(manifest, at, feats);
        }
        // Next to the other `zapline.*` keys (also right for
        // `zapline.workspace = true`, which takes extra features too).
        let last = *dotted.last().unwrap_or(&0);
        let line = format!(
            "zapline.features = {}\n",
            format_array(&sorted(feats), None)
        );
        return Ok(insert_after_line(manifest, &lines, last, &line));
    }
    if let Some((first, end)) = dep_table {
        for &(start, stop) in &lines[first..end] {
            if let Some((key, value_at)) = key_of(&manifest[start..stop]) {
                if key == "features" {
                    return merge_array(manifest, start + value_at, feats);
                }
            }
        }
        // After the table's last key, before blank lines and the next table.
        let last = (first..end)
            .rev()
            .find(|&i| {
                let t = manifest[lines[i].0..lines[i].1].trim();
                !t.is_empty() && !t.starts_with('#')
            })
            .unwrap_or(first.saturating_sub(1));
        let line = format!("features = {}\n", format_array(&sorted(feats), None));
        return Ok(insert_after_line(manifest, &lines, last, &line));
    }
    bail!("Cargo.toml has no `zapline` dependency in [dependencies]")
}

/// `(start, end)` byte offsets of each line, without the line break.
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches('\n').trim_end_matches('\r');
        spans.push((start, start + content.len()));
        start += line.len();
    }
    spans
}

/// `[dependencies . "zapline"]` -> `dependencies.zapline`; `None` for other
/// lines (and for `[[bin]]`).
fn table_header(trimmed: &str) -> Option<String> {
    if trimmed.starts_with("[[") {
        return Some(String::from("[["));
    }
    let inner = trimmed.strip_prefix('[')?;
    let end = inner.find(']')?;
    Some(
        inner[..end]
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
            .collect(),
    )
}

/// The normalized key of a `key = value` line and where its value starts.
fn key_of(line: &str) -> Option<(String, usize)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let eq = line.find('=')?;
    let key: String = line[..eq]
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
        .collect();
    if key.is_empty() {
        return None;
    }
    let after = &line[eq + 1..];
    let value_at = eq + 1 + (after.len() - after.trim_start().len());
    Some((key, value_at))
}

/// Rewrites the value of `zapline = <value>` starting at byte `at`.
fn add_to_value(manifest: &str, at: usize, feats: &[&str]) -> Result<String> {
    let rest = &manifest[at..];
    match rest.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let close = rest[1..]
                .find(q)
                .context("unterminated version string for zapline in Cargo.toml")?;
            let version = &rest[..close + 2];
            let new = format!(
                "{{ version = {version}, features = {} }}",
                format_array(&sorted(feats), None)
            );
            if feats.is_empty() {
                return Ok(manifest.to_string());
            }
            Ok(splice(manifest, at, at + version.len(), &new))
        }
        Some('{') => {
            let close = at
                + matching(rest, '{', '}')
                    .context("unterminated inline table for zapline in Cargo.toml")?;
            if let Some(array_at) = key_in_table(manifest, at + 1, close, "features") {
                return merge_array(manifest, array_at, feats);
            }
            if feats.is_empty() {
                return Ok(manifest.to_string());
            }
            let inner = &manifest[at + 1..close];
            let body_end = at + 1 + inner.trim_end().len();
            let array = format_array(&sorted(feats), None);
            // `{}` and `{ path = "..", }` (TOML 1.1) need no comma.
            let insert = if inner.trim().is_empty() || inner.trim_end().ends_with(',') {
                format!(" features = {array} ")
            } else {
                format!(", features = {array} ")
            };
            Ok(splice(manifest, body_end, close, &insert))
        }
        _ => bail!("cannot read the zapline dependency in Cargo.toml; add the features by hand"),
    }
}

/// Where the value of `key = ...` starts inside an inline table spanning
/// `from..to`, at the table's own level.
fn key_in_table(text: &str, from: usize, to: usize, key: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = from;
    while i < to {
        match bytes[i] {
            b'"' | b'\'' => i = skip_string(text, i)?,
            b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 0
                && bytes[i..].starts_with(key.as_bytes())
                && (i == from || !is_key_char(bytes[i - 1])) =>
            {
                let after = &text[i + key.len()..to];
                let trimmed = after.trim_start();
                if let Some(value) = trimmed.strip_prefix('=') {
                    let value_at = to - value.trim_start().len();
                    return Some(value_at);
                }
                i += key.len();
            }
            _ => i += 1,
        }
    }
    None
}

fn is_key_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// The index just past the closing quote of the string starting at `at`.
fn skip_string(text: &str, at: usize) -> Option<usize> {
    let quote = text.as_bytes()[at];
    let mut i = at + 1;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quote == b'"' => i += 2,
            b if b == quote => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Offset of the bracket closing the one that opens `text`.
fn matching(text: &str, open: char, close: char) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\'' {
            i = skip_string(text, i)?;
            continue;
        }
        if b == open as u8 {
            depth += 1;
        } else if b == close as u8 {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Merges `feats` into the array value starting at byte `at`.
fn merge_array(manifest: &str, at: usize, feats: &[&str]) -> Result<String> {
    let rest = &manifest[at..];
    if !rest.starts_with('[') {
        bail!("zapline's `features` in Cargo.toml is not an array");
    }
    let close = matching(rest, '[', ']').context("unterminated `features` array in Cargo.toml")?;
    let body = &rest[1..close];
    let mut existing: Vec<String> = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                let end = skip_string(body, i).context("unterminated string in `features`")?;
                existing.push(body[i + 1..end - 1].to_string());
                i = end;
            }
            b'#' => bail!(
                "zapline's `features` array in Cargo.toml has comments; add {} by hand",
                feats.join(", ")
            ),
            _ => i += 1,
        }
    }
    if feats.iter().all(|f| existing.iter().any(|e| e == f)) {
        return Ok(manifest.to_string());
    }
    let all: Vec<&str> = existing
        .iter()
        .map(String::as_str)
        .chain(feats.iter().copied())
        .collect();
    let multiline = body.contains('\n').then(|| {
        // Keep the layout: one item per line, at the first item's indent.
        let item_indent = body
            .lines()
            .skip(1)
            .find(|l| !l.trim().is_empty())
            .map(|l| &l[..l.len() - l.trim_start().len()])
            .unwrap_or("    ");
        let close_indent = body.rsplit('\n').next().unwrap_or("");
        let close_indent = &close_indent[..close_indent.len() - close_indent.trim_start().len()];
        (item_indent.to_string(), close_indent.to_string())
    });
    let new = format_array(&sorted(&all), multiline.as_ref());
    Ok(splice(manifest, at, at + close + 1, &new))
}

/// Sorted and deduplicated.
fn sorted<'a>(feats: &[&'a str]) -> Vec<&'a str> {
    let set: BTreeSet<&str> = feats.iter().copied().collect();
    set.into_iter().collect()
}

/// `["a", "b"]`, or one item per line given `(item indent, closing indent)`.
fn format_array(items: &[&str], multiline: Option<&(String, String)>) -> String {
    match multiline {
        None => format!(
            "[{}]",
            items
                .iter()
                .map(|f| format!("\"{f}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Some((indent, close)) => {
            let mut out = String::from("[\n");
            for f in items {
                out.push_str(&format!("{indent}\"{f}\",\n"));
            }
            out.push_str(close);
            out.push(']');
            out
        }
    }
}

fn splice(text: &str, from: usize, to: usize, with: &str) -> String {
    let mut out = String::with_capacity(text.len() + with.len());
    out.push_str(&text[..from]);
    out.push_str(with);
    out.push_str(&text[to..]);
    out
}

/// Inserts `new_line` (ending in `\n`) after line `index`.
fn insert_after_line(text: &str, lines: &[(usize, usize)], index: usize, new_line: &str) -> String {
    let end = lines[index].1;
    // Past this line's own line break, if it has one.
    let rest = &text[end..];
    let after = if rest.starts_with("\r\n") {
        end + 2
    } else if rest.starts_with('\n') {
        end + 1
    } else {
        return format!("{text}\n{new_line}");
    };
    splice(text, after, after, new_line)
}

// ---------------------------------------------------------------------------
// `zapline add`

/// What `zapline add` did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Added {
    /// Features newly written to Cargo.toml.
    pub added: Vec<&'static str>,
    /// Features that were already on (listed, a default, or implied).
    pub already: Vec<&'static str>,
}

impl Added {
    /// The lines `zapline add` prints.
    ///
    /// ```
    /// use zapline_cli::features::Added;
    /// let a = Added { added: vec!["ws"], already: vec!["json"] };
    /// assert!(a.message().starts_with("Added features: ws\n"));
    /// assert!(a.message().contains("Already enabled: json"));
    /// ```
    pub fn message(&self) -> String {
        let mut out = String::new();
        if !self.added.is_empty() {
            out.push_str(&format!("Added features: {}\n", self.added.join(", ")));
        }
        if !self.already.is_empty() {
            out.push_str(&format!("Already enabled: {}\n", self.already.join(", ")));
            if self.added.is_empty() {
                out.push_str("Nothing to do: Cargo.toml is unchanged\n");
            }
        }
        for f in self.added.iter().filter_map(|n| find(n)) {
            if !f.usage.is_empty() {
                out.push_str(&format!("  {}: {}\n", f.name, f.usage));
            }
        }
        out
    }
}

/// Checks feature names against [`FEATURES`], with a suggestion for each
/// unknown one.
///
/// ```
/// use zapline_cli::features::validate;
/// assert_eq!(validate(&["ws", "redis"]).unwrap(), ["ws", "redis"]);
/// let err = validate(&["wss"]).unwrap_err().to_string();
/// assert!(err.contains("did you mean `ws`?"), "{err}");
/// ```
pub fn validate<S: AsRef<str>>(names: &[S]) -> Result<Vec<&'static str>> {
    let mut known = Vec::new();
    let mut problems = Vec::new();
    for name in names {
        let name = name.as_ref().trim();
        match find(name) {
            Some(f) => {
                if !known.contains(&f.name) {
                    known.push(f.name);
                }
            }
            None => problems.push(match suggest(name) {
                Some(s) => format!("unknown feature `{name}`; did you mean `{s}`?"),
                None => format!("unknown feature `{name}`"),
            }),
        }
    }
    if known.is_empty() && problems.is_empty() {
        bail!("name at least one feature; `zapline add --list` shows them");
    }
    if !problems.is_empty() {
        bail!(
            "{}\nhint: `zapline add --list` shows every feature",
            problems.join("\n")
        );
    }
    Ok(known)
}

/// `zapline add <feature>...`: enables features in `<root>/Cargo.toml`.
///
/// ```no_run
/// let done = zapline_cli::features::add(std::path::Path::new("."), &["ws"]).unwrap();
/// print!("{}", done.message());
/// ```
pub fn add<S: AsRef<str>>(root: &Path, names: &[S]) -> Result<Added> {
    let wanted = validate(names)?;
    let path = root.join("Cargo.toml");
    let manifest =
        fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let on = enabled(&manifest).unwrap_or_default();
    let mut result = Added::default();
    for name in wanted {
        if on.contains(name) {
            result.already.push(name);
        } else {
            result.added.push(name);
        }
    }
    if !result.added.is_empty() {
        let patched = add_features(&manifest, &result.added)?;
        // A manifest cargo can't read is worse than none at all.
        toml::from_str::<toml::Table>(&patched)
            .context("internal error: the edited Cargo.toml does not parse; it was not written")?;
        if patched != manifest {
            fs::write(&path, patched)
                .with_context(|| format!("cannot write {}", path.display()))?;
        }
    }
    Ok(result)
}

/// `zapline add --list`: each feature and what it is for, marking the ones
/// on in `manifest` when there is one.
///
/// ```
/// let list = zapline_cli::features::list(None);
/// assert!(list.contains("ws "));
/// ```
pub fn list(manifest: Option<&str>) -> String {
    let on = manifest.and_then(enabled);
    let width = FEATURES.iter().map(|f| f.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for f in FEATURES {
        let mark = match &on {
            Some(on) if on.contains(f.name) => "* ",
            Some(_) => "  ",
            None => "",
        };
        out.push_str(&format!(
            "{mark}{:width$}  {}\n",
            f.name,
            f.description,
            width = width
        ));
    }
    if on.is_some() {
        out.push_str("\n* enabled in this project\n");
    }
    out.push_str("Enable with: zapline add <feature>...\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZAPLINE_MANIFEST: &str = include_str!("../../zapline/Cargo.toml");

    fn cargo_features() -> toml::Table {
        let doc: toml::Table = toml::from_str(ZAPLINE_MANIFEST).unwrap();
        doc["features"].as_table().unwrap().clone()
    }

    /// The public features a feature's list turns on (no `dep:`, no
    /// `crate/feature`, no `_internal`).
    fn public_implies(list: &toml::Value) -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .filter(|f| !f.starts_with("dep:") && !f.contains('/') && !f.starts_with('_'))
            .collect()
    }

    #[test]
    fn every_public_feature_has_an_entry_and_the_table_matches_cargo_toml() {
        let features = cargo_features();
        for (name, list) in &features {
            if name.starts_with('_') {
                continue;
            }
            if name == "default" {
                assert_eq!(public_implies(list), DEFAULT_FEATURES, "default");
                continue;
            }
            let f = find(name).unwrap_or_else(|| panic!("no entry for feature `{name}`"));
            assert_eq!(public_implies(list), f.implies, "implies of `{name}`");
            assert!(!f.description.is_empty(), "{name}");
        }
        for f in FEATURES {
            assert!(features.contains_key(f.name), "stale entry `{}`", f.name);
        }
    }

    #[test]
    fn version_string() {
        let m =
            "[package]\nname = \"x\"\n\n[dependencies]\nzapline = \"0.1\" # web\nserde = \"1\"\n";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[package]\nname = \"x\"\n\n[dependencies]\nzapline = { version = \"0.1\", features = [\"ws\"] } # web\nserde = \"1\"\n"
        );
    }

    #[test]
    fn inline_table_with_features() {
        let m =
            "[dependencies]\nzapline = { version = \"0.1\", features = [\"views\", \"sqlite\"] }\n";
        assert_eq!(
            add_features(m, &["ws", "sqlite", "cache"]).unwrap(),
            "[dependencies]\nzapline = { version = \"0.1\", features = [\"cache\", \"sqlite\", \"views\", \"ws\"] }\n"
        );
        // An empty list.
        let m = "[dependencies]\nzapline = { version = \"0.1\", features = [] }\n";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[dependencies]\nzapline = { version = \"0.1\", features = [\"ws\"] }\n"
        );
    }

    #[test]
    fn inline_table_with_path_and_no_features() {
        let m =
            "[dependencies]\nzapline = { path = \"../zapline/crates/zapline\" }\ntokio = \"1\"\n";
        assert_eq!(
            add_features(m, &["redis", "ws"]).unwrap(),
            "[dependencies]\nzapline = { path = \"../zapline/crates/zapline\", features = [\"redis\", \"ws\"] }\ntokio = \"1\"\n"
        );
        let m = "[dependencies]\nzapline = { workspace = true }\n";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[dependencies]\nzapline = { workspace = true, features = [\"ws\"] }\n"
        );
        let m = "[dependencies]\nzapline = {}\n";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[dependencies]\nzapline = { features = [\"ws\"] }\n"
        );
    }

    #[test]
    fn a_features_key_inside_a_string_is_not_the_features_key() {
        let m = "[dependencies]\nzapline = { path = \"features = [x]\" }\n";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[dependencies]\nzapline = { path = \"features = [x]\", features = [\"ws\"] }\n"
        );
        let m = "[dependencies]\nzapline = { version = \"0.1\", default-features = false }\n";
        assert_eq!(
            add_features(m, &["json"]).unwrap(),
            "[dependencies]\nzapline = { version = \"0.1\", default-features = false, features = [\"json\"] }\n"
        );
    }

    #[test]
    fn dependencies_zapline_table() {
        let m = "[dependencies.zapline]\nversion = \"0.1\"\nfeatures = [\"ws\"]\n\n[dev-dependencies]\nx = \"1\"\n";
        assert_eq!(
            add_features(m, &["cookies"]).unwrap(),
            "[dependencies.zapline]\nversion = \"0.1\"\nfeatures = [\"cookies\", \"ws\"]\n\n[dev-dependencies]\nx = \"1\"\n"
        );
        let m =
            "[dependencies.zapline]\npath = \"../z\"\n\n# tools\n[dev-dependencies]\nx = \"1\"\n";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[dependencies.zapline]\npath = \"../z\"\nfeatures = [\"ws\"]\n\n# tools\n[dev-dependencies]\nx = \"1\"\n"
        );
        // Last table in a file without a final newline.
        let m = "[dependencies.zapline]\nversion = \"0.1\"";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[dependencies.zapline]\nversion = \"0.1\"\nfeatures = [\"ws\"]\n"
        );
    }

    #[test]
    fn multiline_arrays_keep_their_layout() {
        let m = "[dependencies.zapline]\nversion = \"0.1\"\nfeatures = [\n    \"ws\",\n    \"fs\",\n]\n";
        assert_eq!(
            add_features(m, &["cache"]).unwrap(),
            "[dependencies.zapline]\nversion = \"0.1\"\nfeatures = [\n    \"cache\",\n    \"fs\",\n    \"ws\",\n]\n"
        );
        let commented =
            "[dependencies.zapline]\nversion = \"0.1\"\nfeatures = [\n  \"ws\", # sockets\n]\n";
        assert!(add_features(commented, &["fs"]).is_err());
    }

    #[test]
    fn dotted_keys() {
        let m = "[dependencies]\nzapline.version = \"0.1\"\nserde = \"1\"\n";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[dependencies]\nzapline.version = \"0.1\"\nzapline.features = [\"ws\"]\nserde = \"1\"\n"
        );
        let m = "[dependencies]\nzapline.version = \"0.1\"\nzapline.features = [\"ws\"]\n";
        assert_eq!(
            add_features(m, &["fs"]).unwrap(),
            "[dependencies]\nzapline.version = \"0.1\"\nzapline.features = [\"fs\", \"ws\"]\n"
        );
    }

    #[test]
    fn only_the_dependencies_section_counts() {
        let m = "[dev-dependencies]\nzapline = \"0.1\"\n";
        assert!(add_features(m, &["ws"]).is_err());
        let m = "[workspace.dependencies]\nzapline = \"0.1\"\n[dependencies]\nzapline = { workspace = true }\n";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[workspace.dependencies]\nzapline = \"0.1\"\n[dependencies]\nzapline = { workspace = true, features = [\"ws\"] }\n"
        );
        // Lookalike names are other crates.
        let m = "[dependencies]\nzapline-macros = \"0.1\"\nzapline = \"0.1\"\n";
        assert_eq!(
            add_features(m, &["ws"]).unwrap(),
            "[dependencies]\nzapline-macros = \"0.1\"\nzapline = { version = \"0.1\", features = [\"ws\"] }\n"
        );
    }

    #[test]
    fn crlf_line_endings_survive() {
        let m = "[dependencies.zapline]\r\nversion = \"0.1\"\r\n\r\n[x]\r\n";
        let out = add_features(m, &["ws"]).unwrap();
        assert_eq!(
            out,
            "[dependencies.zapline]\r\nversion = \"0.1\"\r\nfeatures = [\"ws\"]\n\r\n[x]\r\n"
        );
        toml::from_str::<toml::Table>(&out).unwrap();
    }

    #[test]
    fn adding_is_idempotent_and_keeps_the_rest_of_the_file() {
        let shapes = [
            "[dependencies]\nzapline = \"0.1\"\n",
            "[dependencies]\nzapline = { version = \"0.1\", features = [\"sqlite\"] }\n",
            "[dependencies]\nzapline = { path = \"../z\" }\n",
            "[dependencies.zapline]\nversion = \"0.1\"\n",
            "[dependencies]\nzapline.version = \"0.1\"\n",
        ];
        for shape in shapes {
            let m = format!(
                "# top\n[package]\nname = \"x\" # n\n\n{shape}\n[profile.dev]\nopt-level = 1\n"
            );
            let once = add_features(&m, &["ws", "redis", "ws"]).unwrap();
            let twice = add_features(&once, &["ws", "redis"]).unwrap();
            assert_eq!(once, twice, "{shape}");
            assert!(
                once.starts_with("# top\n[package]\nname = \"x\" # n\n\n"),
                "{once}"
            );
            assert!(once.ends_with("\n[profile.dev]\nopt-level = 1\n"), "{once}");
            let on = enabled(&once).unwrap();
            assert!(on.contains("ws") && on.contains("redis"), "{shape}: {on:?}");
            // Nothing new: the very same text.
            assert_eq!(add_features(&once, &["redis"]).unwrap(), once);
        }
    }

    #[test]
    fn enabled_follows_defaults_and_implications() {
        let on = enabled("[dependencies]\nzapline = \"0.1\"\n").unwrap();
        assert!(on.contains("json") && on.contains("openapi") && !on.contains("ws"));
        let on = enabled(
            "[dependencies]\nzapline = { version = \"0.1\", default-features = false, features = [\"jwt\"] }\n",
        )
        .unwrap();
        assert_eq!(on.into_iter().collect::<Vec<_>>(), ["json", "jwt"]);
        let on =
            enabled("[dependencies]\nzapline = { version = \"0.1\", features = [\"full\"] }\n")
                .unwrap();
        assert!(on.contains("ws") && on.contains("otlp") && !on.contains("sqlite"));
        assert!(enabled("[dependencies]\nzapline = { workspace = true }\n").is_none());
        assert!(enabled("[dependencies]\nserde = \"1\"\n").is_none());
    }

    #[test]
    fn suggestions() {
        assert_eq!(suggest("wss"), Some("ws"));
        assert_eq!(suggest("WS"), Some("ws"));
        assert_eq!(suggest("view"), Some("views"));
        assert_eq!(suggest("cookie"), Some("cookies"));
        assert_eq!(suggest("postgre"), Some("postgres"));
        assert_eq!(suggest("websockets"), Some("ws"));
        assert_eq!(suggest("database"), Some("sqlite"));
        assert_eq!(suggest("HxRequest"), Some("views"));
        assert_eq!(suggest("xyzzy"), None);
        let err = validate(&["ws", "redsi", "nonsense"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown feature `redsi`; did you mean `redis`?"),
            "{err}"
        );
        assert!(err.contains("unknown feature `nonsense`\n"), "{err}");
        assert_eq!(validate(&["ws", "ws"]).unwrap(), ["ws"]);
        assert!(validate::<&str>(&[]).is_err());
    }

    #[test]
    fn diagnostics_map_to_features() {
        let names = |code: &str, text: &str| -> Vec<&str> {
            missing_for(code, text).iter().map(|f| f.name).collect()
        };
        assert_eq!(
            names(
                "E0432",
                "unresolved import `zapline::ws`\ncould not find `ws` in `zapline`"
            ),
            ["ws"]
        );
        assert_eq!(
            names(
                "E0433",
                "failed to resolve: could not find `view` in `zapline`"
            ),
            ["views"]
        );
        assert_eq!(
            names(
                "E0432",
                "unresolved import `zapline::auth::Jwt`\nno `Jwt` in `auth`"
            ),
            ["jwt"]
        );
        assert_eq!(
            names(
                "E0432",
                "unresolved import `zapline::prelude::WebSocketUpgrade`"
            ),
            ["ws"]
        );
        assert_eq!(
            names("E0412", "cannot find type `Db` in this scope"),
            ["sqlite", "postgres"]
        );
        assert_eq!(
            names(
                "E0599",
                "no method named `views` found for struct `App` in the current scope"
            ),
            ["views"]
        );
        assert_eq!(
            names(
                "E0599",
                "no function or associated item named `s3` found for struct `zapline::storage::Storage` in the current scope"
            ),
            ["s3"]
        );
        assert!(names("E0599", "no method named `views` found for struct `Router`").is_empty());
        assert!(names("E0412", "cannot find type `Widget` in this scope").is_empty());
        assert!(names("E0432", "unresolved import `zapline::nope`").is_empty());
        assert!(names("E0308", "mismatched types `WebSocket`").is_empty());
    }
}
