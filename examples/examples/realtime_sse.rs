//! Server-Sent Events: a clock that pushes the time every second, plus a
//! newline-delimited JSON export streamed row by row.
//!
//!     cargo run -p zapline-examples --example realtime_sse
//!     curl -N localhost:8080/clock
//!     curl localhost:8080/export

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::stream::{self, Stream, StreamExt};
use serde::Serialize;
use zapline::prelude::*;
use zapline::sse::{Event, KeepAlive, Sse};
use zapline::stream::Ndjson;

#[derive(Serialize)]
struct Tick {
    seq: u64,
    unix: u64,
}

/// One event per second, forever. `Last-Event-ID` lets reconnecting
/// browsers resume the sequence.
async fn clock(headers: HeaderMap) -> Sse<impl Stream<Item = Result<Event>>> {
    let start = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(0, |n| n + 1);

    let ticks = stream::unfold(start, |seq| async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let event = Event::default()
            .event("tick")
            .id(seq.to_string())
            .json_data(Tick { seq, unix });
        Some((event, seq + 1))
    });

    Sse::new(ticks).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[derive(Serialize)]
struct Row {
    id: u32,
    square: u64,
}

/// Rows are serialized and sent as they are produced; nothing is buffered.
async fn export() -> Ndjson {
    Ndjson::new(stream::iter(1..=1000u32).map(|id| Row {
        id,
        square: u64::from(id) * u64::from(id),
    }))
}

async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html><meta charset="utf-8"><title>clock</title>
<pre id="out">waiting...</pre>
<script>
  const es = new EventSource("/clock");
  es.addEventListener("tick", (e) => {
    const t = JSON.parse(e.data);
    document.getElementById("out").textContent =
      `#${t.seq}  ${new Date(t.unix * 1000).toLocaleTimeString()}`;
  });
</script>"#,
    )
}

fn main() -> std::io::Result<()> {
    App::new()
        .get("/", index)
        .get("/clock", clock)
        .get("/export", export)
        .run("0.0.0.0:8080")
}
