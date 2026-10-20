//! Layered configuration: files, profiles, `.env`, environment variables,
//! overrides, typed extraction, precise errors and secrets.
//!
//! Tests never mutate the process environment: they inject variables with
//! `env_source` and resolve files against temp dirs with `base_dir`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use zapline::config::{Config, ConfigError, Secret, Value};
use zapline::di::Dep;
use zapline::prelude::*;

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct Settings {
    name: String,
    server: Server,
    database: Database,
    #[serde(default)]
    features: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct Server {
    host: String,
    port: u16,
    #[serde(default)]
    debug: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct Database {
    url: Secret<String>,
    pool: u32,
    timeout_secs: Option<f64>,
}

impl PartialEq for Database {
    fn eq(&self, o: &Self) -> bool {
        self.url.expose() == o.url.expose() && self.pool == o.pool
    }
}

const DEFAULT_TOML: &str = r#"
name = "shop"
features = ["search"]

[server]
host = "0.0.0.0"
port = 8080

[database]
url = "postgres://localhost/dev"
pool = 5
"#;

const PRODUCTION_TOML: &str = r#"
[server]
port = 80

[database]
url = "postgres://db.internal/prod"
"#;

fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A temp dir with the standard `config/` layout.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "config/default.toml", DEFAULT_TOML);
    write(dir.path(), "config/production.toml", PRODUCTION_TOML);
    dir
}

fn no_env() -> [(&'static str, &'static str); 0] {
    []
}

#[test]
fn standard_layers_default_profile() {
    let dir = project();
    let config = Config::builder()
        .standard()
        .base_dir(dir.path())
        .env_source(no_env())
        .build()
        .unwrap();
    assert_eq!(config.profile(), "development");
    let s: Settings = config.extract().unwrap();
    assert_eq!(s.name, "shop");
    assert_eq!(s.server.port, 8080);
    assert_eq!(s.database.url.expose(), "postgres://localhost/dev");
    assert_eq!(s.features, vec!["search"]);
}

#[test]
fn profile_file_deep_merges_over_defaults() {
    let dir = project();
    let config = Config::builder()
        .standard()
        .base_dir(dir.path())
        .env_source([("APP_ENV", "production")])
        .build()
        .unwrap();
    assert_eq!(config.profile(), "production");
    let s: Settings = config.extract().unwrap();
    assert_eq!(s.server.port, 80, "overridden by production.toml");
    assert_eq!(s.server.host, "0.0.0.0", "kept from default.toml");
    assert_eq!(s.database.url.expose(), "postgres://db.internal/prod");
    assert_eq!(s.database.pool, 5, "sibling key survives the merge");
    assert_eq!(
        config.origin("server.port"),
        Some(
            format!(
                "file `{}`",
                dir.path().join("config/production.toml").display()
            )
            .as_str()
        )
    );
}

#[test]
fn profile_resolution_order() {
    let build = |vars: &[(&str, &str)]| {
        Config::builder()
            .env_source(vars.iter().copied())
            .build()
            .unwrap()
            .profile()
            .to_owned()
    };
    assert_eq!(build(&[]), "development");
    assert_eq!(build(&[("ZAPLINE_ENV", "staging")]), "staging");
    assert_eq!(
        build(&[("ZAPLINE_ENV", "staging"), ("APP_ENV", "production")]),
        "production"
    );
    assert_eq!(build(&[("APP_ENV", ""), ("ZAPLINE_ENV", "test")]), "test");

    // A custom prefix renames the profile variable; explicit always wins.
    let c = Config::builder()
        .env_prefix("SHOP")
        .env_source([("SHOP_ENV", "qa"), ("APP_ENV", "production")])
        .build()
        .unwrap();
    assert_eq!(c.profile(), "qa");
    let c = Config::builder()
        .profile("explicit")
        .env_source([("APP_ENV", "production")])
        .build()
        .unwrap();
    assert_eq!(c.profile(), "explicit");
}

