//! A real zapline app behind the `zapline dev` proxy: the dev console
//! answers the developer's browser, which the proxy relays with an
//! `X-Forwarded-For` naming it. (Its own file: it sets the environment
//! `zapline dev` gives the app, which the app reads once.)
#![cfg(debug_assertions)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use zapline::prelude::*;
use zapline_cli::dev::proxy::{Hub, Proxy};

fn app(key: &str) -> SocketAddr {
    let dir =
        std::env::temp_dir().join(format!("zapline-dev-proxy-console-{}", std::process::id()));
    // Set before the app reads them (it does once).
    std::env::set_var("ZAPLINE_DEV", "1");
    std::env::set_var(zapline_cli::dev::ENV_DEV_PROXY, dir.join("handshake.json"));
    std::env::set_var(zapline_cli::dev::ENV_DEV_PROXY_KEY, key);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            App::new()
                .dev_console()
                .get("/hello", || async { "hi" })
                .serve_with_shutdown(listener, std::future::pending())
                .await
                .unwrap();
        });
    });
    addr
}

fn get(addr: SocketAddr, path: &str, extra: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nhost: localhost:{}\r\n{extra}connection: close\r\n\r\n",
        addr.port()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn the_dev_console_answers_through_the_proxy() {
    let hub = Hub::new();
    let upstream = app(hub.key());
    let proxy = Proxy::bind("127.0.0.1:0".parse().unwrap(), hub.clone()).unwrap();
    hub.set_upstream(Some(upstream), None);

    let res = get(proxy.addr(), "/hello", "");
    assert!(res.starts_with("HTTP/1.1 200"), "{res}");
    let res = get(proxy.addr(), "/_dev", "");
    assert!(
        res.starts_with("HTTP/1.1 200"),
        "the console answers: {res}"
    );

    // A client that says it relays for someone else is not this machine.
    let res = get(proxy.addr(), "/_dev", "x-forwarded-for: 203.0.113.7\r\n");
    assert!(res.starts_with("HTTP/1.1 404"), "{res}");
    // Straight to the app, a forged key with a loopback X-Forwarded-For is
    // still a forwarded request.
    let forged = "x-zapline-dev-proxy: 0123456789abcdef0123456789abcdef\r\n\
                  x-forwarded-for: 127.0.0.1\r\n";
    let res = get(upstream, "/_dev", forged);
    assert!(res.starts_with("HTTP/1.1 404"), "{res}");
    // With the right key, only a loopback client counts.
    let relayed = |ip: &str| {
        format!(
            "x-zapline-dev-proxy: {}\r\nx-forwarded-for: {ip}\r\n",
            hub.key()
        )
    };
    assert!(get(upstream, "/_dev", &relayed("127.0.0.1")).starts_with("HTTP/1.1 200"));
    assert!(get(upstream, "/_dev", &relayed("203.0.113.7")).starts_with("HTTP/1.1 404"));
}
