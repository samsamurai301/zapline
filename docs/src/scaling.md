# Running several instances (Redis)

One zapline process goes a long way (see [Performance](performance.md)), but for zero-downtime deploys or more traffic you will run two or more instances behind a load balancer. State kept in memory then splits between them: a cache entry, a live update or a rate-limit bucket on one instance is invisible to the others.

What already works across instances, because it lives in the database:

| Piece | Shared through |
|---|---|
| [Sessions](sessions.md) with `App::database_sessions` | The `zapline_sessions` table |
| [Durable jobs](jobs.md) | The `zapline_jobs` table (`FOR UPDATE SKIP LOCKED` on Postgres, so instances share one queue) |
| [Uploaded files](storage.md) with `Storage::s3` | The bucket |

For the rest, the `redis` feature adds a small built-in Redis client (no extra dependency; `redis-tls` adds `rediss://`) and Redis-backed versions of the in-memory pieces:

```rust
use zapline::cache::Cache;
use zapline::live::Live;
use zapline::middleware::RateLimit;
use zapline::redis::{Redis, RedisStore};

let redis = Redis::from_env()?; // REDIS_URL=redis://:password@host:6379/0

App::new()
    .cache(Cache::redis(redis.clone()))                      // was Cache::memory()
    .live(Live::new().redis(redis.clone()))                  // updates reach every instance
    .wrap(RateLimit::per_ip(100, Duration::from_secs(60)).redis(redis.clone()))
    .wrap(SessionLayer::new(RedisStore::new(redis.clone()))) // or database_sessions
    .redis(redis)                                            // `redis: Redis` in handlers
```

| Piece | With Redis | If Redis is down |
|---|---|---|
| `Cache::redis` | Entries under `zapline:cache:`, expiring with `PX`. `delete_prefix` and `clear` use `SCAN` + `UNLINK`, so they never block Redis. | Reads miss and writes are skipped, with a warning in the logs: pages get slower, not broken. |
| `Live::new().redis(..)` | Each update is delivered to this instance's subscribers right away and published on `zapline:live:{topic}`; every other instance delivers it to its own. Order is kept per publisher. | Updates stay on the instance that published them; the listener reconnects with backoff. |
| `RateLimit::...redis(..)` | The same GCRA as in memory, run as one Lua script per request on Redis's clock, so instances with skewed clocks agree. | Requests are let through, with a warning. |
| `RedisStore` | Sessions under `zapline:session:`, expiring with the session. | Requests that need the session fail with `503`. |

`Redis::cmd` runs any command, for whatever else you need to share:

```rust
#[post("/visits")]
async fn visit(redis: Redis) -> Result<String> {
    let n = redis.cmd(["INCR", "visits"]).await?.int().unwrap_or(0);
    Ok(n.to_string())
}
```

A Redis error reply becomes an error (`500`), and an unreachable Redis a `503`. Idle connections are kept for reuse (up to 16), and a command on one that the server closed is retried once on a fresh connection.

The client speaks RESP2 with `AUTH` (password, or user and password) and a database number from the URL: over TCP for `redis://`, and over TLS for `rediss://` (below).

## Redis over TLS

Managed Redis services (AWS ElastiCache and MemoryDB, Azure Cache, Upstash, Redis Cloud, Heroku, DigitalOcean...) usually hand out a `rediss://` URL. Turn on the `redis-tls` feature and use it as is:

```toml
zapline = { version = "0.1", features = ["redis-tls"] } # includes `redis`
```

```sh
REDIS_URL=rediss://default:password@my-cache.example.com:6380/0
```

Everything above (the cache, live updates, rate limits, sessions, `Redis::cmd`) then runs over TLS 1.2 or 1.3 with rustls, and nothing else changes: connections are still pooled, and one the server closed is replaced by a fresh handshake. The server's certificate is checked against the Mozilla root store (compiled in, so it works in `scratch` and distroless images) for the URL's host, which is also sent as SNI.

For a self-hosted Redis with its own CA, or a certificate for a name other than the host you connect to, add parameters to the URL (values percent-encoded):

| Parameter | Meaning |
|---|---|
| `ca_file` | A PEM file of CA certificates to trust instead of the Mozilla roots: your private CA, or a self-signed server certificate |
| `sni` | The name sent as SNI and checked on the certificate, when it differs from the host: connecting by IP, through a tunnel or port-forward, or to a load balancer |
| `cert_file`, `key_file` | A client certificate chain and its key (PKCS#8, PKCS#1 or SEC1 PEM), for servers that require mutual TLS (`tls-auth-clients yes`) |

```sh
REDIS_URL='rediss://:s3cret@10.0.0.5:6380/0?ca_file=/etc/redis/ca.pem&sni=redis.internal'
REDIS_URL='rediss://:s3cret@redis.internal:6380?ca_file=/run/secrets/ca.pem&cert_file=/run/secrets/client.pem&key_file=/run/secrets/client.key'
```

Files are read when the client is made (`Redis::new` / `Redis::from_env`), so a missing or invalid one stops the app at startup rather than on the first request. When the PEM lives in a secret rather than a file, pass it in memory instead:

```rust
let secret = |name: &str| std::env::var(name).map_err(|_| Error::internal(format!("{name} is not set")));
let redis = Redis::from_env()? // REDIS_URL=rediss://...
    .with_ca_pem(secret("REDIS_CA_PEM")?)?
    .with_client_cert_pem(secret("REDIS_CLIENT_CERT")?, secret("REDIS_CLIENT_KEY")?)?;
```

There is deliberately no switch to skip certificate verification: for a self-signed server, trust its certificate with `ca_file`. A handshake that fails (an unknown CA, a certificate for another name, a missing client certificate) is a `503` whose message says why, for example `TLS handshake with Redis at 10.0.0.5:6380 failed: invalid peer certificate: UnknownIssuer`.

Without the `redis-tls` feature, a `rediss://` URL is refused at startup with an error that names the feature, and the `redis` feature alone pulls in no TLS code.

## Without Redis

If you'd rather not run Redis: keep sessions and jobs in the database as above, use `Cache::memory()` per instance (each instance warms its own cache; fine for data that may be a little stale), and accept that per-instance rate limits are `limit × instances` in total. Live updates are the one piece that needs a shared channel to reach visitors connected to other instances.
