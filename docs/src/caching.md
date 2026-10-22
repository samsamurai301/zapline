# Caching

`zapline::cache` (feature `cache`, included in `full`) has three tools. Pick by what you want to skip:

| Skip | Use |
|---|---|
| Recomputing a value (a slow query, an API call) | `Cache` |
| Running a handler at all for the same anonymous `GET` | `ResponseCache` |
| Sending a body the browser already has | `ETag` |

## `Cache`: values

```rust
use std::time::Duration;
use zapline::cache::Cache;
use zapline::prelude::*;

#[model]
struct Stats { users: u64 }

#[get("/stats")]
async fn stats(cache: Cache) -> Result<Json<Stats>> {
    let stats = cache
        .get_or_set("stats", Duration::from_secs(60), || async {
            Ok(Stats { users: 42 }) // the expensive part
        })
        .await?;
    Ok(Json(stats))
}

let app = App::new().cache(Cache::memory()).mount(stats);
```

`get_or_set` is the one to reach for. When 500 requests miss at once, one computes and the rest wait for it (no stampede). Errors aren't cached.

| Call | |
|---|---|
| `cache.get::<T>(key)` | `Option<T>`: a miss when absent, expired, or of another type |
| `cache.set(key, &value, ttl)` | Any `Serialize` value (stored as JSON) |
| `cache.delete(key)` / `delete_prefix("posts:")` / `clear()` | Invalidate after writes |
| `Cache::memory().max_entries(n)` | Capacity (default 10,000). When full, expired entries go first, then the soonest to expire. |

Name keys like `posts:42` so a write can drop a whole family with `delete_prefix`.

## `ResponseCache`: whole pages

```rust
use zapline::cache::ResponseCache;

let app = App::new().nest(
    "/blog",
    Routes::new().mount((index, post)).wrap(ResponseCache::new(Duration::from_secs(300))),
);
```

Repeats are answered from memory with `x-cache: HIT`. It only keeps responses that are safe to share:

- `GET` or `HEAD` with no `Authorization` or `Cookie` header, so logged-in users always get their own page;
- a `200` with a buffered body, no `Set-Cookie`, and no `Cache-Control: private`, `no-store` or `no-cache`.

The query string, `Accept` and `HX-Request` are part of the key, so htmx fragments and full pages don't mix. When the handler set no `Cache-Control`, `public, max-age=<ttl>` is added, so CDNs and browsers help too.

## `ETag`: conditional requests

```rust
let app = App::new().wrap(zapline::cache::ETag);
```

`200` responses to `GET` get a weak `ETag` (a hash of the body). A browser that sends it back in `If-None-Match` gets an empty `304 Not Modified`. Streamed bodies are left alone. [Static files](files.md) have their own ETags already.