#[test]
fn invalid_profile_is_rejected_before_touching_files() {
    let err = Config::builder()
        .file_optional("config/{profile}.toml")
        .env_source([("APP_ENV", "../../etc/passwd")])
        .build()
        .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidProfile { .. }), "{err}");
    assert!(err.to_string().contains("invalid config profile"));
}

#[test]
fn env_vars_override_files_with_nesting_and_types() {
    let dir = project();
    let config = Config::builder()
        .standard()
        .base_dir(dir.path())
        .env_source([
            ("APP__SERVER__PORT", "9000"),
            ("APP__SERVER__DEBUG", "true"),
            ("APP__DATABASE__POOL", "32"),
            ("APP__DATABASE__TIMEOUT_SECS", "1.5"),
            ("APP__FEATURES", "[\"search\", \"checkout\"]"),
            ("APP__NAME", "Shop Prod"),
            ("APPLE", "ignored: no separator"),
            ("APP_SINGLE", "ignored: single underscore"),
            ("OTHER__SERVER__PORT", "1"),
        ])
        .build()
        .unwrap();
    let s: Settings = config.extract().unwrap();
    assert_eq!(s.server.port, 9000);
    assert!(s.server.debug);
    assert_eq!(s.database.pool, 32);
    assert_eq!(s.database.timeout_secs, Some(1.5));
    assert_eq!(s.features, vec!["search", "checkout"]);
    assert_eq!(s.name, "Shop Prod");
    assert!(!config.contains("le") && !config.contains("_single"));
    assert_eq!(config.value("server.port"), Some(&Value::Integer(9000)));
    assert_eq!(
        config.origin("server.port"),
        Some("env `APP__SERVER__PORT`")
    );
}

#[test]
fn env_values_keep_their_exact_text_for_strings() {
    #[derive(Deserialize)]
    struct Strings {
        hex: String,
        padded: String,
        version: String,
        number: String,
        flag: String,
        secret: Secret<String>,
    }
    let config = Config::builder()
        .env_prefix("APP")
        .env_source([
            ("APP__HEX", "0x1F"),
            ("APP__PADDED", "007"),
            ("APP__VERSION", "1.10"),
            ("APP__NUMBER", "12345"),
            ("APP__FLAG", "true"),
            ("APP__SECRET", "98765"),
        ])
        .build()
        .unwrap();
    let s: Strings = config.extract().unwrap();
    assert_eq!(s.hex, "0x1F");
    assert_eq!(s.padded, "007");
    assert_eq!(s.version, "1.10");
    assert_eq!(s.number, "12345");
    assert_eq!(s.flag, "true");
    assert_eq!(s.secret.expose(), "98765");
}

#[test]
fn lenient_scalars_lists_and_enums() {
    #[derive(Deserialize, Debug, PartialEq)]
    #[serde(rename_all = "lowercase")]
    enum Level {
        Debug,
        Info,
    }
    #[derive(Deserialize)]
    struct Opts {
        ports: Vec<u16>,
        hosts: Vec<String>,
        empty: Vec<String>,
        on: bool,
        yes: bool,
        one: bool,
        off: bool,
        level: Level,
        ratio: f32,
        big: u64,
        tuple: (u8, String),
    }
    let config = Config::builder()
        .env_prefix("APP")
        .env_source([
            ("APP__PORTS", "80, 443"),
            ("APP__HOSTS", "a.example,b.example"),
            ("APP__EMPTY", ""),
            ("APP__ON", "on"),
            ("APP__YES", "YES"),
            ("APP__ONE", "1"),
            ("APP__OFF", "off"),
            ("APP__LEVEL", "INFO"),
            ("APP__RATIO", "1e-1"),
            ("APP__BIG", "18446744073709551615"),
            ("APP__TUPLE", "[7, \"x\"]"),
        ])
        .build()
        .unwrap();
    let o: Opts = config.extract().unwrap();
    assert_eq!(o.ports, vec![80, 443]);
    assert_eq!(o.hosts, vec!["a.example", "b.example"]);
    assert!(o.empty.is_empty());
    assert!(o.on && o.yes && o.one && !o.off);
    assert_eq!(o.level, Level::Info);
    assert!((o.ratio - 0.1).abs() < 1e-6);
    assert_eq!(o.big, u64::MAX);
    assert_eq!(o.tuple, (7, "x".to_owned()));
}

