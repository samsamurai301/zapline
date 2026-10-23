//! The mailer's outbox, the dev inbox page, and email as a durable job.
#![cfg(feature = "mail")]

use zapline::mail::{Email, Mailer, DEV_INBOX_PATH};
use zapline::prelude::*;

async fn welcome(mail: Mailer) -> Result<&'static str> {
    mail.send(
        Email::to("ada@example.com")
            .subject("Welcome <3")
            .text("Confirm: https://app.test/confirm?t=abc")
            .html("<p>Hi <b>Ada</b></p>"),
    )
    .await?;
    Ok("sent")
}

#[tokio::test]
async fn outbox_keeps_mail_and_the_dev_inbox_shows_it() {
    let mailer = Mailer::outbox().default_from("App <app@example.com>");
    let client = App::new()
        .mailer(mailer.clone())
        .post("/welcome", welcome)
        .test();

    client
        .get(DEV_INBOX_PATH)
        .send()
        .await
        .assert_contains("Nothing sent yet");
    client.post("/welcome").send().await.assert_text("sent");

    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, ["ada@example.com"]);
    assert_eq!(sent[0].from.as_deref(), Some("App <app@example.com>"));

    let page = client.get(DEV_INBOX_PATH).send().await;
    page.assert_status(200)
        .assert_contains("Welcome &lt;3")
        .assert_contains("<a href=\"https://app.test/confirm?t=abc\">")
        .assert_contains("srcdoc=\"&lt;p&gt;Hi &lt;b&gt;Ada&lt;/b&gt;&lt;/p&gt;\"");
}

#[tokio::test]
async fn the_dev_inbox_is_for_this_machine_only() {
    let client = App::new().mailer(Mailer::outbox()).test();
    client.get(DEV_INBOX_PATH).send().await.assert_status(200);
    client
        .get(DEV_INBOX_PATH)
        .remote_addr("203.0.113.9:4000".parse().unwrap())
        .send()
        .await
        .assert_status(404);
    client
        .get(DEV_INBOX_PATH)
        .header("x-forwarded-for", "203.0.113.9")
        .send()
        .await
        .assert_status(404);
    // DNS rebinding: a page on evil.example resolved to 127.0.0.1.
    client
        .get(DEV_INBOX_PATH)
        .header("host", "evil.example:3000")
        .send()
        .await
        .assert_status(404);
    for host in [
        "localhost:3000",
        "127.0.0.1:3000",
        "[::1]:3000",
        "app.localhost",
    ] {
        client
            .get(DEV_INBOX_PATH)
            .header("host", host)
            .send()
            .await
            .assert_status(200);
    }
}

#[tokio::test]
async fn smtp_mailers_have_no_dev_inbox() {
    let mailer = Mailer::smtp("smtp://localhost:1").unwrap();
    assert!(!mailer.is_outbox());
    let client = App::new().mailer(mailer).test();
    client.get(DEV_INBOX_PATH).send().await.assert_status(404);
}

