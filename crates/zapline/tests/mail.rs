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
