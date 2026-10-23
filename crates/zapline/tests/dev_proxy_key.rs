//! Requests relayed by `zapline dev`'s proxy count as this machine's when
//! they carry its key and name a loopback client, and only then. (Its own
//! file: it sets the environment `zapline dev` gives the app, read once.)
#![cfg(debug_assertions)]

use std::net::SocketAddr;

use zapline::prelude::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn the_dev_proxy_key_vouches_for_loopback_clients_only() {
    std::env::set_var("ZAPLINE_DEV", "1");
    std::env::set_var(zapline::dev::ENV_DEV_PROXY, "/nonexistent/handshake.json");
    std::env::set_var(zapline::dev::ENV_DEV_PROXY_KEY, KEY);
    let client = App::new().dev_console().test();
    let get = |key: &'static str, xff: &'static str| {
        client
            .get("/_dev")
            .header("x-zapline-dev-proxy", key)
            .header("x-forwarded-for", xff)
    };

    get(KEY, "127.0.0.1").send().await.assert_status(200);
    get(KEY, "::1").send().await.assert_status(200);
    // A client elsewhere, or one relaying for someone else.
    get(KEY, "203.0.113.7").send().await.assert_status(404);
    get(KEY, "203.0.113.7, 127.0.0.1")
        .send()
        .await
        .assert_status(404);
    get(KEY, "127.0.0.1, 127.0.0.1")
        .send()
        .await
        .assert_status(404);
    // Not the proxy.
    get("not-the-key", "127.0.0.1")
        .send()
        .await
        .assert_status(404);
    client
        .get("/_dev")
        .header("x-forwarded-for", "127.0.0.1")
        .send()
        .await
        .assert_status(404);
    // Other forwarding headers are still another proxy's.
    get(KEY, "127.0.0.1")
        .header("x-real-ip", "203.0.113.7")
        .send()
        .await
        .assert_status(404);
    // The proxy's own connection must come from loopback.
    get(KEY, "127.0.0.1")
        .remote_addr(SocketAddr::from(([203, 0, 113, 7], 4000)))
        .send()
        .await
        .assert_status(404);
    // And the page must have been asked for by a loopback name.
    get(KEY, "127.0.0.1")
        .header("host", "evil.example")
        .send()
        .await
        .assert_status(404);
}