#[test]
fn env_keys_match_existing_keys_case_insensitively() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Api {
        max_connections: u32,
        base_url: String,
    }
    let config = Config::builder()
        .toml_str("[api]\nmaxConnections = 10")
        .env_prefix("APP")
        .env_source([
            ("APP__API__MAXCONNECTIONS", "50"),
            ("APP__API__BASEURL", "https://x"),
        ])
        .build()
        .unwrap();
    // The env var updated the existing camelCase key instead of adding a twin.
    assert_eq!(config.table()["api"].as_table().unwrap().len(), 2);
    let api: Api = config.get("api").unwrap();
    assert_eq!(api.max_connections, 50);
    assert_eq!(api.base_url, "https://x");
}

#[test]
fn custom_separator_and_prefix() {
    let config = Config::builder()
        .env_prefix("SVC")
        .separator("_")
        .env_source([("SVC_DB_URL", "x"), ("APP__DB__URL", "ignored")])
        .build()
        .unwrap();
    assert_eq!(config.get::<String>("db.url").unwrap(), "x");
}

#[test]
fn dotenv_fills_only_unset_variables() {
    let dir = project();
    write(
        dir.path(),
        ".env",
        r#"
# comment
export APP_ENV=production
APP__SERVER__PORT=7000       # inline comment
APP__DATABASE__POOL="12"
APP__NAME='literal $name # not a comment'
APP__SERVER__HOST="multi
line"
"#,
    );
    let config = Config::builder()
        .standard()
        .base_dir(dir.path())
        .env_source([("APP__DATABASE__POOL", "99")])
        .build()
        .unwrap();
    assert_eq!(
        config.profile(),
        "production",
        ".env can select the profile"
    );
    let s: Settings = config.extract().unwrap();
    assert_eq!(s.server.port, 7000);
    assert_eq!(s.database.pool, 99, "real env wins over .env");
    assert_eq!(s.name, "literal $name # not a comment");
    assert_eq!(s.server.host, "multi\nline");
    assert_eq!(s.database.url.expose(), "postgres://db.internal/prod");
    assert!(
        std::env::var_os("APP__SERVER__PORT").is_none(),
        "process env untouched"
    );
}

#[test]
fn malformed_dotenv_reports_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".env", "OK=1\nNOT AN ASSIGNMENT\n");
    let err = Config::builder()
        .base_dir(dir.path())
        .dotenv(".env")
        .env_source(no_env())
        .build()
        .unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, ConfigError::Parse { .. }));
    assert!(msg.contains(".env") && msg.contains("line 2"), "{msg}");

    write(dir.path(), ".env", "A=\"unterminated\n");
    let msg = Config::builder()
        .base_dir(dir.path())
        .dotenv(".env")
        .env_source(no_env())
        .build()
        .unwrap_err()
        .to_string();
    assert!(msg.contains("unterminated"), "{msg}");
}

#[test]
fn overrides_and_defaults_bracket_every_layer() {
    let config = Config::builder()
        .set_default("server.port", 1)
        .set_default("server.workers", 4)
        .toml_str("[server]\nport = 8080")
        .env_prefix("APP")
        .env_source([("APP__SERVER__PORT", "9000")])
        .set("server.port", 443u16)
        .set("server.tags", vec!["a", "b"])
        .build()
        .unwrap();
    assert_eq!(config.get::<u16>("server.port").unwrap(), 443);
    assert_eq!(config.get::<u8>("server.workers").unwrap(), 4);
    assert_eq!(
        config.get::<Vec<String>>("server.tags").unwrap(),
        ["a", "b"]
    );
    assert_eq!(config.origin("server.port"), Some("override"));
    assert_eq!(config.origin("server.workers"), Some("default"));
}