#[tokio::test]
async fn unreachable_smtp_is_a_503() {
    let mailer = Mailer::smtp("smtp://127.0.0.1:1").unwrap();
    let err = mailer
        .send(Email::to("ada@example.com").subject("x").text("y"))
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn handlers_without_a_mailer_get_a_clear_error() {
    let client = App::new().post("/welcome", welcome).test();
    client.post("/welcome").send().await.assert_status(500);
}

#[cfg(all(feature = "sqlite", feature = "json"))]
#[tokio::test]
async fn email_is_a_durable_job() {
    use zapline::db::sqlx::Sqlite;
    use zapline::db::Database;
    use zapline::jobs::Jobs;

    let mailer = Mailer::outbox();
    let client = App::new()
        .mailer(mailer.clone())
        .database(Database::sqlite_memory())
        .jobs_in::<Sqlite>(|q| q.register::<Email>().workers(0))
        .test();
    let jobs: Jobs<Sqlite> = client.resources().await.require().unwrap();
    jobs.enqueue(Email::to("ada@example.com").subject("Later").text("hi"))
        .await
        .unwrap();
    assert!(mailer.sent().is_empty());
    assert_eq!(jobs.run_pending().await.unwrap(), 1);
    assert_eq!(mailer.sent()[0].subject, "Later");
}

/// `app()` builds its mailer in `main`, before the runtime starts; that must
/// not need one (it once panicked in release builds).
#[test]
fn smtp_mailers_are_made_outside_the_runtime() {
    let mailer = Mailer::smtp("smtp://localhost:2525").unwrap();
    drop(App::new().mailer(mailer.clone()));
    drop(mailer);
}

fn mime(email: &Email) -> String {
    String::from_utf8(email.to_mime("App <app@example.com>").unwrap()).unwrap()
}

#[test]
fn cc_and_custom_headers_are_sent_but_bcc_is_not() {
    let email = Email::to("ada@example.com")
        .and_to("Grace <grace@example.com>")
        .cc("team@example.com")
        .cc("lead@example.com")
        .bcc("audit@example.com")
        .header("List-Unsubscribe", "<https://app.test/unsubscribe?u=1>")
        .header("X-Entity-Ref-ID", "42")
        .subject("Quarterly")
        .text("numbers");
    let raw = mime(&email);
    assert!(
        raw.contains("To: ada@example.com, \"Grace\" <grace@example.com>\r\n")
            || raw.contains("To: ada@example.com, Grace <grace@example.com>\r\n"),
        "{raw}"
    );
    assert!(
        raw.contains("Cc: team@example.com, lead@example.com\r\n"),
        "{raw}"
    );
    assert!(raw.contains("List-Unsubscribe: <https://app.test/unsubscribe?u=1>\r\n"));
    assert!(raw.contains("X-Entity-Ref-ID: 42\r\n"));
    assert!(!raw.contains("audit@example.com"), "bcc leaked: {raw}");
    assert!(!raw.to_ascii_lowercase().contains("bcc:"), "{raw}");

    // A blind copy alone is enough of a recipient.
    assert!(Email::to("x@example.com").to.len() == 1);
    let only_bcc = Email {
        bcc: vec!["audit@example.com".into()],
        ..Email::default()
    };
    assert!(only_bcc.to_mime("app@example.com").is_ok());
    assert!(Email::default().to_mime("app@example.com").is_err());
}

#[tokio::test]
async fn header_injection_is_refused() {
    let mailer = Mailer::outbox();
    for (name, value) in [
        ("X-Note", "hi\r\nBcc: eve@example.com"),
        ("X-Note", "hi\nBcc: eve@example.com"),
        ("X-Note\r\nBcc", "eve@example.com"),
        ("To", "eve@example.com"),
    ] {
        let email = Email::to("ada@example.com").header(name, value).text("x");
        let err = mailer.send(email).await.unwrap_err();
        assert!(err.message().contains("header"), "{}", err.message());
    }
    assert!(mailer.sent().is_empty());
}

#[test]
fn attachments_and_inline_images_are_mime_parts() {
    let pdf = [b"%PDF-1.7\n".as_slice(), &[0xe2, 0xe3, 0xcf, 0xd3, 0, 0xff]].concat();
    let png = vec![0x89, b'P', b'N', b'G', 0, 1, 2, 3];
    let email = Email::to("ada@example.com")
        .subject("Invoice")
        .text("Attached.")
        .html("<p><img src=\"cid:logo\"> Attached.</p>")
        .attach("invoice 42.pdf", "application/pdf", pdf.clone())
        .inline("logo", "image/png", png.clone());
    let raw = mime(&email);
    assert!(raw.contains("Content-Type: multipart/mixed"), "{raw}");
    assert!(raw.contains("Content-Type: multipart/related"), "{raw}");
    assert!(raw.contains("Content-Type: multipart/alternative"), "{raw}");
    assert!(raw.contains("Content-Type: application/pdf"));
    assert!(
        raw.contains("Content-Disposition: attachment; filename=\"invoice 42.pdf\""),
        "{raw}"
    );
    assert!(raw.contains("Content-ID: <logo>"), "{raw}");
    assert!(raw.contains("Content-Disposition: inline"), "{raw}");
    use base64::prelude::{Engine as _, BASE64_STANDARD};
    assert!(
        raw.contains(&BASE64_STANDARD.encode(&pdf)),
        "the PDF is base64 in the message"
    );
    assert!(raw.contains(&BASE64_STANDARD.encode(&png)));

    // Text with a file only: mixed { text, file }.
    let raw = mime(
        &Email::to("a@example.com")
            .text("hi")
            .attach("a.csv", "text/csv", "x,y"),
    );
    assert!(raw.contains("multipart/mixed") && !raw.contains("multipart/related"));

    // A bad content type is a clear error.
    let err = Email::to("a@example.com")
        .attach("a", "not a type", "x")
        .to_mime("b@example.com")
        .unwrap_err();
    assert!(
        err.message().contains("invalid content type"),
        "{}",
        err.message()
    );
}

#[tokio::test]
async fn oversized_attachments_fail_with_a_clear_error() {
    let mailer = Mailer::outbox();
    let big = vec![0u8; zapline::mail::MAX_ATTACHMENT_BYTES + 1];
    let err = mailer
        .send(Email::to("a@example.com").attach("big.bin", "application/octet-stream", big))
        .await
        .unwrap_err();
    assert!(
        err.message().contains("MAX_ATTACHMENT_BYTES"),
        "{}",
        err.message()
    );
    assert!(mailer.sent().is_empty());
}

#[cfg(feature = "views")]
mod templates {
    use super::*;
    use zapline::view::{context, Views};

    fn views() -> Views {
        Views::new("tests/fixtures/templates", true)
    }

    #[test]
    fn subject_html_and_text_come_from_the_templates() {
        let email = Email::to("ada@example.com")
            .template(
                &views(),
                "mail/welcome",
                context! { name => "Tom & Jerry", url => "https://app.test/c?t=1&u=2" },
            )
            .unwrap();
        // From `{% block subject %}` of the layout's <title>, unescaped.
        assert_eq!(email.subject, "Welcome, Tom & Jerry!");
        let html = email.html.as_deref().unwrap();
        assert!(html.contains("<h1>Hi Tom &amp; Jerry</h1>"), "{html}");
        assert!(html.contains("-- The Acme team"), "{html}");
        // The .txt is used as is (no HTML escaping).
        assert_eq!(
            email.text.as_deref(),
            Some("Hi Tom & Jerry,\n\nConfirm your address: https://app.test/c?t=1&u=2\n")
        );
    }

    #[test]
    fn without_a_txt_the_text_is_made_from_the_html() {
        let email = Email::to("ada@example.com")
            .subject("ignored")
            .template(
                &views(),
                "mail/receipt",
                context! { id => 7, total => "$12", item => "Pen" },
            )
            .unwrap();
        assert_eq!(email.subject, "Your receipt #7");
        assert_eq!(email.text.as_deref(), Some("Total: $12\n\n- Pen\n"));
    }

    #[test]
    fn a_missing_template_is_an_error() {
        let err = Email::to("ada@example.com")
            .template(&views(), "mail/nope", context! {})
            .unwrap_err();
        assert!(
            err.message().contains("mail/nope.html"),
            "{}",
            err.message()
        );
    }

    #[tokio::test]
    async fn handlers_send_templated_mail() {
        async fn signup(views: Views, mail: Mailer) -> Result<&'static str> {
            let ctx = context! { name => "Ada", url => "https://app.test/confirm" };
            mail.send(Email::to("ada@example.com").template(&views, "mail/welcome", ctx)?)
                .await?;
            Ok("ok")
        }
        let mailer = Mailer::outbox();
        let client = App::new()
            .views("tests/fixtures/templates")
            .mailer(mailer.clone())
            .post("/signup", signup)
            .test();
        client.post("/signup").send().await.assert_text("ok");
        assert_eq!(mailer.sent()[0].subject, "Welcome, Ada!");
    }
}

