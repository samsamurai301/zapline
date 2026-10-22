//! The development console at `/_dev`.
#![cfg(debug_assertions)]

use std::net::SocketAddr;

use zapline::prelude::*;

/// List the users.
#[get("/users", tag = "users")]
async fn users() -> &'static str {
    "[]"
}

#[get("/users/{id}")]
async fn user(Path(id): Path<u32>) -> String {
    id.to_string()
}

#[tokio::test]
async fn lists_routes_and_recent_requests() {
    let client = App::new().mount((users, user)).dev_console().test();
    client.get("/users").send().await.assert_status(200);
    client.get("/missing").send().await.assert_status(404);

    let page = client.get("/_dev").send().await;
    page.assert_status(200)
        .assert_header("content-type", "text/html; charset=utf-8")
        .assert_contains("<h1>Dev console</h1>")
        // Parameterless GET routes are links, the others are not.
        .assert_contains(r#"<a href="/users"><code>/users</code></a>"#)
        .assert_contains("<code>/users/{id}</code>")
        // The latest request comes first.
        .assert_contains("<td class=s4>404<td>GET<td><code>/missing</code>");
    #[cfg(feature = "openapi")]
    page.assert_contains("List the users. <small>users</small>");
    assert!(
        !page.text().contains("<code>/_dev</code></td>"),
        "the console's own requests are not recorded"
    );
}

#[tokio::test]
async fn only_answers_this_machine() {
    let client = App::new().dev_console().test();
    let remote = SocketAddr::from(([203, 0, 113, 7], 4000));
    client
        .get("/_dev")
        .remote_addr(remote)
        .send()
        .await
        .assert_status(404);
    client
        .post("/_dev/anything")
        .remote_addr(remote)
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn unknown_panel_actions_are_404() {
    let client = App::new().dev_console().test();
    client.post("/_dev/nope").send().await.assert_status(404);
}

#[tokio::test]
async fn not_mounted_unless_asked_for() {
    let client = App::new().mount(users).test();
    client.get("/_dev").send().await.assert_status(404);
}

#[cfg(all(feature = "sqlite", feature = "json"))]
mod jobs {
    use serde::{Deserialize, Serialize};
    use zapline::db::sqlx::Sqlite;
    use zapline::db::Database;
    use zapline::jobs::{Job, Jobs};
    use zapline::prelude::*;

    #[derive(Serialize, Deserialize)]
    struct Broken;

    impl Job for Broken {
        const NAME: &'static str = "broken";
        const MAX_ATTEMPTS: u32 = 1;

        async fn run(self, _res: Resources) -> Result<()> {
            Err(Error::bad_request("smtp <down>"))
        }
    }

    #[tokio::test]
    async fn shows_failed_jobs_and_retries_them() {
        let client = App::new()
            .database(Database::sqlite_memory())
            .jobs_in::<Sqlite>(|q| q.register::<Broken>().workers(0))
            // Panels register whichever order the calls come in.
            .dev_console()
            .test();
        let jobs: Jobs<Sqlite> = client.resources().await.require().unwrap();
        jobs.enqueue(Broken).await.unwrap();
        jobs.run_pending().await.unwrap();

        let page = client.get("/_dev").send().await;
        page.assert_status(200)
            .assert_contains("<h2>Jobs (SQLite)</h2>")
            .assert_contains("0 queued · 0 running · 1 failed")
            .assert_contains("registered: broken")
            .assert_contains("<code>broken</code>")
            .assert_contains("smtp &lt;down&gt;")
            .assert_contains(r#"<form method=post action="/_dev/jobs-sqlite">"#);

        client
            .post("/_dev/jobs-sqlite")
            .send()
            .await
            .assert_status(303)
            .assert_header("location", "/_dev");
        let stats = jobs.stats().await.unwrap();
        assert_eq!((stats.queued, stats.failed), (1, 0));
    }
}

#[cfg(feature = "mail")]
#[tokio::test]
async fn links_the_dev_inbox() {
    use zapline::mail::{Email, Mailer};

    let mailer = Mailer::outbox();
    let client = App::new().dev_console().mailer(mailer.clone()).test();
    mailer
        .send(Email::to("ada@example.com").subject("hi").text("hello"))
        .await
        .unwrap();
    client
        .get("/_dev")
        .send()
        .await
        .assert_contains("<h2>Mail</h2>")
        .assert_contains(r#"1 email(s) kept in the <a href="/_dev/mail">dev inbox</a>."#);
}
