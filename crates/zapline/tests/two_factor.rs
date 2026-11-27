//! Two-factor and magic-link sign-in through a real app: TOTP enrollment
//! and login (with replay refused), recovery codes, and single-use magic
//! links bound to the user's last sign-in.
#![cfg(all(feature = "two-factor", feature = "cookies", feature = "json"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use zapline::auth::magic_link::MagicLinks;
use zapline::auth::recovery_codes;
use zapline::auth::totp::Totp;
use zapline::cookies::Key;
use zapline::prelude::*;

/// One user's stored credentials (what a `users` table would hold).
#[derive(Default)]
struct Account {
    totp_secret: Option<String>,
    last_step: Option<u64>,
    recovery_hashes: Vec<String>,
    last_login: u64,
}

type Store = Arc<Mutex<Account>>;

fn app(store: Store) -> App {
    App::new()
        .cookie_key(Key::generate())
        .state(store)
        // Enrollment: a new secret, the QR URI, and recovery codes (shown once).
        .post("/2fa/enroll", |State(store): State<Store>| async move {
            let totp = Totp::generate();
            let codes = recovery_codes::generate(8);
            let mut acc = store.lock().unwrap();
            acc.totp_secret = Some(totp.secret_base32());
            acc.recovery_hashes = codes.iter().map(|c| recovery_codes::hash(c)).collect();
            Json(json!({
                "uri": totp.otpauth_uri("Shop", "ada@example.com"),
                "secret": totp.secret_base32(),
                "recovery_codes": codes,
            }))
        })
        // The second step of login: a TOTP code or a recovery code.
        .post(
            "/2fa/verify",
            |State(store): State<Store>, Json(body): Json<Value>| async move {
                let code = body["code"].as_str().unwrap_or("");
                let mut acc = store.lock().unwrap();
                let Some(secret) = acc.totp_secret.clone() else {
                    return Err(Error::bad_request("two-factor login is off"));
                };
                let totp = Totp::from_base32(&secret)?;
                if let Some(step) = totp.verify(code, acc.last_step) {
                    acc.last_step = Some(step);
                    return Ok("signed in with a code");
                }
                if let Some(i) = recovery_codes::find(code, &acc.recovery_hashes) {
                    acc.recovery_hashes.remove(i);
                    return Ok("signed in with a recovery code");
                }
                Err(Error::unauthorized().with_code("code_invalid"))
            },
        )
        .post(
            "/magic/send",
            |State(store): State<Store>, links: MagicLinks| async move {
                let last = store.lock().unwrap().last_login;
                links.issue(
                    "ada",
                    "sign-in",
                    &last.to_string(),
                    Duration::from_secs(900),
                )
            },
        )
        .get(
            "/magic/{token}",
            |State(store): State<Store>, links: MagicLinks, Path(token): Path<String>| async move {
                let user = links.verify(&token, "sign-in", |user| {
                    (user == "ada").then(|| store.lock().unwrap().last_login.to_string())
                })?;
                // Signing in changes the binding: this link is now used up.
                store.lock().unwrap().last_login += 1;
                Ok::<_, Error>(format!("welcome back, {user}"))
            },
        )
}

#[tokio::test]
async fn totp_enrollment_login_and_replay() {
    let store = Store::default();
    let c = app(store.clone()).test();
    let enrolled: Value = c.post("/2fa/enroll").send().await.json();
    assert!(enrolled["uri"]
        .as_str()
        .unwrap()
        .starts_with("otpauth://totp/Shop:ada%40example.com?secret="));
    let totp = Totp::from_base32(enrolled["secret"].as_str().unwrap()).unwrap();

    let code = totp.code_now();
    let ok = c
        .post("/2fa/verify")
        .json(&json!({"code": code}))
        .send()
        .await;
    assert_eq!(ok.text(), "signed in with a code");
    // The same code a second time (say, intercepted) is refused.
    let again = c
        .post("/2fa/verify")
        .json(&json!({"code": code}))
        .send()
        .await;
    assert_eq!(again.status(), 401);

    let wrong = c
        .post("/2fa/verify")
        .json(&json!({"code": "000000"}))
        .send()
        .await;
    assert_eq!(wrong.status(), 401);
}

#[tokio::test]
async fn recovery_codes_work_once() {
    let c = app(Store::default()).test();
    let enrolled: Value = c.post("/2fa/enroll").send().await.json();
    let first = enrolled["recovery_codes"][0]
        .as_str()
        .unwrap()
        .to_uppercase();
    let r = c
        .post("/2fa/verify")
        .json(&json!({"code": first}))
        .send()
        .await;
    assert_eq!(r.text(), "signed in with a recovery code");
    let r = c
        .post("/2fa/verify")
        .json(&json!({"code": first}))
        .send()
        .await;
    assert_eq!(r.status(), 401, "used up");
}

#[tokio::test]
async fn magic_links_are_single_use() {
    let c = app(Store::default()).test();
    let token = c.post("/magic/send").send().await.text();
    let r = c.get(&format!("/magic/{token}")).send().await;
    assert_eq!(r.text(), "welcome back, ada");
    let r = c.get(&format!("/magic/{token}")).send().await;
    assert_eq!(r.status(), 400);
    assert!(r.text().contains("link_invalid"), "{}", r.text());
    let r = c.get("/magic/not-a-token").send().await;
    assert_eq!(r.status(), 400);
}