#[tokio::test]
async fn the_dev_inbox_lists_attachments_and_toggles_html_and_text() {
    let mailer = Mailer::outbox();
    let client = App::new().mailer(mailer.clone()).test();
    mailer
        .send(
            Email::to("ada@example.com")
                .cc("team@example.com")
                .bcc("audit@example.com")
                .subject("Report")
                .text("See attached.")
                .html("<p><img src=\"cid:chart\">See attached.</p>")
                .attach("q3 report.csv", "text/csv", "q,total\n3,42\n")
                .inline("chart", "image/png", vec![0x89, b'P', b'N', b'G']),
        )
        .await
        .unwrap();

    let page = client.get(DEV_INBOX_PATH).send().await;
    page.assert_status(200)
        .assert_contains("to ada@example.com · cc team@example.com")
        .assert_contains("bcc, not in the sent headers: audit@example.com")
        .assert_contains("<label for=v1h>HTML</label>")
        .assert_contains("<label for=v1t>Text</label>")
        .assert_contains("<a href=\"/_dev/mail/1/0\" download=\"q3 report.csv\">q3 report.csv</a>")
        .assert_contains("text/csv · 13 B")
        // The inline image is shown in the HTML preview.
        .assert_contains("src=&quot;/_dev/mail/1/1&quot;");
    assert!(!page.text().contains("to ada@example.com, audit"));

    let file = client.get("/_dev/mail/1/0").send().await;
    file.assert_status(200)
        .assert_header("content-type", "text/csv")
        .assert_header(
            "content-disposition",
            "attachment; filename=\"q3_report.csv\"",
        )
        .assert_header("x-content-type-options", "nosniff");
    assert_eq!(file.text(), "q,total\n3,42\n");
    client
        .get("/_dev/mail/1/1")
        .send()
        .await
        .assert_header("content-disposition", "inline; filename=\"chart\"");

    let eml = client.get("/_dev/mail/1/raw").send().await;
    eml.assert_status(200)
        .assert_header("content-type", "message/rfc822");
    assert!(eml.text().contains("Subject: Report"));
    assert!(!eml.text().contains("audit@example.com"));

    for missing in [
        "/_dev/mail/1/9",
        "/_dev/mail/2/0",
        "/_dev/mail/x/0",
        "/_dev/mail/1/raw2",
    ] {
        client.get(missing).send().await.assert_status(404);
    }
    client
        .get("/_dev/mail/1/0")
        .remote_addr("203.0.113.9:4000".parse().unwrap())
        .send()
        .await
        .assert_status(404);
}

