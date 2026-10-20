# Server-Sent Events & streaming

Return `Sse` with a stream of `Event`s and each event reaches the client as soon as the stream yields it:

```rust
use std::time::Duration;
use futures_util::stream::{self, Stream, StreamExt};
use zapline::prelude::*;
use zapline::sse::{Event, KeepAlive, Sse};

async fn ticks() -> Sse<impl Stream<Item = Event>> {
    let events = stream::iter(1..=3).map(|n| Event::default().event("tick").data(n.to_string()));
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn app() -> App {
    App::new().get("/ticks", ticks)
}
```

`zapline::sse` and `zapline::stream` are always compiled. Only `Ndjson` and the JSON helpers need the `json` feature, which is on by default. The examples use `futures_util` for stream combinators, so add `futures-util = "0.3"` to your `Cargo.toml`.

## Server-Sent Events

An `Sse` response is `Content-Type: text/event-stream` with `Cache-Control: no-cache` and `X-Accel-Buffering: no`, so proxies such as nginx don't buffer the stream. The stream may yield plain `Event`s or `Result<Event, E>`. An `Err` ends the response and closes the connection mid-stream, because once the headers are sent there is no way to report a status.

### `Event`

Build events with a chain of methods. Fields are written in the order you call them.

| Method | Wire field | Notes |
|---|---|---|
| `.data(text)` | `data:` | Multi-line text becomes one `data:` line per line |
| `.json_data(value)?` | `data:` | Serializes compact JSON on one line. Returns `Result<Event>`. |
| `.event(name)` | `event:` | The event type, dispatched to `addEventListener(name)` in the browser. Single line only. |
| `.id(id)` | `id:` | Echoed by the browser in the `Last-Event-ID` header when it reconnects. No line breaks or NUL. |
| `.retry(duration)` | `retry:` | The client's reconnection delay |
| `.comment(text)` | `:text` | Ignored by clients |

Line breaks (`\n`, `\r\n`, `\r`) in `data` and `comment` become new lines of the same field, so user-supplied text can never inject extra fields. `event` and `id` must be single-line. A bad value doesn't panic: the event is marked invalid (`is_valid()` is `false`) and the stream ends with an error when it reaches the client. `Event::to_bytes()` gives the wire format.

### `KeepAlive`

Proxies and load balancers close connections that stay silent. `Sse::keep_alive(KeepAlive::new())` sends a comment frame whenever the stream has been idle for the interval (default 15 seconds, at least 1 ms). `.text("ping")` changes the comment text.

### A live feed

A typical feed subscribes to a broadcast channel. Turn the receiver into a stream with `stream::unfold`:

```rust
use std::time::Duration;
use futures_util::stream::{self, Stream};
use tokio::sync::broadcast::{self, error::RecvError};
use zapline::prelude::*;
use zapline::sse::{Event, KeepAlive, Sse};

#[derive(Clone)]
struct Bus(broadcast::Sender<String>);

async fn notifications(State(bus): State<Bus>, headers: HeaderMap) -> Sse<impl Stream<Item = Event>> {
    // A reconnecting browser sends the id of the last event it saw.
    let last_seen = headers.get("last-event-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
    if let Some(id) = last_seen {
        // replay missed events from your own store here
        let _ = id;
    }

    let events = stream::unfold(bus.0.subscribe(), |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(text) => return Some((Event::default().event("notice").data(text), rx)),
                Err(RecvError::Lagged(_)) => continue,      // slow client: skip ahead
                Err(RecvError::Closed) => return None,      // ends the response
            }
        }
    });
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[post("/notify")]
async fn notify(State(bus): State<Bus>, body: String) -> StatusCode {
    let _ = bus.0.send(body);
    StatusCode::ACCEPTED
}

fn app() -> App {
    let (tx, _) = broadcast::channel(256);
    App::new().state(Bus(tx)).get("/notifications", notifications).mount(notify)
}
```

```js
const es = new EventSource("/notifications");
es.addEventListener("notice", (e) => console.log(e.data));
```

When a client goes away, the response body, and with it your stream, is dropped. Release resources in `Drop`, or simply let the receiver drop. A stream with typed events and ids looks like this:

```rust
use std::time::Duration;
use futures_util::stream::{self, Stream};
use serde::Serialize;
use zapline::prelude::*;
use zapline::sse::{Event, Sse};

#[derive(Serialize)]
struct Tick {
    seq: u64,
}

async fn clock() -> Sse<impl Stream<Item = Result<Event>>> {
    let ticks = stream::unfold(0u64, |seq| async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let event = Event::default().event("tick").id(seq.to_string()).json_data(Tick { seq });
        Some((event, seq + 1))
    });
    Sse::new(ticks)
}
```

Things to know:

- **Browsers allow six connections per host over HTTP/1.1.** Each open `EventSource` uses one. Use one stream per page and multiplex event types on it.
- **Shutdown.** A long-lived stream keeps its connection open while the server drains. End streams when the app shuts down by racing against `State<zapline::tasks::Shutdown>` (`stream.take_until(shutdown.wait())`). See [Background tasks](tasks.md).
- **Compression** skips `text/event-stream` by default, because encoders buffer. See [Hardening middleware](hardening.md#compression).
- **Timeouts.** `Timeout` bounds the time to the response head, not the lifetime of the stream.
- **Authentication.** `EventSource` can't set headers, so authenticate with a cookie or a short-lived token in the query string.
- **Testing.** The in-process client collects the whole body, so an infinite stream never finishes. Test with a finite stream (`stream::iter`, or `.take(3)`):

```rust
use futures_util::stream::{self, Stream, StreamExt};
use zapline::prelude::*;
use zapline::sse::{Event, Sse};

async fn ticks() -> Sse<impl Stream<Item = Event>> {
    Sse::new(stream::iter(1..=2).map(|n| Event::default().event("tick").data(n.to_string())))
}

#[tokio::test]
async fn ticks_are_events() {
    let res = App::new().get("/ticks", ticks).test().get("/ticks").send().await;
    assert_eq!(res.header("content-type"), Some("text/event-stream"));
    assert_eq!(res.text(), "event: tick\ndata: 1\n\nevent: tick\ndata: 2\n\n");
}
```

## `StreamBody`

`StreamBody` turns any stream of byte chunks into a response with chunked transfer encoding. Chunks can be anything convertible to `Bytes` (`String`, `Vec<u8>`, `&'static str`, `Bytes`), and an `Err` item aborts the response mid-stream. The default `Content-Type` is `application/octet-stream`:

```rust
use futures_util::stream;
use zapline::prelude::*;
use zapline::stream::StreamBody;

async fn report() -> StreamBody<impl futures_util::Stream<Item = std::result::Result<String, std::io::Error>>> {
    let rows = (1..=1000).map(|id| Ok(format!("{id},row-{id}\n")));
    StreamBody::new(stream::iter(rows)).content_type("text/csv")
}
```

An invalid `content_type` value turns the response into a `500` instead of panicking. To build the body yourself, `Body::from_stream(stream)` makes a `Body` for a plain `Response`.

## `Ndjson`

`Ndjson` streams [newline-delimited JSON](https://github.com/ndjson/ndjson-spec) (`application/x-ndjson`): each item is serialized as one compact JSON document followed by `\n`. Rows are serialized and sent as they are produced, so a million-row export needs constant memory.

```rust
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use zapline::prelude::*;
use zapline::stream::Ndjson;

#[derive(Serialize)]
struct Row {
    id: u32,
    square: u64,
}

async fn export() -> Ndjson {
    Ndjson::new(stream::iter(1..=1000u32).map(|id| Row { id, square: u64::from(id) * u64::from(id) }))
}
```

`Ndjson::new(stream)` takes infallible items; a serialization failure aborts the response. `Ndjson::try_new(stream)` takes `Result<T, E>` items for sources that can fail (a database cursor, say), and an `Err` aborts the response. Clients detect an aborted stream because the connection ends without a clean close, so make consumers tolerate a truncated last line.

## `BodyStream`: large uploads

`Json<T>`, `Form<T>`, `Bytes` and `String` buffer the whole body, bounded by the app's body limit (2 MiB by default). `BodyStream` reads the request body chunk by chunk without buffering it, so a multi-gigabyte upload needs constant memory. It must be the **last** handler argument and implements `Stream<Item = Result<Bytes>>`, so `futures_util::StreamExt` works on it too.

```rust
use tokio::io::AsyncWriteExt;
use zapline::prelude::*;
use zapline::stream::BodyStream;

#[put("/uploads/{name}")]
async fn upload(Path(name): Path<u64>, body: BodyStream) -> Result<StatusCode> {
    let mut body = body.with_limit(1024 * 1024 * 1024);          // 1 GiB for this route only
    let mut file = tokio::fs::File::create(format!("/var/uploads/{name}.bin")).await?;
    while let Some(chunk) = body.chunk().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(StatusCode::CREATED)
}
```

| Method | Effect |
|---|---|
| `chunk().await?` | The next `Bytes` chunk, or `None` at the end of the body |
| `with_limit(bytes)` | Raises or lowers the limit for this handler. The default is the app's `body_limit` (2 MiB), so **raise it** for large uploads. |
| `limit()`, `bytes_read()` | The current limit and the number of bytes received so far |

The limit is enforced across chunks: once more bytes arrive than allowed, the stream yields a `413 Payload Too Large` error and ends. A `Content-Length` that already exceeds the limit fails on the first read, before any bytes are consumed. For form uploads with metadata and several files, use `Multipart` instead (see [Static files & uploads](files.md#uploads-with-multipart)).
