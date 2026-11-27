//! Live updates: `Live` pub/sub, over SSE to a real socket.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapline::live::{Live, Update};
use zapline::prelude::*;

#[tokio::test]
async fn subscribers_get_what_is_published_on_their_topic() {
    let live = Live::new();
    let mut orders = live.subscribe("orders");
    let mut also = live.subscribe("orders");
    let mut users = live.subscribe("users");

    assert_eq!(live.publish("orders", "created", "7"), 2);
    let expected = Update {
        topic: "orders".into(),
        event: "created".into(),
        data: "7".into(),
    };
    assert_eq!(orders.next().await.unwrap(), expected);
    assert_eq!(also.next().await.unwrap(), expected);

    live.publish("users", "joined", "ada");
    assert_eq!(users.next().await.unwrap().data, "ada");
    assert_eq!(live.publish("nobody", "x", "y"), 0);
}

#[tokio::test]
async fn topics_go_away_with_their_last_subscriber() {
    let live = Live::new();
    let sub = live.subscribe("t");
    assert_eq!(live.publish("t", "e", "1"), 1);
    drop(sub);
    assert_eq!(live.publish("t", "e", "2"), 0);
    // A new subscriber starts fresh.
    let mut sub = live.subscribe("t");
    live.publish("t", "e", "3");
    assert_eq!(sub.next().await.unwrap().data, "3");
}

#[tokio::test]
async fn slow_subscribers_skip_rather_than_block() {
    let live = Live::new().capacity(2);
    let mut slow = live.subscribe("t");
    for n in 0..5 {
        live.publish("t", "e", n.to_string());
    }
    // Only the last two were kept for it.
    assert_eq!(slow.next().await.unwrap().data, "3");
    assert_eq!(slow.next().await.unwrap().data, "4");
}

#[cfg(feature = "json")]
#[tokio::test]
async fn publish_json() {
    let live = Live::new();
    let mut sub = live.subscribe("t");
    live.publish_json("t", "order", &serde_json::json!({ "id": 7 }))
        .unwrap();
    assert_eq!(sub.next().await.unwrap().data, r#"{"id":7}"#);
}

#[get("/live/{room}")]
async fn listen(live: Live, Path(room): Path<String>) -> Response {
    live.sse(&room).into_response()
}

#[post("/say/{room}")]
async fn say(live: Live, Path(room): Path<String>, body: String) -> String {
    live.publish(&room, "message", body).to_string()
}

#[tokio::test]
async fn browsers_receive_updates_over_sse() {
    let live = Live::new();
    let app = App::new().live(live.clone()).mount((listen, say));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));

    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /live/lobby HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    // Wait for the headers, so the subscription exists before publishing.
    while !String::from_utf8_lossy(&out).contains("\r\n\r\n") {
        let n = s.read(&mut buf).await.unwrap();
        out.extend_from_slice(&buf[..n]);
    }
    for _ in 0..100 {
        if live.publish("lobby", "message", "<p>hi\nthere</p>") > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while !String::from_utf8_lossy(&out).contains("data: there</p>\n\n") {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0);
            out.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .expect("no update arrived");
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("content-type: text/event-stream"), "{out}");
    assert!(
        out.contains("event: message\ndata: <p>hi\ndata: there</p>\n\n"),
        "{out}"
    );

    // The dev console (debug builds) lists the topic.
    #[cfg(debug_assertions)]
    {
        let client = App::new().live(live.clone()).dev_console().test();
        client
            .get("/_dev")
            .send()
            .await
            .assert_contains("<h2>Live topics</h2>")
            .assert_contains("<td><code>lobby</code><td>1</tr>");
    }
}

#[tokio::test]
async fn without_a_hub_the_extractor_explains() {
    let client = App::new().mount(say).test();
    let res = client.post("/say/x").body("hi").send().await;
    res.assert_status(500);
}
