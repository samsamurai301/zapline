# Configuration

Load layered settings into your own struct, then hand it to the app:

```rust
use serde::Deserialize;
use zapline::config::{Config, Secret};
use zapline::di::Dep;
use zapline::prelude::*;

#[derive(Clone, Deserialize)]
struct Settings {
    server: Server,
    database: Database,
}

#[derive(Clone, Deserialize)]
struct Server {
    host: String,
    port: u16,
}

#[derive(Clone, Deserialize)]
struct Database {
    url: Secret<String>,   // never printed by Debug/Display
    pool: u32,
}

async fn info(settings: Dep<Settings>) -> String {
    format!("pool size {}", settings.database.pool)
}

fn main() -> std::io::Result<()> {
    let settings: Settings = Config::load()?.extract()?;
    let addr = (settings.server.host.clone(), settings.server.port);
    App::new()
        .config(settings)          // State<Settings> and Dep<Settings>
        .get("/info", info)
        .run(addr)
}
```

Requires the Cargo feature `config` (on by default; it adds the `toml` crate). `ConfigError` converts into `std::io::Error` and into `zapline::Error`, so `?` works in `main` and in handlers.

## Layers

`Config::load()` merges these layers, **lowest precedence first**:

| Layer | Example |
|---|---|
| `config/default.toml` (optional) | `[database]`<br>`url = "postgres://localhost/app"` |
| `config/{profile}.toml` (optional) | `config/production.toml` |
| `.env` (optional) | `APP__DATABASE__POOL=20`. It never overrides variables that are really set. |
| Environment variables `APP__*` | `APP__DATABASE__URL` → `database.url` |

Tables are **deep-merged**: a later layer overrides single keys and keeps the siblings. A `production.toml` that only sets `[database] url` leaves `database.pool` from `default.toml` alone.

```toml
# config/default.toml
[server]
host = "127.0.0.1"
port = 8080

[database]
url = "postgres://localhost/app"
pool = 5

# config/production.toml
[server]
host = "0.0.0.0"

[database]
pool = 20
```

```sh
APP_ENV=production APP__DATABASE__URL=postgres://db.internal/app ./my-app
```

### Profiles

The active profile is, in order: the builder's `.profile(..)`, the `APP_ENV` environment variable, `ZAPLINE_ENV`, and finally `development`. An empty variable counts as unset. `APP_ENV` follows the configured prefix, so with `.env_prefix("MYAPP")` it is `MYAPP_ENV`. Both can also be set in the `.env` file. `config.profile()` returns the name, which you can use to switch behavior (for example, to require TLS in `production`).

Profile names may only contain ASCII letters, digits, `-` and `_`, so `APP_ENV=../../etc` can never reach the file system. Anything else is a `ConfigError::InvalidProfile`.

### Environment variables

Names are `PREFIX`, then the path segments, all joined by the separator (default `__`). Segments are lowercased, and an existing key with different casing matches case-insensitively:

| Variable | Sets |
|---|---|
| `APP__SERVER__PORT=9000` | `server.port` |
| `APP__DATABASE__MAX_CONNECTIONS=50` | `database.max_connections` |

Values are interpreted leniently, so you never need to quote anything in a shell:

- An environment value becomes a TOML scalar or array only if that round-trips exactly (`8080`, `true`, `1.5`, `[1, 2]`). Otherwise it stays a string. So `007`, `0x1F` and `1.10` are kept as written.
- Typed extraction is lenient where environment variables need it: `"8080"` reads as a number, `8080` as a string, `"a,b"` as a list, and `yes`, `on`, `1` as `true`.
- Field and enum variant names match case-insensitively when there is no exact match.

### `.env` files

`KEY=value`, `export KEY=value`, `# comments`, inline comments after unquoted values (` # ...`), `'single'` quotes (literal), and `"double"` quotes with `\n \r \t \" \\ \$` escapes. Quoted values may span lines. Variables are **not** expanded (`${VAR}` stays literal). Values from `.env` feed the environment layer and profile selection, but are never exported into the process, and real environment variables always win. A malformed file is an error that names the line.

## The builder

`Config::load()` is `Config::builder().standard().build()`. Use the builder for anything else:

```rust
use zapline::config::Config;

fn load() -> Result<Config, zapline::config::ConfigError> {
    Config::builder()
        .toml_str("[server]\nport = 8080")        // embedded defaults
        .file_optional("config/default.toml")
        .file_optional("config/{profile}.toml")   // {profile} is replaced
        .dotenv(".env")
        .env_prefix("MYAPP")                      // MYAPP__SERVER__PORT
        .set_default("server.workers", 4)         // below every other layer
        .set("server.tls", false)                 // above every other layer
        .build()
}
```

Layers merge in this order: `set_default` values, then files and TOML strings **in the order added**, then the environment (including `.env`), then `set` overrides.

