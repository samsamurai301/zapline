//! Sessions stored in the database survive an app restart.
#![cfg(all(feature = "sqlite", feature = "cookies"))]

use zapline::cookies::key_from_secret;
use zapline::db::sqlx::Sqlite;
use zapline::db::{Database, SqlSessionStore};
use zapline::prelude::*;
use zapline::session::Session;

const SECRET: &str = "a test secret that is at least 32 bytes long";

async fn login(session: Session) -> Result<&'static str> {
    session.regenerate();
    session.insert("user_id", 7)?;
    Ok("ok")
}

async fn me(session: Session) -> Result<String> {
    let id: i64 = session.get("user_id").ok_or_else(Error::unauthorized)?;
    Ok(id.to_string())
}

async fn logout(session: Session) -> &'static str {
    session.destroy();
    "bye"
}

fn app(url: &str) -> App {
    App::new()
        .database(Database::sqlite(url).migrate(false))
        .cookie_key(key_from_secret(SECRET).unwrap())
        .database_sessions_in::<Sqlite>(|layer| layer.secure(false))
        .post("/login", login)
        .get("/me", me)
        .post("/logout", logout)
}

#[tokio::test]
async fn sessions_survive_a_restart_and_logout_deletes_them() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("app.db").display());

    let first = app(&url).test().with_cookies();
    first.post("/login").send().await.assert_status(200);
    let sid = first.cookie("zapline.sid").expect("session cookie");
    first
        .get("/me")
        .send()
        .await
        .assert_status(200)
        .assert_text("7");

    // A new app on the same database (a restart, or another instance).
    let second = app(&url).test();
    let cookie = format!("zapline.sid={sid}");
    let res = second
        .get("/me")
        .header("cookie", cookie.as_str())
        .send()
        .await;
    res.assert_status(200).assert_text("7");

    second
        .post("/logout")
        .header("cookie", cookie.as_str())
        .send()
        .await
        .assert_status(200);
    second
        .get("/me")
        .header("cookie", cookie.as_str())
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
#[should_panic(expected = "call `.database(..)` before")]
async fn sessions_need_a_database_first() {
    let app = App::new()
        .cookie_key(key_from_secret(SECRET).unwrap())
        .database_sessions_in::<Sqlite>(|l| l)
        .database(Database::sqlite_memory());
    app.test().get("/").send().await;
}

#[test]
fn short_secrets_are_rejected() {
    assert!(key_from_secret("too short").is_err());
    let _: fn(_) -> _ = SqlSessionStore::<Sqlite>::new;
}
