//! Live updates: publish to a topic, and every browser subscribed to it
//! gets the message at once, over Server-Sent Events (with htmx, no
//! JavaScript to write) or WebSockets.
//!
//! ```
//! use zapline::live::Live;
//! use zapline::prelude::*;
//!
//! // Browsers listen: <div hx-ext="sse" sse-connect="/live/chat" sse-swap="message"></div>
//! #[get("/live/chat")]
//! async fn listen(live: Live) -> Response {
//!     live.sse("chat").into_response()
//! }
//!
//! #[post("/chat")]
//! async fn say(live: Live) -> StatusCode {
//!     // Every listener's <div> now shows this HTML (render it with your
//!     // templates in a real app, so user input is escaped).
//!     live.publish("chat", "message", "<p>Someone said hi</p>");
//!     StatusCode::NO_CONTENT
//! }
//!
//! let app = App::new().live(Live::new()).mount((listen, say));
//! ```
//!
//! Topics are created by the first subscriber and dropped with the last
//! one. Publishing reaches the subscribers of this process; a subscriber
//! too slow to keep up skips the messages it missed rather than slowing
//! the publisher.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio::sync::broadcast;

use crate::dev::{escape, DevPanel};
use crate::extract::FromRequestParts;
use crate::sse::{Event, KeepAlive, Sse};
use crate::{App, Error, Request, Result};

/// Messages a topic buffers for a subscriber that falls behind.
const DEFAULT_CAPACITY: usize = 256;

/// One published message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// The topic it was published to.
    pub topic: String,
    /// What kind of message: the SSE event name (htmx's `sse-swap`).
    pub event: String,
    /// The payload: HTML for htmx, JSON, or text.
    pub data: String,
}

impl Update {
    /// The update as an SSE [`Event`].
    ///
    /// ```
    /// use zapline::live::Update;
    ///
    /// let update = Update { topic: "t".into(), event: "message".into(), data: "<p>hi</p>".into() };
    /// assert!(update.to_event().is_valid());
    /// ```
    pub fn to_event(&self) -> Event {
        Event::default().event(&self.event).data(&self.data)
    }
}

/// The pub/sub hub. Cheap to clone; a handler argument once registered with
/// [`App::live`].
#[derive(Clone)]
pub struct Live {
    topics: Arc<Mutex<HashMap<String, broadcast::Sender<Update>>>>,
    capacity: usize,
    #[cfg(feature = "redis")]
    relay: Option<Arc<relay::Relay>>,
}

impl Default for Live {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Live")
            .field("topics", &self.lock().len())
            .finish()
    }
}

impl Live {
    /// A hub for this process.
    ///
    /// ```
    /// let live = zapline::live::Live::new();
    /// ```
    pub fn new() -> Self {
        Live {
            topics: Arc::default(),
            capacity: DEFAULT_CAPACITY,
            #[cfg(feature = "redis")]
            relay: None,
        }
    }

