# Live updates

`zapline::live` pushes changes to every open page as they happen: publish to a topic, and every browser subscribed to it gets the message. With htmx you write no JavaScript. It's always available (no feature flag).

```rust
use zapline::live::Live;
use zapline::prelude::*;

#[get("/live/chat")]
async fn listen(live: Live) -> Response {
    live.sse("chat").into_response()
}

#[post("/chat")]
async fn say(live: Live, views: Views, Form(msg): Form<Message>) -> Result<StatusCode> {
    let html = views.render("chat/message.html", context! { msg })?;
    live.publish("chat", "message", html.0);
    Ok(StatusCode::NO_CONTENT)
}

let app = App::new().live(Live::new()).mount((listen, say));
```

```html
<script src="https://unpkg.com/htmx-ext-sse@2.2.2/sse.js" defer></script>

<div hx-ext="sse" sse-connect="/live/chat" sse-swap="message" hx-swap="beforeend"></div>
```

`sse-swap` names the event to swap in. Here every `message` is appended to the `<div>`.

Projects created with `zapline new --template fullstack` already work this way: open the todo page in two tabs, change a todo in one, and the other updates. The handlers publish the re-rendered `list` block after each change.

## The API

| Call | |
|---|---|
| `live.publish(topic, event, data)` | Send to everyone on `topic`. Returns how many got it. |
| `live.publish_json(topic, event, &value)` | The same, with `value` as JSON. |
| `live.sse(topic)` | An SSE response streaming the topic, with a keep-alive every 15 s. |
| `live.subscribe(topic)` | A `Subscription`: `sub.next().await`, or use it as a `Stream`. |
| `Live::new().capacity(n)` | Messages kept for a subscriber that falls behind (default 256). |

Topics are just strings. Use `room:42` or `user:7` for one room or one user, and check access in the handler that calls `live.sse` (for example by taking a `CurrentUser` argument).

A topic exists while someone listens. A slow subscriber skips the messages it missed rather than slowing everyone else down. The [dev console](dev-console.md) lists topics and their subscriber counts.

## WebSockets

Forward a subscription to a socket:

```rust,ignore
#[get("/ws/chat")]
async fn ws(live: Live, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |mut socket| async move {
        let mut sub = live.subscribe("chat");
        while let Some(update) = sub.next().await {
            if socket.send(Message::text(update.data)).await.is_err() {
                break;
            }
        }
    })
}
```

## Several instances

`Live::new()` reaches the subscribers of one process. Behind a load balancer with several instances, use `Live::new().redis(redis)` (feature `redis`) so every update reaches visitors on every instance: see [Running several instances](scaling.md).

## Testing

Subscribe in the test and assert on what a handler published:

```rust,ignore
let client = app(Database::sqlite_memory()).test();
let live: Live = client.resources().await.require()?;
let mut updates = live.subscribe("todos");
client.post("/todos").form(&[("title", "x")]).send().await;
assert!(updates.next().await.unwrap().data.contains("x"));
```
