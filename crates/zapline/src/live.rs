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

    /// Send `data` as an `event` to everyone subscribed to `topic`, and
    /// return how many subscribers there were.
    ///
    /// ```
    /// let live = zapline::live::Live::new();
    /// assert_eq!(live.publish("orders", "created", r#"{"id":7}"#), 0);
    /// ```
    pub fn publish(&self, topic: &str, event: &str, data: impl Into<String>) -> usize {
        let mut topics = self.lock();
        let Some(sender) = topics.get(topic) else {
            return 0;
        };
        let update = Update {
            topic: topic.to_owned(),
            event: event.to_owned(),
            data: data.into(),
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
        let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
            loop {
                match receiver.recv().await {
                    Ok(update) => return Some((update, receiver)),
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
        self.state(live).dev_panel(panel)
    }
}
