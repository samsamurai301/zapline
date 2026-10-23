//! What `zapline dev` relies on in the app: the proxy handshake of
//! `App::run`, and translations read again when they change. Everything
//! here only happens with `ZAPLINE_DEV=1`, which this test binary sets.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use zapline::prelude::*;

/// Sets the variables `zapline dev` sets, before anything reads them. The
/// handshake file is shared by the one test that runs a server.
fn dev_env() -> &'static Path {
    static FILE: OnceLock<PathBuf> = OnceLock::new();
    FILE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("zapline-dev-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("handshake.json");
        std::env::set_var("ZAPLINE_DEV", "1");
        std::env::set_var(zapline::dev::ENV_DEV_PROXY, &file);
        file
    })
}

fn get(addr: SocketAddr, path: &str) -> String {
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        conn,
        "GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    conn.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn run_listens_on_loopback_behind_the_dev_proxy_and_says_where() {
    let file = dev_env();
    // The address the app asks for: a port that is free right now.
    let asked: SocketAddr = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = std::thread::spawn(move || {
        let mut app = App::new().get("/", || async { "behind the proxy" });
        app.server_config_mut().banner = false;
        app.run_with_shutdown(asked, async {
            let _ = stop_rx.await;
        })
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    let text = loop {
        if let Ok(text) = std::fs::read_to_string(file) {
            break text;
        }
        assert!(Instant::now() < deadline, "no handshake written");
        std::thread::sleep(Duration::from_millis(20));
    };
    let hs: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(hs["addr"], asked.to_string(), "{hs}");
    assert_eq!(hs["pid"], std::process::id(), "{hs}");
    let internal: SocketAddr = hs["internal"].as_str().unwrap().parse().unwrap();
    assert!(internal.ip().is_loopback(), "{internal}");
    assert_ne!(internal.port(), asked.port());
    // No temporary file is left behind.
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    assert!(!Path::new(&tmp).exists());

    // The app answers on its internal port, and leaves its own address to
    // the proxy.
    assert!(get(internal, "/").ends_with("behind the proxy"));
    assert!(TcpListener::bind(asked).is_ok(), "the app bound {asked}");

    stop_tx.send(()).unwrap();
    server.join().unwrap().unwrap();
}

#[cfg(feature = "i18n")]
#[test]
fn translations_are_read_again_when_a_file_changes() {
    use zapline::i18n::{I18n, Locale};

    dev_env();
    let dir = tempfile::tempdir().unwrap();
    let en = dir.path().join("en.toml");
    std::fs::write(&en, r#"hello = "Hello""#).unwrap();
    std::fs::write(dir.path().join("fr.toml"), r#"hello = "Bonjour""#).unwrap();
    let i18n = I18n::load(dir.path()).unwrap().default_lang("fr");
    let client = App::new()
        .i18n(i18n)
        .get("/", |t: Locale| async move {
            format!("{} {}", t.lang(), t.t("hello"))
        })
        .test();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let hello = |lang: &'static str| {
            let client = &client;
            async move {
                client
                    .get("/")
                    .header("accept-language", lang)
                    .send()
                    .await
                    .text()
            }
        };
        assert_eq!(hello("en").await, "en Hello");

        // A different size, so the change shows even on coarse mtimes.
        std::fs::write(&en, "hello = \"Hello again\"\nbye = \"Bye\"\n").unwrap();
        assert_eq!(hello("en").await, "en Hello again");
        // The default language set in code survives the reload.
        assert_eq!(hello("ja").await, "fr Bonjour");

        // A broken file keeps the last good messages.
        std::fs::write(&en, "hello = [not text").unwrap();
        assert_eq!(hello("en").await, "en Hello again");
        std::fs::write(&en, r#"hello = "Fixed""#).unwrap();
        assert_eq!(hello("en").await, "en Fixed");

        // A new language file is picked up too.
        std::fs::write(dir.path().join("de.toml"), r#"hello = "Hallo""#).unwrap();
        assert_eq!(hello("de").await, "de Hallo");
    });
}
