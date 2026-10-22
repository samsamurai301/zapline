//! Sending email: SMTP in production, an inbox page in development, and
//! captured messages in tests.
//!
//! Register a [`Mailer`] on the app and take it as a handler argument:
//!
//! ```no_run
//! use zapline::mail::{Email, Mailer};
//! use zapline::prelude::*;
//!
//! #[post("/invite")]
//! async fn invite(mail: Mailer) -> Result<&'static str> {
//!     mail.send(
//!         Email::to("ada@example.com")
//!             .subject("You're invited")
//!             .text("Join us at https://example.com")
//!             .html("<p>Join us at <a href=\"https://example.com\">example.com</a></p>"),
//!     )
//!     .await?;
//!     Ok("sent")
//! }
//!
//! fn main() -> zapline::Result<()> {
//!     App::new()
//!         .mailer(Mailer::from_env()?) // SMTP_URL, MAIL_FROM
//!         .mount(invite)
//!         .run("0.0.0.0:8080")?;
//!     Ok(())
//! }
//! ```
//!
//! [`Mailer::from_env`] reads `SMTP_URL` (`smtps://user:pass@smtp.example.com`
//! for implicit TLS, `smtp://user:pass@host:587?tls=required` for STARTTLS)
//! and `MAIL_FROM`. When `SMTP_URL` is unset in a debug build, mail goes to
//! an in-memory **outbox** instead: every message is logged and the app
//! serves the latest ones at `GET /_dev/mail`, so sign-up and password-reset
//! links can be clicked locally without a mail server. A release build
//! without `SMTP_URL` refuses to start.
//!
//! In tests, use [`Mailer::outbox`] and read what was sent with
//! [`Mailer::sent`].
//!
//! With a database registered, [`Email`] is also a [`Job`](crate::jobs::Job):
//! register it with `.jobs(|q| q.register::<Email>())` and
//! `jobs.enqueue(email)` sends it in the background, retrying if the SMTP
//! server is down.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};

use crate::dev::{escape, internal, DevPanel};
use crate::extract::FromRequestParts;
use crate::response::{IntoResponse, Response};
use crate::{App, Error, Html, Request, Result, Routes};

/// How many messages the development outbox keeps.
const OUTBOX_SIZE: usize = 50;

/// The path of the development inbox page.
pub const DEV_INBOX_PATH: &str = "/_dev/mail";

/// One email message. Build it with [`Email::to`].
///
/// ```
/// use zapline::mail::Email;
///
/// let email = Email::to("ada@example.com")
///     .and_to("grace@example.com")
///     .subject("Hello")
///     .text("plain-text body")
///     .html("<p>HTML body</p>");
/// assert_eq!(email.to.len(), 2);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Email {
    /// Recipients (`ada@example.com` or `Ada <ada@example.com>`).
    pub to: Vec<String>,
    /// Sender; the mailer's default sender when `None`.
    pub from: Option<String>,
    /// Where replies go.
    pub reply_to: Option<String>,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub text: Option<String>,
    /// HTML body. With both bodies, clients pick the one they display.
    pub html: Option<String>,
}

impl Email {
    /// A message to `address`.
    pub fn to(address: impl Into<String>) -> Self {
        Email {
            to: vec![address.into()],
            ..Email::default()
        }
    }

    /// Add another recipient.
    pub fn and_to(mut self, address: impl Into<String>) -> Self {
        self.to.push(address.into());
        self
    }

    /// Set the sender (instead of the mailer's default).
    pub fn from(mut self, address: impl Into<String>) -> Self {
        self.from = Some(address.into());
        self
    }

    /// Set the reply-to address.
    pub fn reply_to(mut self, address: impl Into<String>) -> Self {
        self.reply_to = Some(address.into());
        self
    }

    /// Set the subject.
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = subject.into();
        self
    }

    /// Set the plain-text body.
    pub fn text(mut self, body: impl Into<String>) -> Self {
        self.text = Some(body.into());
        self
    }

    /// Set the HTML body (for example a rendered [view](crate::view)).
    pub fn html(mut self, body: impl Into<String>) -> Self {
        self.html = Some(body.into());
        self
    }

    fn to_message(&self, default_from: &str) -> Result<Message> {
        let from = self.from.as_deref().unwrap_or(default_from);
        let mut builder = Message::builder()
            .from(mailbox(from)?)
            .subject(self.subject.clone());
        if self.to.is_empty() {
            return Err(Error::internal("an email needs at least one recipient"));
        }
        for to in &self.to {
            builder = builder.to(mailbox(to)?);
        }
        if let Some(reply_to) = &self.reply_to {
            builder = builder.reply_to(mailbox(reply_to)?);
        }
        let built = match (&self.text, &self.html) {
            (Some(text), Some(html)) => builder.multipart(MultiPart::alternative_plain_html(
                text.clone(),
                html.clone(),
            )),
            (None, Some(html)) => builder.singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_HTML)
                    .body(html.clone()),
            ),
            (text, None) => builder.singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_PLAIN)
                    .body(text.clone().unwrap_or_default()),
            ),
        };
        built.map_err(|e| Error::internal(format!("cannot build the email: {e}")))
    }
}

