# Running several instances (Redis)

One zapline process goes a long way (see [Performance](performance.md)), but for zero-downtime deploys or more traffic you will run two or more instances behind a load balancer. State kept in memory then splits between them: a cache entry, a live update or a rate-limit bucket on one instance is invisible to the others.

What already works across instances, because it lives in the database:

| Piece | Shared through |
|---|---|
| [Sessions](sessions.md) with `App::database_sessions` | The `zapline_sessions` table |
| [Durable jobs](jobs.md) | The `zapline_jobs` table (`FOR UPDATE SKIP LOCKED` on Postgres, so instances share one queue) |
| [Uploaded files](storage.md) with `Storage::s3` | The bucket |

For the rest, the `redis` feature adds a small built-in Redis client (no extra dependency) and Redis-backed versions of the in-memory pieces:

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

The client speaks RESP2 over TCP with `AUTH` (password, or user and password) and a database number from the URL. TLS (`rediss://`) is not supported yet, so run Redis on a private network (your platform's internal network, a VPC, or a sidecar), or reach a TLS-only service through a tunnel.

## Without Redis

If you'd rather not run Redis: keep sessions and jobs in the database as above, use `Cache::memory()` per instance (each instance warms its own cache; fine for data that may be a little stale), and accept that per-instance rate limits are `limit × instances` in total. Live updates are the one piece that needs a shared channel to reach visitors connected to other instances.