#[cfg(all(feature = "sqlite", feature = "json"))]
#[tokio::test]
async fn email_jobs_carry_their_attachments() {
    use zapline::db::sqlx::Sqlite;
    use zapline::db::Database;
    use zapline::jobs::Jobs;

    let mailer = Mailer::outbox();
    let client = App::new()
        .mailer(mailer.clone())
        .database(Database::sqlite_memory())
        .jobs_in::<Sqlite>(|q| q.register::<Email>().workers(0))
        .test();
    let jobs: Jobs<Sqlite> = client.resources().await.require().unwrap();
    let pdf: Vec<u8> = (0..=255).collect();
    let email = Email::to("ada@example.com")
        .cc("team@example.com")
        .bcc("audit@example.com")
        .header("X-Campaign", "spring")
        .subject("Your invoice")
        .text("Attached.")
        .attach("invoice.pdf", "application/pdf", pdf.clone());
    jobs.enqueue(email.clone()).await.unwrap();
    assert_eq!(jobs.run_pending().await.unwrap(), 1);
    let sent = mailer.sent();
    let mut expected = email;
    expected.from = Some("no-reply@localhost".into());
    if let Ok(from) = std::env::var("MAIL_FROM") {
        expected.from = Some(from);
    }
    assert_eq!(
        sent[0], expected,
        "the email survives the round trip intact"
    );
    assert_eq!(sent[0].attachments[0].data.as_ref(), pdf.as_slice());

    // Too large to send is too large to enqueue.
    let big = Email::to("a@example.com").attach(
        "big.bin",
        "application/octet-stream",
        vec![0u8; zapline::mail::MAX_ATTACHMENT_BYTES + 1],
    );
    let err = jobs.enqueue(big).await.unwrap_err();
    assert!(
        err.message().contains("MAX_ATTACHMENT_BYTES"),
        "{}",
        err.message()
    );
}

#[cfg(feature = "json")]
#[test]
fn emails_without_the_new_fields_still_deserialize() {
    // A job payload enqueued by an older version.
    let old = r#"{"to":["a@example.com"],"from":null,"reply_to":null,"subject":"s","text":"t","html":null}"#;
    let email: Email = serde_json::from_str(old).unwrap();
    assert!(email.cc.is_empty() && email.attachments.is_empty());
    let json =
        serde_json::to_string(&Email::to("a@example.com").attach("a.txt", "text/plain", "hi"))
            .unwrap();
    assert!(json.contains(r#""data":"aGk=""#), "{json}");
}
