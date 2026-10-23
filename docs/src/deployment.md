# Deployment

## Build

```toml
# Cargo.toml of your app
[profile.release]
lto = "fat"
codegen-units = 1
panic = "abort"      # note: CatchPanic cannot catch panics with abort
```

```sh
cargo build --release
```

Consider `mimalloc` as the global allocator for throughput-heavy services.

## One binary, one command

A zapline app can be a single executable: templates, static files, migrations,
translations and config compiled in. `zapline ship` puts it on a Linux server
and swaps it in without dropping a request.

### Compile the directories in: `zapline::embed!`

```rust,ignore
App::new()
    .views(zapline::embed!("templates"))
    .assets("/static", zapline::embed!("static"))
    .database(Database::from_env().migrations(zapline::embed!("migrations")))
    .i18n(I18n::load(zapline::embed!("locales"))?)
```

`embed!("dir")` reads the directory (relative to the crate's `Cargo.toml`)
at compile time and gives a [`Dir`](https://docs.rs/zapline/latest/zapline/embed/struct.Dir.html).
Everything that takes a directory takes a `Dir`: `App::views`, `App::assets`,
`ServeDir::new`, `Database::migrations`, `I18n::load` and
`Config::builder().standard_in(..)` (for `config/`).

- **Release builds** serve from memory. Fingerprinted asset URLs, ETags,
  ranges and precompressed `.gz`/`.br` files work as they do from disk; the
  hashes are computed at compile time, so startup reads nothing.
  Migrations have the same checksums as the files, so a database migrated
  from disk accepts the embedded copy.
- **Debug builds** read the real directory while it exists, so editing a
  template or a stylesheet still needs only a browser refresh.
  `Dir::in_memory()` uses the compiled copy in debug too (for tests).

Cargo rebuilds when an embedded file changes, but it doesn't watch for files
that are added. The `fullstack` template has a `build.rs` for that; in your
own project, add one:

```rust,ignore
// build.rs
fn main() {
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        for dir in ["templates", "static", "migrations"] {
            println!("cargo:rerun-if-changed={dir}");
        }
    } else {
        println!("cargo:rerun-if-changed=build.rs");
    }
}
```

(`zapline ship` also sets `ZAPLINE_EMBED_STAMP`, which forces the re-read.)

### Deploy: `zapline ship`

```sh
zapline ship deploy@203.0.113.7 --dry-run   # every command and file, nothing runs
zapline ship deploy@203.0.113.7             # build, upload, swap
zapline ship deploy@203.0.113.7 --rollback  # the previous release, the same way
```

The server needs systemd and a login that is root or can `sudo`. Only the
system `ssh` and `scp` are used, so keys, agents, `~/.ssh/config` hosts and
known hosts work as in your terminal. One ssh connection is shared by
the whole run.

What one run does:

1. `ssh host uname -sm` picks the build: a plain `cargo build --release` when
   this machine has the same OS and CPU, otherwise an error that says what
   to use. `--static` builds for `<cpu>-unknown-linux-musl`, which runs on
   any Linux. It uses `musl-gcc` (`apt install musl-tools`), a
   `<cpu>-linux-musl-gcc` cross compiler, or `cargo zigbuild`, whichever is
   installed, and says what to install when none is. `--target <triple>`
   picks the target yourself.
2. With `styles/app.css`, Tailwind builds the minified `static/app.css` first.
3. `scp` uploads the binary, its SHA-256, a systemd unit, the settings file
   and the install script to a private directory in `/tmp`.
4. The script installs and swaps:

   | First run only | Every run |
   |---|---|
   | A system user named after the app | The binary in `/opt/<app>/releases/<id>/`, checked against its SHA-256 |
   | `/etc/<app>/env` with `PORT`, `APP_ENV=production`, the database URL (SQLite in `/var/lib/<app>/`), and a generated value for each `*_SECRET`/`*_KEY` in `.env.example` | `--env KEY=VALUE` settings merged into `/etc/<app>/env` |
   | `net.ipv4.tcp_migrate_req=1` (see below) | `/etc/systemd/system/<app>@.service` rewritten |

   It then runs `systemctl start <app>@<id>` next to the running release. Both
   bind the same port (`SO_REUSEPORT`). `systemctl start` returns only once
   the new process has answered its own health check (`/health/ready` with
   the `Health` plugin, otherwise `/`; `--health` picks another path) and
   said `READY=1`. The script checks the port with curl, points
   `/opt/<app>/current` at the new release, enables it for boot, and stops
   the old one. The old release drains: it finishes its requests and closes
   its connections. Only the newest three releases are kept (`--keep N`).

Any failure before the old release stops leaves it serving. This covers a
build error, a corrupted upload, a release that crashes on startup, a
failing health check or a port that doesn't answer. The new release and its
unit change are undone, and the script prints the new release's last log
lines. If the new release dies right after the swap, the old one is started
again.

On the server:

```sh
journalctl -u 'myapp@*' -f            # logs
sudoedit /etc/myapp/env               # settings; ship again (or restart) to apply
systemctl status 'myapp@*'
ls /opt/myapp/releases                # what --rollback can go back to
```

### Why no request is dropped

- **The new process is ready before it takes traffic.** With
  `ZAPLINE_READY_PATH` set, the server requests that path from itself,
  in-process, before it binds the port. It retries until the answer is below
  400, for up to `ZAPLINE_READY_TIMEOUT` seconds (default 30; the unit sets
  60). A 404 fails at once. With `NOTIFY_SOCKET` set (systemd `Type=notify`),
  it sends `READY=1` once it is listening and `STOPPING=1` when it starts to
  drain.
- **Two processes share the port.** `ZAPLINE_REUSE_PORT=1` makes every
  listener use `SO_REUSEPORT`. The default thread-per-core runtime always
  does on Linux.
- **Queued connections move over.** When a listener closes, Linux drops the
  connections still waiting in its accept queue. With
  `net.ipv4.tcp_migrate_req=1` (Linux 5.14+, which `zapline ship` turns on),
  it hands them to the other listener on the port instead.
- **Keep-alive connections end cleanly.** For `ServerConfig::drain_grace`
  (default 1s) after draining starts, a connection may still send its next
  request. The response says `connection: close`, so the client opens its
  next connection to the new release. Closing an idle keep-alive connection
  right away would reset a request that is already on its way. Connections
  still idle when the grace ends are closed.

On a test server, 8 clients (half of them keeping connections alive) sent
about 150,000 requests to a `fullstack` app through a swap, and none failed.

The same environment variables work under any supervisor that can start a
second instance before stopping the first.

## Container image

Projects generated by `zapline new` (`api`, `full` and `fullstack` templates) include a two-stage Dockerfile. The first stage compiles your dependencies in their own cached layer, then your code. The runtime image is `debian:bookworm-slim` with CA certificates, running as a non-root user. The `fullstack` binary has `templates/`, `static/` and `migrations/` compiled in ([`embed!`](#compile-the-directories-in-zaplineembed)), so the image holds just the binary, and it keeps the SQLite file in `/home/app/data`, which is meant to be a volume.

The app listens on `ADDR` if set, otherwise on `0.0.0.0:$PORT` (the image sets `PORT=8080`). Hosts that choose the port, such as Railway and Render, override `PORT`.

## Hosting configs: `zapline g deploy`

```sh
zapline g deploy fly        # fly.toml
zapline g deploy railway    # railway.json
zapline g deploy render     # render.yaml (a Blueprint)
zapline g deploy compose    # compose.yaml: any server with Docker
```

Each target builds the project's Dockerfile, and the CLI adds one if the project has none. The config follows what the app uses:

| The project has | The config gets |
|---|---|
| A SQLite database (the `fullstack` default) | A volume at `/home/app/data` (Fly mount, Render disk, Compose volume) |
| zapline's `postgres` feature | A Postgres service with `DATABASE_URL` wired to it (Compose, Render) |
| zapline's `redis` feature | A Redis service with `REDIS_URL` (Compose) |
| The `Health` plugin | Health checks on `/health/ready` |
| Settings in `.env.example` (`SESSION_SECRET`, `SMTP_URL`, ...) | Entries that read them from the environment; on Render, `*_SECRET` and `*_KEY` are generated |

After writing the file, the CLI prints what is left to do on that host, such as `fly volumes create data` and `fly secrets set SESSION_SECRET=$(openssl rand -base64 48) ...`. Nothing is deployed and no account is touched until you run those commands.

With Compose, put the settings in a `.env` file next to `compose.yaml`. It is in `.dockerignore`, so it never ends up in the image:

```sh
echo "SESSION_SECRET=$(openssl rand -base64 48)" > .env
docker compose up --build -d     # http://localhost:8080
```

## Kubernetes

zapline handles `SIGTERM` gracefully. It stops accepting new connections, lets in-flight requests finish (up to `shutdown_timeout`, default 30s), and then runs shutdown hooks. Point probes at the health plugin:

```yaml
livenessProbe:
  httpGet: { path: /health/live, port: 8080 }
readinessProbe:
  httpGet: { path: /health/ready, port: 8080 }
terminationGracePeriodSeconds: 35   # > shutdown_timeout
```

Scrape `/metrics` with Prometheus. Use JSON logs (`Telemetry::new().json().init()`) for log aggregation.

## Behind a reverse proxy

- Rate limiting by IP must trust only your proxy: `RateLimit::per_ip(..).trust_forwarded_for(["10.0.0.0/8"])`.
- Terminate TLS at the proxy, or in zapline with `App::tls(TlsConfig::from_pem_files(cert, key)?)`.
- On Unix you can listen on a domain socket: `App::run_unix("/run/app.sock")`.

## Configuration in production

Set `APP_ENV=production` to layer `config/production.toml` over `config/default.toml`. Inject secrets through environment variables such as `APP__DATABASE__URL=...`. Wrap them in `Secret<String>` so they never show up in logs.
