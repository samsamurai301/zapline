//! A WebSocket chat room: every message is broadcast to all connected
//! clients through a `tokio::sync::broadcast` channel kept in app state.
//!
//!     cargo run -p zapline-examples --example realtime_chat
//!     open http://localhost:8080 in two browser tabs

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::{self, error::RecvError};
use zapline::prelude::*;
use zapline::ws::{Message, WebSocket, WebSocketUpgrade};

#[derive(Clone)]
struct Room(broadcast::Sender<String>);

async fn index() -> Html<&'static str> {
    Html(PAGE)
}

async fn chat(ws: WebSocketUpgrade, State(room): State<Room>) -> Response {
    ws.protocols(["chat"])
        .max_message_size(4 * 1024)
        .on_upgrade(move |socket| session(socket, room))
}

async fn session(socket: WebSocket, room: Room) {
    let (mut sink, mut stream) = socket.split();

    // Room -> this client.
    let mut rx = room.0.subscribe();
    let mut outgoing = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(text) => {
                    if sink.send(Message::text(text)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => continue, // slow client: skip ahead
                Err(RecvError::Closed) => break,
            }
        }
    });

    // This client -> room.
    let tx = room.0.clone();
    let mut incoming = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            if let Some(text) = msg.as_text() {
                let _ = tx.send(text.trim().to_owned());
            }
        }
    });

    // When either side finishes, stop the other.
    tokio::select! {
        _ = &mut outgoing => incoming.abort(),
        _ = &mut incoming => outgoing.abort(),
    }
}

fn main() -> std::io::Result<()> {
    let (tx, _) = broadcast::channel(256);
    App::new()
        .state(Room(tx))
        .get("/", index)
        .get("/ws", chat)
        .run("0.0.0.0:8080")
}

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>zapline chat</title>
<ul id="log"></ul>
<form id="f"><input id="msg" autocomplete="off" autofocus><button>Send</button></form>
<script>
  const ws = new WebSocket(`ws://${location.host}/ws`, ["chat"]);
  ws.onmessage = (e) => {
    const li = document.createElement("li");
    li.textContent = e.data;
    document.getElementById("log").append(li);
  };
  document.getElementById("f").onsubmit = (e) => {
    e.preventDefault();
    const input = document.getElementById("msg");
    if (input.value) ws.send(input.value);
    input.value = "";
  };
</script>
"#;