    /// How many messages a topic keeps for a subscriber that falls behind
    /// (default 256). Older ones are skipped for that subscriber.
    ///
    /// ```
    /// let live = zapline::live::Live::new().capacity(1024);
    /// ```
    #[must_use]
    pub fn capacity(mut self, n: usize) -> Self {
        self.capacity = n.max(1);
        self
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, broadcast::Sender<Update>>> {
        self.topics.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Also deliver updates to (and from) every other instance of the app
    /// through Redis pub/sub (feature `redis`), on channels
    /// `zapline:live:{topic}`. Register the hub with [`App::live`], which
    /// runs the listener.
    ///
    /// ```
    /// use zapline::live::Live;
    /// use zapline::redis::Redis;
    ///
    /// let live = Live::new().redis(Redis::new("redis://localhost")?);
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[cfg(feature = "redis")]
    #[must_use]
    pub fn redis(mut self, redis: crate::redis::Redis) -> Self {
        self.relay = Some(Arc::new(relay::Relay::new(redis)));
        self
    }

    /// Send `data` as an `event` to everyone subscribed to `topic`, and
    /// return how many subscribers there were in this process.
    ///
    /// ```
    /// let live = zapline::live::Live::new();
    /// assert_eq!(live.publish("orders", "created", r#"{"id":7}"#), 0);
    /// ```
    pub fn publish(&self, topic: &str, event: &str, data: impl Into<String>) -> usize {
        let data = data.into();
        #[cfg(feature = "redis")]
        if let Some(relay) = &self.relay {
            relay.send(topic, event, &data);
        }
        self.deliver(topic, event, data)
    }

    /// Hand an update to this process's subscribers.
    fn deliver(&self, topic: &str, event: &str, data: String) -> usize {
        let mut topics = self.lock();
        let Some(sender) = topics.get(topic) else {
            return 0;
        };
        let update = Update {
            topic: topic.to_owned(),
            event: event.to_owned(),
            data,
        };
        match sender.send(update) {
            Ok(n) => n,
            Err(_) => {
                // Everyone left: forget the topic.
                topics.remove(topic);
                0
            }
        }
    }

    /// [`publish`](Self::publish) `value` as JSON.
    ///
    /// # Errors
    ///
    /// If `value` can't be serialized.
    ///
    /// ```
    /// # fn main() -> zapline::Result<()> {
    /// let live = zapline::live::Live::new();
    /// live.publish_json("orders", "created", &serde_json::json!({ "id": 7 }))?;
    /// # Ok(()) }
    /// ```
    #[cfg(feature = "json")]
    pub fn publish_json<T: serde::Serialize + ?Sized>(
        &self,
        topic: &str,
        event: &str,
        value: &T,
    ) -> Result<usize> {
        let data = serde_json::to_string(value)
            .map_err(|e| Error::internal("cannot serialize a live update").with_source(e))?;
        Ok(self.publish(topic, event, data))
    }

    /// The updates published to `topic` from now on, for as long as the
    /// subscription is kept. Read them with [`Subscription::next`], or use
    /// it as a `Stream` (to forward to a WebSocket, say).
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let live = zapline::live::Live::new();
    /// let mut updates = live.subscribe("orders");
    /// live.publish("orders", "created", "7");
    /// assert_eq!(updates.next().await.unwrap().data, "7");
    /// # }
    /// ```
    pub fn subscribe(&self, topic: &str) -> Subscription {
        let receiver = self
            .lock()
            .entry(topic.to_owned())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .subscribe();
        let listener = Listener {
            receiver: Some(receiver),
            topics: self.topics.clone(),
            topic: topic.to_owned(),
        };
        let stream = futures_util::stream::unfold(listener, |mut listener| async move {
            let receiver = listener.receiver.as_mut()?;
            loop {
                match receiver.recv().await {
                    Ok(update) => return Some((update, listener)),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Subscription {
            stream: Box::pin(stream),
        }
    }

    /// An SSE response that streams `topic` to the browser, with a
    /// keep-alive comment every 15 s so proxies don't cut it.
    ///
    /// ```
    /// use zapline::live::Live;
    /// use zapline::prelude::*;
    ///
    /// #[get("/live/{room}")]
    /// async fn listen(live: Live, Path(room): Path<String>) -> Response {
    ///     live.sse(&format!("room:{room}")).into_response()
    /// }
    /// ```
    pub fn sse(&self, topic: &str) -> Sse<impl Stream<Item = Event> + Send + 'static> {
        use futures_util::StreamExt;
        Sse::new(self.subscribe(topic).map(|u| u.to_event()))
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
    }

    /// Subscribers per topic.
    fn topics(&self) -> Vec<(String, usize)> {
        let mut topics: Vec<(String, usize)> = self
            .lock()
            .iter()
            .map(|(t, s)| (t.clone(), s.receiver_count()))
            .filter(|(_, n)| *n > 0)
            .collect();
        topics.sort();
        topics
    }
}

/// One subscriber's receiver. When the last one of a topic goes, the topic
/// goes too, so clients can't grow the map by naming topics nobody
/// publishes to.
struct Listener {
    receiver: Option<broadcast::Receiver<Update>>,
    topics: Arc<Mutex<HashMap<String, broadcast::Sender<Update>>>>,
    topic: String,
}

impl Drop for Listener {
    fn drop(&mut self) {
        // `subscribe` takes the same lock, so no new receiver can appear
        // between this check and the removal.
        let mut topics = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        drop(self.receiver.take());
        if topics
            .get(&self.topic)
            .is_some_and(|tx| tx.receiver_count() == 0)
        {
            topics.remove(&self.topic);
        }
    }
}

/// The updates of one topic, from [`Live::subscribe`]. Unsubscribes when
/// dropped.
pub struct Subscription {
    stream: Pin<Box<dyn Stream<Item = Update> + Send>>,
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

impl Subscription {
    /// The next update; `None` only if the hub is gone.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let live = zapline::live::Live::new();
    /// let mut sub = live.subscribe("t");
    /// live.publish("t", "e", "hi");
    /// assert_eq!(sub.next().await.unwrap().event, "e");
    /// # }
    /// ```
    pub async fn next(&mut self) -> Option<Update> {
        std::future::poll_fn(|cx| self.stream.as_mut().poll_next(cx)).await
    }
}

impl Stream for Subscription {
    type Item = Update;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Update>> {
        self.stream.as_mut().poll_next(cx)
    }
}

impl FromRequestParts for Live {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Live>().cloned().ok_or_else(|| {
            Error::internal("no live hub registered: add `.live(Live::new())` to the app")
        })
    }
}

impl App {
    /// Register `live` for handlers ([`Live`] extractor), jobs and
    /// background tasks. The [dev console](crate::dev) lists its topics.
    ///
    /// ```
    /// let app = zapline::App::new().live(zapline::live::Live::new());
    /// ```
    pub fn live(self, live: Live) -> Self {
        let panel = DevPanel::new("live", "Live topics", |req: &Request| {
            let topics = req.state::<Live>().map(Live::topics).unwrap_or_default();
            Box::pin(async move {
                if topics.is_empty() {
                    return "<p class=dim>No subscribers.</p>".to_owned();
                }
                let mut html = String::from("<table><tr><th>Topic<th>Subscribers</tr>");
                for (topic, n) in topics {
                    html.push_str(&format!(
                        "<tr><td><code>{}</code><td>{n}</tr>",
                        escape(&topic)
                    ));
                }
                html.push_str("</table>");
                html
            })
        });
        #[cfg(feature = "redis")]
        let app = match live.relay.clone() {
            Some(relay) => {
                let hub = live.clone();
                self.background(move |_res, shutdown: crate::tasks::Shutdown| async move {
                    relay
                        .listen(shutdown, move |topic, event, data| {
                            hub.deliver(topic, event, data);
                        })
                        .await;
                })
            }
            None => self,
        };
        #[cfg(not(feature = "redis"))]
        let app = self;
        app.state(live).dev_panel(panel)
    }
}

#[cfg(feature = "redis")]
mod relay {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::OnceLock;
    use std::time::Duration;

    use serde::{Deserialize, Serialize};
    use tokio::sync::mpsc;

    use crate::redis::Redis;
    use crate::tasks::{log_warn, Shutdown};

    const CHANNEL: &str = "zapline:live:";

    /// What goes over Redis: the update, and which process sent it (so it
    /// isn't delivered twice there).
    #[derive(Serialize, Deserialize)]
    struct Wire<'a> {
        o: &'a str,
        e: &'a str,
        d: &'a str,
    }

    pub(super) struct Relay {
        redis: Redis,
        origin: String,
        /// Updates waiting to be published, in order.
        outbox: OnceLock<mpsc::UnboundedSender<(String, String)>>,
    }

    impl Relay {
        pub(super) fn new(redis: Redis) -> Self {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
            );
            h.write_u32(std::process::id());
            Relay {
                redis,
                origin: format!("{:016x}", h.finish()),
                outbox: OnceLock::new(),
            }
        }

        pub(super) fn send(&self, topic: &str, event: &str, data: &str) {
            let wire = Wire {
                o: &self.origin,
                e: event,
                d: data,
            };
            let Ok(payload) = serde_json::to_string(&wire) else {
                return;
            };
            let outbox = self.outbox.get_or_init(|| {
                let (tx, mut rx) = mpsc::unbounded_channel::<(String, String)>();
                let redis = self.redis.clone();
                match tokio::runtime::Handle::try_current() {
                    Ok(rt) => {
                        rt.spawn(async move {
                            while let Some((channel, payload)) = rx.recv().await {
                                if let Err(e) = redis.cmd(["PUBLISH", &channel, &payload]).await {
                                    log_warn(format_args!("live: {}", e.message()));
                                }
                            }
                        });
                    }
                    Err(_) => log_warn(format_args!(
                        "live: publishing outside a Tokio runtime; updates stay in this process"
                    )),
                }
                tx
            });
            let _ = outbox.send((format!("{CHANNEL}{topic}"), payload));
        }

        /// Deliver other instances' updates until shutdown, reconnecting
        /// with backoff when Redis goes away.
        pub(super) async fn listen(
            &self,
            shutdown: Shutdown,
            deliver: impl Fn(&str, &str, String),
        ) {
            let mut backoff = Duration::from_millis(500);
            while !shutdown.is_shutdown() {
                match self.redis.psubscribe(&format!("{CHANNEL}*")).await {
                    Ok(mut sub) => loop {
                        backoff = Duration::from_millis(500);
                        tokio::select! {
                            message = sub.next() => match message {
                                Ok((channel, payload)) => {
                                    let Some(topic) = channel.strip_prefix(CHANNEL) else { continue };
                                    let Ok(wire) = serde_json::from_slice::<Wire<'_>>(&payload) else { continue };
                                    if wire.o != self.origin {
                                        deliver(topic, wire.e, wire.d.to_owned());
                                    }
                                }
                                Err(e) => {
                                    log_warn(format_args!("live: lost the Redis subscription: {e}"));
                                    break;
                                }
                            },
                            () = shutdown.wait() => return,
                        }
                    },
                    Err(e) => log_warn(format_args!("live: {}", e.message())),
                }
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    () = shutdown.wait() => return,
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Live;

    #[tokio::test]
    async fn unused_topics_are_forgotten() {
        let live = Live::new();
        for i in 0..100 {
            let sub = live.subscribe(&format!("t{i}"));
            drop(sub);
        }
        let mut kept = live.subscribe("kept");
        let other = live.subscribe("kept");
        drop(other);
        assert_eq!(live.lock().len(), 1);
        live.publish("kept", "e", "still here");
        assert_eq!(kept.next().await.unwrap().data, "still here");
        drop(kept);
        assert!(live.lock().is_empty());
    }
}