#[test]
fn later_sources_override_earlier_ones() {
    let config = Config::builder()
        .toml_str("a = 1\n[t]\nx = 1\ny = 1")
        .toml_str("a = 2\n[t]\ny = 2")
        .build()
        .unwrap();
    assert_eq!(config.get::<i32>("a").unwrap(), 2);
    assert_eq!(config.get::<i32>("t.x").unwrap(), 1);
    assert_eq!(config.get::<i32>("t.y").unwrap(), 2);

    // A scalar may replace a table and vice versa.
    let config = Config::builder()
        .toml_str("[db]\nurl = \"x\"")
        .env_prefix("APP")
        .env_source([("APP__DB", "sqlite::memory:")])
        .build()
        .unwrap();
    assert_eq!(config.get::<String>("db").unwrap(), "sqlite::memory:");
}

#[test]
fn get_paths_arrays_options_and_defaults() {
    let config = Config::builder()
        .toml_str("[[servers]]\nhost = \"a\"\n[[servers]]\nhost = \"b\"\nport = 81")
        .build()
        .unwrap();
    assert_eq!(config.get::<String>("servers[1].host").unwrap(), "b");
    assert_eq!(config.get::<String>("servers.0.host").unwrap(), "a");
    assert_eq!(config.get::<Option<u16>>("servers[0].port").unwrap(), None);
    assert_eq!(
        config.get::<Option<u16>>("servers[1].port").unwrap(),
        Some(81)
    );
    assert_eq!(config.get_or("servers[0].port", 80u16).unwrap(), 80);
    assert!(config.contains("servers[1]"));
    assert!(!config.contains("servers[2]"));
    let map: HashMap<String, Value> = config.get("servers[1]").unwrap();
    assert_eq!(map.len(), 2);

    let err = config.get::<u16>("servers[0].port").unwrap_err();
    assert!(matches!(&err, ConfigError::Missing { key } if key == "servers[0].port"));
    assert_eq!(err.to_string(), "missing config key `servers[0].port`");
}

#[test]
fn extraction_errors_name_key_type_and_origin() {
    let config = Config::builder()
        .toml_str(DEFAULT_TOML)
        .env_prefix("APP")
        .env_source([("APP__SERVER__PORT", "eighty")])
        .build()
        .unwrap();
    let err = config.extract::<Settings>().unwrap_err();
    assert_eq!(err.key(), Some("server.port"));
    let msg = err.to_string();
    assert!(msg.contains("invalid config key `server.port`"), "{msg}");
    assert!(msg.contains("expected u16"), "{msg}");
    assert!(msg.contains("set by env `APP__SERVER__PORT`"), "{msg}");

    // Out of range, from a file.
    let config = Config::builder()
        .toml_str("[server]\nhost = \"h\"\nport = 70000")
        .build()
        .unwrap();
    let msg = config.get::<Server>("server").unwrap_err().to_string();
    assert!(
        msg.contains("server.port") && msg.contains("70000"),
        "{msg}"
    );

    // Missing nested field reports the full path.
    let config = Config::builder()
        .toml_str("name = \"x\"\n[server]\nhost = \"h\"\nport = 1\n[database]\npool = 1")
        .build()
        .unwrap();
    let err = config.extract::<Settings>().unwrap_err();
    assert!(
        matches!(&err, ConfigError::Missing { key } if key == "database.url"),
        "{err}"
    );

    // Array element errors carry the index.
    let config = Config::builder()
        .toml_str("ports = [80, \"x\"]")
        .build()
        .unwrap();
    let msg = config.get::<Vec<u16>>("ports").unwrap_err().to_string();
    assert!(msg.contains("`ports[1]`"), "{msg}");
}