| Method | Effect |
|---|---|
| `standard()` | The layers `Config::load` uses: `config/default.toml`, `config/{profile}.toml`, `.env` and the `APP` prefix. It keeps a prefix or `.env` path you already set. |
| `file(path)` | A required TOML file. A missing file is an error. `{profile}` in the path is replaced. |
| `file_optional(path)` | A TOML file that is skipped when it doesn't exist |
| `toml_str(text)` | An inline TOML document, for embedded defaults |
| `env_prefix(prefix)` | Enables the environment layer: `PREFIX__A__B=v` sets `a.b`. Also selects the `PREFIX_ENV` variable for the profile. |
| `separator(sep)` | Nesting separator in variable names (default `__`). Panics if empty. |
| `dotenv(path)` | Reads variables from a `.env` file (skipped when missing) |
| `env_source(vars)` | Uses these `(name, value)` pairs instead of the process environment. For tests and sandboxes. |
| `profile(name)` | Forces the profile |
| `base_dir(dir)` | Resolves relative file and `.env` paths against `dir` instead of the working directory |
| `set(key, value)` | Overrides a dotted key with any serializable value. Highest precedence. |
| `set_default(key, value)` | A fallback below all other layers |
| `build()` | Reads and merges everything |

`env_source` makes configuration trivially testable, without touching real environment variables:

```rust
use zapline::config::Config;

#[test]
fn environment_overrides_files() {
    let config = Config::builder()
        .toml_str("[database]\nurl = \"postgres://localhost/dev\"\npool = 5")
        .env_prefix("MYAPP")
        .env_source([("MYAPP__DATABASE__POOL", "20")])
        .build()
        .unwrap();
    assert_eq!(config.get::<u32>("database.pool").unwrap(), 20);
}
```

## Reading values

| Method | Returns |
|---|---|
| `extract::<T>()` | The whole tree deserialized into `T` |
| `get::<T>("a.b")` | One value. Keys are dotted, with `servers[0].host` or `servers.0.host` for arrays. A missing key is an error unless `T` is an `Option`. |
| `get_or(key, default)` | `default` when the key is missing. A present but invalid value is still an error. |
| `contains(key)` / `value(key)` / `table()` | Presence check, the raw `toml::Value`, the whole merged table |
| `origin(key)` | Which layer set a key, for diagnostics: ``env `APP__SERVER__PORT` ``, ``file `config/production.toml` `` |
| `profile()` | The active profile |

```rust
use zapline::config::Config;

fn read(config: &Config) -> Result<(), zapline::config::ConfigError> {
    let port: u16 = config.get("server.port")?;
    let workers: usize = config.get_or("server.workers", 4)?;
    let first_host: String = config.get("hosts[0]")?;
    let timeout: Option<u64> = config.get("server.timeout_secs")?;
    println!("{port} {workers} {first_host} {timeout:?}");
    Ok(())
}
```

Use `#[serde(default)]` and `#[serde(deny_unknown_fields)]` on your settings structs as usual. The latter turns a typo such as `prot = 8080` into an error that names the key.

## Using settings in the app

`App::config(settings)` registers the value as both `State<T>` and `Dep<T>` (it needs `T: Clone + Send + Sync + 'static`). Pass it to services in provider factories with `res.dep::<Settings>()?`:

```rust
use std::sync::Arc;
use serde::Deserialize;
use zapline::prelude::*;

#[derive(Clone, Deserialize)]
struct Settings {
    greeting: String,
}

struct Greeter {
    greeting: String,
}

fn app(settings: Settings) -> App {
    App::new()
        .config(settings)
        .provide_with(|res: Resources| async move {
            let settings: Arc<Settings> = res.dep::<Settings>()?;
            Ok(Greeter { greeting: settings.greeting.clone() })
        })
}
```

See [Dependency injection](di.md) for how factories work. You can also register the `Config` itself with `.state(config)` to read dynamic keys in handlers.

## Secrets

`Secret<T>` wraps a value that must not end up in logs. Its `Debug` and `Display` print `[REDACTED]`, it deserializes transparently (a `Secret<String>` field reads a plain string), and it is not `Serialize`, so it cannot be returned from a handler by accident. Read the value with `.expose()`. The method name is easy to grep for:

```rust
use zapline::config::Secret;

fn demo() {
    let key = Secret::new("hunter2".to_string());
    assert_eq!(format!("{key:?}"), "[REDACTED]");
    assert_eq!(key.expose(), "hunter2");
}
```

`Config`'s own `Debug` output lists only the profile and the top-level keys, never values.

Inject secrets through environment variables (`APP__DATABASE__URL=...`) rather than committing them to `config/*.toml`.

## Error messages

Configuration errors name the key, the expected type and the layer that set the value:

```text
invalid config key `server.port`: invalid value: string "eighty", expected u16 (set by env `APP__SERVER__PORT`)
invalid config key `server.port`: invalid value: integer `70000`, expected u16 (set by string #1)
missing config key `database.url`
invalid config key `s.prot`: unknown key, expected one of `port` (set by string #1)
config file `config/nope.toml`: No such file or directory (os error 2)
config file `config/bad.toml` is invalid: TOML parse error at line 2, column 8 ...
config file `.env` is invalid: line 2: expected `KEY=value`
invalid config profile "pro d": use ASCII letters, digits, `-` and `_`
```

`ConfigError` is an enum (`Io`, `Parse`, `Missing`, `Invalid`, `InvalidProfile`) with `.key()` for the key path. When one reaches a request handler through `?` it becomes a `500` whose details stay in the server log, so a bad configuration value is never shown to clients.
