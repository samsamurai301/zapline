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

## Recipients and headers

```rust
Email::to("ada@example.com")
    .and_to("grace@example.com")       // more `To` recipients
    .cc("team@example.com")             // visible copies
    .bcc("audit@example.com")           // blind copies
    .reply_to("support@example.com")
    .header("List-Unsubscribe", "<https://example.com/unsubscribe?u=42>")
    .subject("Release notes")
    .text("...");
```

Blind copies go to the SMTP envelope only: the sent message has no `Bcc` header, so no recipient can see them. `header(name, value)` adds any other header; sending fails with a clear error for a value with a line break (header injection), a name that isn't a valid header name, or a header that has its own method (`To`, `Subject`, `Content-Type`, ...).

## Attachments and inline images

```rust
let email = Email::to(&user.email)
    .subject("Your invoice")
    .text("Your invoice is attached.")
    .html(r#"<img src="cid:logo" alt="Acme"><p>Your invoice is attached.</p>"#)
    .attach("invoice-42.pdf", "application/pdf", pdf_bytes)
    .inline("logo", "image/png", logo_png);
```

`attach` adds a file the recipient can download; `inline` embeds an image the HTML shows with `src="cid:<id>"`. Together they may be at most `MAX_ATTACHMENT_BYTES` (10 MiB, before base64 makes them a third larger): a larger email fails to send, and fails to enqueue, with an error that says so. For big files, send a [storage link](storage.md#links-are-private) instead.

`email.to_mime(default_from)` returns the message exactly as it would be sent, for saving as an `.eml` file or checking in tests.

## Templated email: `zapline g mailer`

With the `views` feature, `Email::template` renders the subject and both bodies from templates in the app's `templates/` directory:

```rust
use zapline::view::{context, Views};

#[post("/signup")]
async fn signup(views: Views, mail: Mailer, Json(form): Json<Signup>) -> Result<StatusCode> {
    let url = format!("https://app.example.com/confirm?t={}", token(&form.email));
    let email = Email::to(&form.email)
        .template(&views, "mail/welcome", context! { name => form.name, url })?;
    mail.send(email).await?;
    Ok(StatusCode::CREATED)
}
```

`templates/mail/welcome.html` is the HTML body; its `{% block subject %}` is the subject, so it fits in `<title>` (and in a shared layout that other emails extend):

```jinja
<title>{% block subject %}Welcome, {{ name }}{% endblock %}</title>
<p>Hi {{ name }}, please <a href="{{ url }}">confirm your address</a>.</p>
```

`templates/mail/welcome.txt`, when it exists, is the plain-text body (not HTML-escaped). Without it the text part is made from the HTML: paragraphs and line breaks kept, links as `label (url)`, lists as `- item`. Either template may be missing, not both.

`zapline g mailer welcome` writes all of it: `src/mail/welcome.rs` with `pub fn welcome(views: &Views, to: &str) -> Result<Email>`, declared in `src/mail/mod.rs` (and `pub mod mail;` in `src/lib.rs`), the two templates, and `tests/welcome_email.rs`, which renders the email, sends it to an outbox mailer and checks the subject, the recipient and both bodies. It turns on the `mail` and `views` features and tells you what is left to register on the app.

## The dev inbox

An outbox mailer sends nothing. It logs each message and serves the last 50 at **`GET /_dev/mail`**: subject, recipients (with blind copies listed apart, since the message itself doesn't carry them), the HTML body in a sandboxed frame with its inline images, an HTML / text toggle with clickable links in the text, download links for attachments, and the whole message as an `.eml` file. Sign-up confirmations and password-reset links work end to end on your machine with no mail server. The page only exists when the outbox is in use, and the [dev console](dev-console.md) links it.

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

A failed send is retried with backoff (5 s, 20 s, 80 s, ...). Attachments travel with the job (stored base64 in its payload).

## Testing

```rust
let mailer = Mailer::outbox();
let client = App::new().mailer(mailer.clone()).mount(invite).test();

client.post("/invite").send().await.assert_status(200);
let sent = mailer.sent();
assert_eq!(sent[0].to, ["ada@example.com"]);
assert!(sent[0].text.as_deref().unwrap().contains("/join"));
```

`Email` is a plain struct with public fields (`to`, `cc`, `bcc`, `subject`, `text`, `html`, `headers`, `attachments`), so assertions can look at anything. Generated emails come with such a test (`zapline g mailer`).
