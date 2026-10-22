# Email

`zapline::mail` sends email over SMTP in production, keeps it in a **dev inbox** you can open in the browser while developing, and captures it in tests.

```rust
use zapline::mail::{Email, Mailer};
use zapline::prelude::*;

#[post("/invite")]
async fn invite(mail: Mailer) -> Result<&'static str> {
    mail.send(
        Email::to("ada@example.com")
            .subject("You're invited")
            .text("Join us: https://example.com/join")
            .html("<p>Join us: <a href=\"https://example.com/join\">example.com</a></p>"),
    )
    .await?;
    Ok("sent")
}

fn main() -> zapline::Result<()> {
    App::new()
        .mailer(Mailer::from_env()?)
        .mount(invite)
        .run("0.0.0.0:8080")?;
    Ok(())
}
```

Requirements: the `mail` feature (included in `full`).

## Configuration

`Mailer::from_env()` reads:

| Variable | Example | |
|---|---|---|
| `SMTP_URL` | `smtps://user:pass@smtp.example.com` | Implicit TLS (port 465). Use `smtp://user:pass@host:587?tls=required` for STARTTLS and `smtp://localhost:1025` for a local catcher like Mailpit. |
| `MAIL_FROM` | `Acme <no-reply@acme.dev>` | The sender when an `Email` doesn't set `.from(..)`. Defaults to `no-reply@localhost`. |

Without `SMTP_URL`, a **debug** build uses the outbox described below, and a **release** build refuses to start, so production never drops mail silently.

`Mailer::smtp(url)` and `Mailer::outbox()` build the two kinds directly, and `.default_from(addr)` sets the sender in code.

## The dev inbox

An outbox mailer sends nothing. It logs each message and serves the last 50 at **`GET /_dev/mail`**: subject, recipients, the HTML body in a sandboxed frame and the text body with clickable links. Sign-up confirmations and password-reset links work end to end on your machine with no mail server. The page only exists when the outbox is in use, and the [dev console](dev-console.md) links it.

## Sending in the background

Sending inline makes the request wait for the SMTP server and fail if it is down. With a database, `Email` is also a [durable job](jobs.md): register it once and enqueue messages instead of sending them.

```rust
App::new()
    .database(Database::from_env())
    .mailer(Mailer::from_env()?)
    .jobs(|queue| queue.register::<Email>());

// in a handler
jobs.enqueue(Email::to(&user.email).subject("Welcome").text(body)).await?;
```

A failed send is retried with backoff (5 s, 20 s, 80 s, ...).

## Testing

```rust
let mailer = Mailer::outbox();
let client = App::new().mailer(mailer.clone()).mount(invite).test();

client.post("/invite").send().await.assert_status(200);
let sent = mailer.sent();
assert_eq!(sent[0].to, ["ada@example.com"]);
assert!(sent[0].text.as_deref().unwrap().contains("/join"));
```

`Email` is a plain struct with public fields, so assertions can look at anything.
