# WebSockets

Take a `WebSocketUpgrade` in a `GET` handler and give it an async callback:

```rust
use zapline::prelude::*;
use zapline::ws::{WebSocket, WebSocketUpgrade};

async fn echo(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if (msg.is_text() || msg.is_binary()) && socket.send(msg).await.is_err() {
                break;
            }
        }
    })
}

fn app() -> App {
    App::new().get("/ws", echo)
}
```

Needs the Cargo feature `ws` (it pulls in `tokio-tungstenite`). The handler returns `101 Switching Protocols`; the callback then runs in its own task with the connected `WebSocket`, long after the handler has returned. The callback may return `()` or `Result<(), E: Display>`; an `Err` is logged.

## The handshake: `WebSocketUpgrade`

As an extractor, `WebSocketUpgrade` validates the handshake and rejects bad requests before your handler runs:

| Problem | Status | Code |
|---|---|---|
| Not a `GET` request | `400` | `ws_invalid_method` |
| Missing `Connection: upgrade` or `Upgrade: websocket` | `426` with `Upgrade: websocket` | `ws_upgrade_required` |
| `Sec-WebSocket-Version` is not `13` | `426` with `Sec-WebSocket-Version: 13` | `ws_unsupported_version` |
| Missing or malformed `Sec-WebSocket-Key` | `400` | `ws_invalid_key` |

Because it is an ordinary extractor, it composes with the others: take `State<T>`, `Path<T>`, `Query<T>`, a session or `CurrentUser<U>` first, and reject unauthorized clients with a plain `Error` before the upgrade happens.

### Subprotocols

`.protocols([..])` lists the subprotocols you support in order of preference. The first one the client also offered is selected and echoed back; if none match, no protocol is selected and the client decides whether to proceed. Inside the callback, `socket.protocol()` returns the negotiated name. `requested_protocols()` and `selected_protocol()` are available on the upgrade, too.

```rust
use zapline::prelude::*;
use zapline::ws::WebSocketUpgrade;

async fn chat(ws: WebSocketUpgrade) -> Response {
    ws.protocols(["chat.v2", "chat.v1"])
        .max_message_size(64 * 1024)
        .on_upgrade(|socket| async move {
            let _version = socket.protocol(); // Some("chat.v2"), Some("chat.v1") or None
        })
}
```

### Limits and buffers

An incoming message larger than the limit closes the connection with an error. **Lower the defaults** to what your protocol needs: the defaults suit file transfer, not chat.

| Method | Default | Effect |
|---|---|---|
| `.max_message_size(bytes)` | 16 MiB (`DEFAULT_MAX_MESSAGE_SIZE`) | Largest incoming message, after fragments are reassembled |
| `.max_frame_size(bytes)` | 16 MiB (`DEFAULT_MAX_FRAME_SIZE`) | Largest single incoming frame |
| `.read_buffer_size(bytes)` | 128 KiB | Per-connection read buffer. Lower it for many mostly idle connections. |
| `.write_buffer_size(bytes)` | 128 KiB | Outgoing data is buffered until this many bytes are queued. `0` writes eagerly. Every `send` flushes regardless. |
| `.max_write_buffer_size(bytes)` | unlimited | Cap on the outgoing buffer when writes are failing. Kept above the write buffer size. |

## The `WebSocket`

| Method | Effect |
|---|---|
| `recv().await` | The next `Message`. `None` once the connection is closed (after the close handshake, or if the peer went away). |
| `send(msg).await` | Sends and flushes. Accepts a `Message`, `String`, `&str`, `Bytes`, `Vec<u8>` or `&[u8]`. |
| `close().await` | Starts the close handshake with code 1000. Keep calling `recv` until it returns `None`, or just drop the socket. |
| `close_with(code, reason).await` | Closes with a code from `zapline::ws::close_code` and a reason (at most 123 bytes are sent) |
| `protocol()` | The negotiated subprotocol |
| `send_json(&value).await` | Serializes with `serde_json` and sends a text message (needs the `json` feature) |
| `recv_json::<T>().await` | The next text or binary message parsed as JSON. Skips pings and pongs. `None` on close. |

`WebSocket` implements `Stream` and `Sink`, so `futures_util::StreamExt::split()` gives independent reader and writer halves. Pings are answered with pongs automatically **while you are reading**, and they are still delivered to you as `Message::Ping`. Read errors are `zapline::Error`s with stable codes: `ws_closed` (410), `ws_message_too_large` (413), `ws_protocol_error` (400), `ws_io_error` (500), `ws_invalid_json` (422, from `parse_json`/`recv_json`).

### Messages

`zapline::ws::Message` is zapline's own type, so your code doesn't change when `tungstenite` does:

| Variant | Constructor | Helpers |
|---|---|---|
| `Text(String)` | `Message::text(s)` | `is_text()`, `as_text()`, `into_text()` |
| `Binary(Bytes)` | `Message::binary(b)` | `is_binary()` |
| `Ping(Bytes)` / `Pong(Bytes)` | `Message::ping(b)` / `Message::pong(b)` (payload at most 125 bytes) | `is_ping()`, `is_pong()` |
| `Close(Option<CloseFrame>)` | `Message::close(code, reason)` | `is_close()` |
| any | `Message::json(&value)?` (text) | `parse_json::<T>()`, `as_bytes()`, `into_bytes()` |

Typed JSON messages are one line each way:

```rust
use serde::{Deserialize, Serialize};
use zapline::prelude::*;
use zapline::ws::{WebSocket, WebSocketUpgrade};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Incoming {
    Subscribe { topic: String },
    Ping,
}

#[derive(Serialize)]
struct Reply {
    ok: bool,
}

async fn api(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv_json::<Incoming>().await {
            let ok = matches!(msg, Incoming::Subscribe { .. } | Incoming::Ping);
            if socket.send_json(&Reply { ok }).await.is_err() {
                break;
            }
        }
    })
}
```

## Broadcast chat

The usual shape of a realtime feature: every connection forwards a shared broadcast channel to its client while reading the client's messages. Split the socket, run both directions as tasks, and stop one when the other ends:

```rust
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::{self, error::RecvError};
use zapline::prelude::*;
use zapline::ws::{Message, WebSocket, WebSocketUpgrade};

#[derive(Clone)]
struct Room(broadcast::Sender<String>);

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
                Err(RecvError::Lagged(_)) => continue,      // slow client: skip ahead
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

fn app() -> App {
    let (tx, _) = broadcast::channel(256);
    App::new().state(Room(tx)).get("/ws", chat)
}
```

The browser side is the standard API:

```js
const ws = new WebSocket(`ws://${location.host}/ws`, ["chat"]);
ws.onmessage = (e) => console.log(e.data);
ws.send("hello, room");
```

## Production concerns

- **Authentication.** Browsers cannot set an `Authorization` header on a `WebSocket`. Authenticate with the session or cookie that the handshake request already carries (extract `Session` or `CurrentUser<U>` in the handler), or with a short-lived token in the query string that you validate before upgrading.
- **Check the `Origin`.** WebSocket handshakes are not subject to CORS, so a malicious page can open a socket to your server using the visitor's cookies (cross-site WebSocket hijacking). When you authenticate with cookies, reject unexpected origins:

  ```rust
  use zapline::prelude::*;
  use zapline::ws::WebSocketUpgrade;

  async fn secure_chat(headers: HeaderMap, ws: WebSocketUpgrade) -> Result<Response> {
      let origin = headers.get("origin").and_then(|v| v.to_str().ok());
      if origin != Some("https://app.example.com") {
          return Err(Error::forbidden().with_code("bad_origin"));
      }
      Ok(ws.on_upgrade(|_socket| async {}))
  }
  ```

- **Middleware only sees the handshake.** `Timeout`, `ConcurrencyLimit` and `RateLimit` apply to the upgrade request. They don't bound the lifetime of the connection afterwards, and a connection doesn't hold a concurrency slot. Limit connections yourself (a semaphore in state, or a per-user count) and use `max_message_size`.
- **Dead peers.** Idle proxies and NATs drop silent connections, and TCP won't tell you for a long time. Send a ping every 20 to 30 seconds with `socket.send(Message::ping(Bytes::new()))`, and treat a failed send as a closed connection.
- **Reverse proxies** must forward the upgrade headers. For nginx: `proxy_http_version 1.1; proxy_set_header Upgrade $http_upgrade; proxy_set_header Connection "upgrade";`, plus a long `proxy_read_timeout`.
- **Shutdown.** Open sockets keep running while the server drains. If you use a worker, a job or `App::tasks_grace_period(..)`, `State<zapline::tasks::Shutdown>` is available: `select!` on `shutdown.wait()` in your loop and call `socket.close_with(close_code::AWAY, "restarting")`. See [Background tasks](tasks.md).
- **HTTP/2.** With the `http2` feature, the same handler also serves WebSockets over HTTP/2 (RFC 8441), over TLS (ALPN `h2`) and h2c. The server advertises `SETTINGS_ENABLE_CONNECT_PROTOCOL`; a client opens a socket with an extended `CONNECT` and `:protocol = websocket`. zapline routes it like the HTTP/1.1 `GET` (handlers and middleware see `GET`), validates `Sec-WebSocket-Version: 13`, negotiates subprotocols, and answers `200` instead of `101`. Each socket is one HTTP/2 stream, so many sockets share one connection. WebSockets over HTTP/3 (RFC 9220) are not supported yet.

## Testing

The in-process test client has no real connection to upgrade. There, `on_upgrade` still validates the handshake and returns the `101` response, so handshake behavior is testable, but the callback never runs:

```rust
use zapline::prelude::*;
use zapline::ws::WebSocketUpgrade;

fn app() -> App {
    App::new().get("/ws", |ws: WebSocketUpgrade| async move { ws.on_upgrade(|_socket| async {}) })
}

#[tokio::test]
async fn handshake_is_validated() {
    let client = app().test();
    let res = client
        .get("/ws")
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await;
    assert_eq!(res.status(), 101);
    assert_eq!(client.get("/ws").send().await.status(), 426);   // a plain GET is not an upgrade
}
```

To test the conversation itself, serve the app on an ephemeral port and connect with a real WebSocket client. Add one to your dev-dependencies, for example `tokio-tungstenite = "0.30"` with its `connect` feature:

```rust
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as ClientMessage;
use zapline::prelude::*;
use zapline::ws::{WebSocket, WebSocketUpgrade};

async fn echo(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if (msg.is_text() || msg.is_binary()) && socket.send(msg).await.is_err() {
                break;
            }
        }
    })
}

#[tokio::test]
async fn echoes_over_a_real_socket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(App::new().get("/ws", echo).serve(listener));

    let (mut ws, response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
    assert_eq!(response.status(), 101);

    ws.send(ClientMessage::text("hello")).await.unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    assert_eq!(reply, ClientMessage::text("hello"));
}
```
