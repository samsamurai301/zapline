# Webhooks

The `webhooks` feature covers both directions. Your app can receive webhooks from other services and check that each one is genuine, and it can send its own, signed, to your customers.

## Receiving

A webhook endpoint is a public URL, so anyone can post to it. A provider proves a request is theirs by signing it with a secret you both know. `Webhook` holds that secret, and the `Signed<T>` extractor checks the signature before your handler runs:

```rust
use serde::Deserialize;
use zapline::prelude::*;
use zapline::webhooks::{Signed, Webhook};

#[derive(Deserialize)]
struct Event {
    r#type: String,
}

async fn billing(Signed(event): Signed<Event>) -> StatusCode {
    println!("Stripe sent {}", event.r#type);
    StatusCode::NO_CONTENT
}

fn app() -> App {
    App::new()
        .post("/webhooks/stripe", billing)
        .meta(Webhook::stripe(std::env::var("STRIPE_WEBHOOK_SECRET").expect("STRIPE_WEBHOOK_SECRET")))
}
```

| Provider | Constructor | Signature it checks |
|---|---|---|
| Anything following [Standard Webhooks](https://www.standardwebhooks.com) (Svix, Clerk, Resend, ...) | `Webhook::standard("whsec_...")?` | `webhook-id`, `webhook-timestamp`, `webhook-signature: v1,<base64>` |
| GitHub | `Webhook::github(secret)` | `X-Hub-Signature-256: sha256=<hex>` |
| Stripe | `Webhook::stripe("whsec_...")` | `Stripe-Signature: t=<time>,v1=<hex>` |

- **Which secret applies:** `.meta(Webhook::...)` sets the secret for the route just added. `.state(Webhook::...)` sets one for the whole app; a route's own `meta` wins.
- **Rejections:**
  - missing or malformed signature headers: `400`, code `webhook_signature_missing`;
  - a signature that doesn't match: `401`, `webhook_signature_invalid`;
  - a body that isn't the expected JSON: `422`.
- **Replays:** signed timestamps more than 5 minutes from now are refused (`401`, `webhook_timestamp_stale`), so a captured request can't be sent again later. Change the window with `.tolerance(..)`. GitHub signs no timestamp; skip repeats by their `X-GitHub-Delivery` id.
- **Rotating secrets:** `.also(old_secret)` accepts both the old and the new secret during the switch.
- **Empty or invalid secrets are refused at startup:** `Webhook::github("")`, `Webhook::stripe("")` and an invalid `.also(..)` panic, since an empty key would let anyone sign. Read the secret with `expect`, not `unwrap_or_default`.
- **Malformed signatures:** a header with no well-formed signature, or more than 8, is `400 webhook_signature_missing`, so junk entries can't make a request expensive to check.
- **Verifying it yourself:** `webhook.verify(headers, body)` gives the same check on raw bytes.
- **Exempt it from cross-site checks.** A provider's server sends no browser `Origin`, so exempt the signed endpoint, and only it: `SameOrigin::new().exempt(["/webhooks/stripe"])` and `Csrf::new().exempt("/webhooks/stripe")`. A whole `/webhooks/` prefix would also exempt any unsigned route added under it later.

## Sending

`WebhookSender` signs payloads the Standard Webhooks way, so receivers can verify them with any Standard Webhooks library, including `Webhook::standard` above:

```rust
use zapline::prelude::*;
use zapline::webhooks::WebhookSender;

async fn notify(url: &str, secret: &str) -> Result<()> {
    let sender = WebhookSender::new(secret)?;
    let delivery = sender
        .send(url, &serde_json::json!({"type": "invoice.paid", "id": "inv_42"}))
        .await?;
    println!("{} delivered after {} attempt(s)", delivery.id, delivery.attempts);
    Ok(())
}
```

- **Secrets:** give each subscriber their own secret from `WebhookSender::generate_secret()?` (`whsec_` and 32 random bytes; an error, never a weaker secret, when the OS has no secure randomness). Store it, and show it to them once.
- **Retries:** a connection failure, timeout, `408`, `429` or `5xx` is retried 3 times, waiting 1 s, 2 s, then 4 s (`.retries(n)`, `.backoff(d)`). Every attempt carries the same `webhook-id`, so receivers can drop duplicates. Each attempt is signed with the current time, so a late retry isn't refused as stale.
- **No retry:** another `4xx` (the receiver refused it) isn't retried. When every attempt fails, `send` returns `502` (code `webhook_delivery_failed`) with the last answer.
- **Durable delivery:** `send` retries for seconds, within one request or job. For delivery that survives restarts, call it from a [durable job](jobs.md), which retries with backoff of its own.
- **Unsafe URLs are refused.** Subscribers type their own URLs, so the sender uses the [outbound client](outbound.md) with its guard on: private, loopback and cloud-metadata addresses are refused (`400`, code `upstream_blocked`) and never retried.