fn mailbox(address: &str) -> Result<Mailbox> {
    address
        .parse()
        .map_err(|e| Error::internal(format!("invalid email address `{address}`: {e}")))
}

// ---------------------------------------------------------------------------
// Mailer

enum Transport {
    Smtp(AsyncSmtpTransport<Tokio1Executor>),
    Outbox,
}

struct Inner {
    transport: Transport,
    from: String,
    outbox: Mutex<VecDeque<Email>>,
}

/// Sends [`Email`]s. Cheap to clone; register it with [`App::mailer`] and
/// take it as a handler argument (or `res.require::<Mailer>()`).
#[derive(Clone)]
pub struct Mailer {
    inner: Arc<Inner>,
}

impl fmt::Debug for Mailer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let transport = match self.inner.transport {
            Transport::Smtp(_) => "smtp",
            Transport::Outbox => "outbox",
        };
        f.debug_struct("Mailer")
            .field("transport", &transport)
            .field("from", &self.inner.from)
            .finish()
    }
}

fn default_from() -> String {
    std::env::var("MAIL_FROM")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "no-reply@localhost".to_string())
}

impl Mailer {
    fn with(transport: Transport) -> Self {
        Mailer {
            inner: Arc::new(Inner {
                transport,
                from: default_from(),
                outbox: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Send through the SMTP server at `url`: `smtps://user:pass@host` (TLS,
    /// port 465), `smtp://user:pass@host:587?tls=required` (STARTTLS) or
    /// `smtp://localhost:1025` (no TLS, e.g. Mailpit).
    ///
    /// # Errors
    ///
    /// If the URL is invalid.
    pub fn smtp(url: &str) -> Result<Self> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
            .map_err(|e| Error::internal(format!("invalid SMTP URL: {e}")))?
            .build();
        Ok(Self::with(Transport::Smtp(transport)))
    }

    /// Keep messages in memory instead of sending them: for development
    /// (they are shown at [`DEV_INBOX_PATH`]) and tests ([`Mailer::sent`]).
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> zapline::Result<()> {
    /// use zapline::mail::{Email, Mailer};
    ///
    /// let mailer = Mailer::outbox();
    /// mailer.send(Email::to("ada@example.com").subject("Hi").text("hello")).await?;
    /// assert_eq!(mailer.sent()[0].subject, "Hi");
    /// # Ok(()) }
    /// ```
    pub fn outbox() -> Self {
        Self::with(Transport::Outbox)
    }

    /// SMTP from `SMTP_URL`; without it, the [outbox](Self::outbox) in
    /// debug builds.
    ///
    /// # Errors
    ///
    /// If `SMTP_URL` is invalid, or unset in a release build.
    pub fn from_env() -> Result<Self> {
        match std::env::var("SMTP_URL") {
            Ok(url) if !url.trim().is_empty() => Self::smtp(url.trim()),
            _ if cfg!(debug_assertions) => Ok(Self::outbox()),
            _ => Err(Error::internal(
                "SMTP_URL is not set: set it to e.g. smtps://user:pass@smtp.example.com",
            )),
        }
    }

    /// The sender used when an [`Email`] doesn't set one (default:
    /// `MAIL_FROM`, else `no-reply@localhost`).
    ///
    /// # Panics
    ///
    /// If the mailer has already been cloned.
    pub fn default_from(mut self, address: impl Into<String>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("set the default sender before cloning the mailer")
            .from = address.into();
        self
    }

    /// Whether messages stay in memory instead of being sent.
    pub fn is_outbox(&self) -> bool {
        matches!(self.inner.transport, Transport::Outbox)
    }

    /// Send `email`.
    ///
    /// # Errors
    ///
    /// If an address is invalid or the SMTP server rejects the message.
    pub async fn send(&self, email: Email) -> Result<()> {
        let message = email.to_message(&self.inner.from)?;
        match &self.inner.transport {
            Transport::Smtp(smtp) => {
                smtp.send(message).await.map_err(|e| {
                    Error::service_unavailable("cannot send the email").with_source(e)
                })?;
            }
            Transport::Outbox => {
                log_outbox(&email);
                let mut email = email;
                email.from.get_or_insert_with(|| self.inner.from.clone());
                let mut outbox = self.inner.outbox.lock().unwrap_or_else(|e| e.into_inner());
                if outbox.len() == OUTBOX_SIZE {
                    outbox.pop_front();
                }
                outbox.push_back(email);
            }
        }
        Ok(())
    }

    /// Messages kept by an [outbox](Self::outbox) mailer, oldest first
    /// (the last 50). Empty for SMTP.
    pub fn sent(&self) -> Vec<Email> {
        let outbox = self.inner.outbox.lock().unwrap_or_else(|e| e.into_inner());
        outbox.iter().cloned().collect()
    }
}

#[cfg(feature = "tracing")]
fn log_outbox(email: &Email) {
    tracing::info!(
        to = %email.to.join(", "),
        subject = %email.subject,
        "email kept in the dev outbox ({DEV_INBOX_PATH})"
    );
}

#[cfg(not(feature = "tracing"))]
fn log_outbox(email: &Email) {
    eprintln!(
        "[zapline] email to {} ({}) kept in the dev outbox ({DEV_INBOX_PATH})",
        email.to.join(", "),
        email.subject
    );
}

impl FromRequestParts for Mailer {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Mailer>().cloned().ok_or_else(|| {
            Error::internal("no mailer registered: add `.mailer(Mailer::from_env()?)` to the app")
        })
    }
}

impl App {
    /// Register `mailer` for handlers ([`Mailer`] extractor), jobs and
    /// startup hooks. An [outbox](Mailer::outbox) mailer also serves the
    /// development inbox at [`DEV_INBOX_PATH`].
    pub fn mailer(self, mailer: Mailer) -> Self {
        let outbox = mailer.is_outbox();
        let app = self.state(mailer);
        if !outbox {
            return app;
        }
        let panel = DevPanel::new("mail", "Mail", |req: &Request| {
            let count = req.state::<Mailer>().map_or(0, |m| m.sent().len());
            Box::pin(async move {
                format!(
                    "<p>{count} email(s) kept in the <a href=\"{DEV_INBOX_PATH}\">dev inbox</a>.</p>"
                )
            })
        });
        app.mount(internal(Routes::new().get(DEV_INBOX_PATH, dev_inbox)))
            .dev_panel(panel)
    }
}

async fn dev_inbox(req: Request) -> Response {
    // Like the dev console: only for requests from this machine.
    if !crate::dev::local(&req) {
        return http::StatusCode::NOT_FOUND.into_response();
    }
    let Some(mail) = req.state::<Mailer>().cloned() else {
        return http::StatusCode::NOT_FOUND.into_response();
    };
    let mut page = String::from(
        "<!doctype html><meta charset=utf-8><title>Dev inbox</title>\
         <style>body{font:15px/1.5 system-ui,sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem}\
         article{border:1px solid #8884;border-radius:8px;padding:1rem;margin:1rem 0}\
         pre{white-space:pre-wrap}iframe{width:100%;border:1px solid #8884;min-height:12rem}\
         small{color:#888}</style><h1>Dev inbox</h1>\
         <p><small>Mail sent by this app while <code>SMTP_URL</code> is unset. Newest first; \
         the last 50 are kept until restart.</small></p>",
    );
    let sent = mail.sent();
    if sent.is_empty() {
        page.push_str("<p>Nothing sent yet.</p>");
    }
    for email in sent.iter().rev() {
        page.push_str("<article><h2>");
        page.push_str(&escape(&email.subject));
        page.push_str("</h2><p><small>to ");
        page.push_str(&escape(&email.to.join(", ")));
        if let Some(from) = &email.from {
            page.push_str(" · from ");
            page.push_str(&escape(from));
        }
        page.push_str("</small></p>");
        if let Some(html) = &email.html {
            page.push_str("<iframe sandbox srcdoc=\"");
            page.push_str(&escape(html));
            page.push_str("\"></iframe>");
        }
        if let Some(text) = &email.text {
            page.push_str("<pre>");
            page.push_str(&linkify(&escape(text)));
            page.push_str("</pre>");
        }
        page.push_str("</article>");
    }
    Html(page).into_response()
}

/// Turn `http(s)://` URLs in already-escaped text into links, so reset and
/// confirmation links in the dev inbox are one click away.
fn linkify(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let mut rest = escaped;
    while let Some(start) = [rest.find("http://"), rest.find("https://")]
        .into_iter()
        .flatten()
        .min()
    {
        out.push_str(&rest[..start]);
        let url_len = rest[start..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - start);
        let url = &rest[start..start + url_len];
        out.push_str("<a href=\"");
        out.push_str(url);
        out.push_str("\">");
        out.push_str(url);
        out.push_str("</a>");
        rest = &rest[start + url_len..];
    }
    out.push_str(rest);
    out
}

#[cfg(all(feature = "json", any(feature = "postgres", feature = "sqlite")))]
impl crate::jobs::Job for Email {
    const NAME: &'static str = "zapline.email";

    async fn run(self, res: crate::Resources) -> Result<()> {
        res.require::<Mailer>()?.send(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linkify_wraps_urls() {
        assert_eq!(
            linkify("reset: https://x.test/r?t=1&amp;u=2 now"),
            "reset: <a href=\"https://x.test/r?t=1&amp;u=2\">https://x.test/r?t=1&amp;u=2</a> now"
        );
        assert_eq!(linkify("no links"), "no links");
    }

    #[test]
    fn messages_need_valid_addresses() {
        let err = Email::to("not an address")
            .to_message("a@b.test")
            .unwrap_err();
        assert!(err.message().contains("invalid email address"));
        assert!(Email::to("Ada <ada@x.test>")
            .text("hi")
            .to_message("a@b.test")
            .is_ok());
    }
}