#[test]
fn deny_unknown_fields_reports_the_typo() {
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Strict {
        port: u16,
    }
    let config = Config::builder()
        .toml_str("[s]\nport = 1\nprot = 2")
        .build()
        .unwrap();
    let err = config.get::<Strict>("s").unwrap_err();
    assert_eq!(err.key(), Some("s.prot"));
    assert!(err.to_string().contains("expected one of `port`"), "{err}");
}

#[test]
fn file_errors_name_the_file() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "bad.toml", "[server]\nport = = 1\n");

    let err = Config::builder()
        .base_dir(dir.path())
        .file("bad.toml")
        .build()
        .unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, ConfigError::Parse { .. }));
    assert!(msg.contains("bad.toml") && msg.contains("line 2"), "{msg}");

    let err = Config::builder()
        .base_dir(dir.path())
        .file("missing.toml")
        .build()
        .unwrap_err();
    assert!(
        matches!(&err, ConfigError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    );
    assert!(err.to_string().contains("missing.toml"));
    let io: std::io::Error = err.into();
    assert_eq!(io.kind(), std::io::ErrorKind::NotFound);

    // Optional files are skipped.
    Config::builder()
        .base_dir(dir.path())
        .file_optional("missing.toml")
        .build()
        .unwrap();

    let err = Config::builder().toml_str("x = ").build().unwrap_err();
    assert!(err.to_string().contains("string #1"), "{err}");
}

#[test]
fn invalid_override_values_are_reported() {
    let err = Config::builder().set("a..b", 1).build().unwrap_err();
    assert!(err.to_string().contains("empty key segment"), "{err}");

    let err = Config::builder().set("big", u64::MAX).build().unwrap_err();
    assert_eq!(err.key(), Some("big"));
}

#[test]
fn config_debug_hides_values() {
    let config = Config::builder()
        .toml_str("[db]\npassword = \"hunter2\"")
        .build()
        .unwrap();
    let dbg = format!("{config:?}");
    assert!(dbg.contains("db") && !dbg.contains("hunter2"), "{dbg}");
}

#[test]
fn secret_redacts_and_deserializes_transparently() {
    #[derive(Deserialize, Debug)]
    struct Creds {
        token: Secret<String>,
        pin: Secret<u32>,
    }
    let config = Config::builder()
        .toml_str("token = \"abc\"\npin = \"1234\"")
        .build()
        .unwrap();
    let creds: Creds = config.extract().unwrap();
    assert_eq!(creds.token.expose(), "abc");
    assert_eq!(*creds.pin.expose(), 1234);
    let shown = format!("{creds:?} {}", creds.token);
    assert!(!shown.contains("abc") && !shown.contains("1234"), "{shown}");
    assert!(shown.contains("[REDACTED]"));
    assert_eq!(Secret::from(5).into_inner(), 5);
}

#[test]
fn load_uses_the_process_environment() {
    // Only asserts that the default layering works from the crate directory
    // (no config/ dir there); never mutates the environment.
    let config = Config::load().unwrap();
    assert!(!config.profile().is_empty());
}

#[tokio::test]
async fn app_config_registers_state_and_dep() {
    let settings: Settings = Config::builder()
        .toml_str(DEFAULT_TOML)
        .build()
        .unwrap()
        .extract()
        .unwrap();
    let c = App::new()
        .config(settings)
        .get("/state", |State(s): State<Settings>| async move { s.name })
        .get("/dep", |s: Dep<Settings>| async move {
            s.server.port.to_string()
        })
        .test();
    assert_eq!(c.get("/state").send().await.text(), "shop");
    assert_eq!(c.get("/dep").send().await.text(), "8080");
}

#[tokio::test]
async fn config_errors_convert_into_zapline_errors() {
    let config = Config::builder().toml_str("port = \"x\"").build().unwrap();
    let c = App::new()
        .state(config)
        .get("/", |State(cfg): State<Config>| async move {
            let port: u16 = cfg.get("port")?;
            Ok::<_, Error>(port.to_string())
        })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.status(), 500);
    assert!(
        !r.text().contains("port"),
        "details are not leaked to clients"
    );
}
