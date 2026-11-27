# Calling other services

Use `HttpClient` from `zapline::outbound` (feature `http-client`) when a handler calls another HTTP API. It comes with production defaults, so a slow or broken upstream can't hang your handler, and a URL a user typed can't reach your internal network.

```rust
use serde::{Deserialize, Serialize};
use zapline::outbound::HttpClient;
use zapline::prelude::*;

#[derive(Deserialize, Serialize)]
struct Rate {
    usd: f64,
}

async fn rate(http: HttpClient) -> Result<Json<Rate>> {
    let rate: Rate = http
        .get("https://api.example.com/rate")
        .bearer(&std::env::var("RATES_TOKEN").unwrap_or_default())
        .send()
        .await?
        .error_for_status()   // non-2xx → 502 with the upstream's status
        .await?
        .json()
        .await?;
    Ok(Json(rate))
}

fn app() -> App {
    App::new()
        .state(HttpClient::new().header("x-client", "shop"))   // optional: configure once
        .get("/rate", rate)
}
```

A handler that takes `HttpClient` gets the one registered with `.state(..)`, or a shared default. Requests are built with `.get` / `.post` / `.put` / `.patch` / `.delete`, then `.header(..)`, `.bearer(..)`, `.json(&value)` or `.body(..)`, and `.send().await`. The response has `.status()`, `.headers()`, `.json::<T>()`, `.text()`, `.bytes()`, and `.chunk()` for streams.

## Defaults

| Setting | Default | Change with |
|---|---|---|
| Timeout, until headers and again for the body (or between chunks when streaming) | 30 s | `.timeout(d)`, per request too |
| Connecting | 10 s | `.connect_timeout(d)` |
| Retries for `GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS` after a failed connection, a timeout, `429`, `502`, `503` or `504` | 2, with backoff (100 ms, 200 ms, ... ± 25%), or what `Retry-After` asks for (up to 10 s) | `.retries(n)` |
| Largest buffered body (`json`, `text`, `bytes`) | 16 MiB | `.max_response_size(n)` |
| Private and reserved networks | refused | `.allow_private_networks(true)` |

`POST` and `PATCH` are never retried: they may have taken effect. To retry them safely, send an `Idempotency-Key` to an API that supports it (zapline's own [`Idempotency`](hardening.md#idempotency) middleware does).

## Errors

Every failure is an `Error` that a handler can return with `?`:

| Failure | Answer |
|---|---|
| The address is refused (see below) | `400`, code `upstream_blocked` |
| Not an absolute `http://` or `https://` URL | `400`, code `upstream_invalid_url` |
| No connection | `502`, code `upstream_unreachable` |
| `error_for_status()` on a non-2xx answer | `502`, code `upstream_status` (the message has the status and the start of the body) |
| Body larger than the limit | `502`, code `upstream_too_large` |
| `json()` on a body that doesn't match | `502`, code `upstream_invalid_json` |
| Timeout | `504`, code `upstream_timeout` |

## Server-side request forgery

When your app fetches a URL that came from a user (a webhook endpoint, an avatar URL, a link preview), an attacker can point it at `http://169.254.169.254/` (the cloud metadata endpoint, which hands out credentials), `http://localhost:6379/`, or an internal admin service. `HttpClient` refuses these by default:

- **Refused addresses:** loopback, private (10/8, 172.16/12, 192.168/16, `fc00::/7`), link-local (169.254/16, `fe80::/10`), shared (100.64/10), documentation, benchmarking, multicast, broadcast and reserved ranges, IPv4 inside IPv6 (`::ffff:10.0.0.1`), and NAT64/6to4 prefixes, which can embed a private IPv4.
- **Checked after DNS:** hostnames are checked after they're resolved, so `localhost`, or a domain that points (or rebinds) to `10.0.0.5`, is refused too.
- **Redirects aren't followed,** so a public URL can't redirect to an internal one. A `3xx` comes back as the response.
- **The opt-out:** `.allow_private_networks(true)` turns this off. Use it only for a client that talks to your own internal services, never for URLs users supply. If you need both, keep two clients.
