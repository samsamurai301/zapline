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
//!             .cc("team@example.com")
//!             .subject("You're invited")
//!             .text("Join us at https://example.com")
//!             .html("<p>Join us at <a href=\"https://example.com\">example.com</a></p>")
//!             .attach("agenda.pdf", "application/pdf", std::fs::read("agenda.pdf")?),
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
//! With the `views` feature, [`Email::template`] renders the subject and
//! both bodies from `templates/mail/<name>.html` and `.txt`.
//!
//! In tests, use [`Mailer::outbox`] and read what was sent with
//! [`Mailer::sent`].
//!
//! With a database registered, [`Email`] is also a [`Job`](crate::jobs::Job):
//! register it with `.jobs(|q| q.register::<Email>())` and
//! `jobs.enqueue(email)` sends it in the background, retrying if the SMTP
//! server is down. Attachments travel with it.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{Attachment as Part, Mailbox, MultiPart, SinglePart};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};

use crate::dev::{escape, internal, DevPanel};
use crate::extract::FromRequestParts;
use crate::response::{IntoResponse, Response};
use crate::{header, App, Error, Html, Request, Result, Routes};

/// How many messages the development outbox keeps.
const OUTBOX_SIZE: usize = 50;

/// The path of the development inbox page.
pub const DEV_INBOX_PATH: &str = "/_dev/mail";

/// The most attachment bytes one [`Email`] may carry (10 MiB, before the
/// base64 encoding that makes them a third larger on the wire). Most mail
/// providers refuse messages much past that; link to a
/// [stored file](crate::storage) instead.
pub const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

/// Headers [`Email::header`] can't set: they have their own methods, or
/// the message structure depends on them.
const RESERVED_HEADERS: &[&str] = &[
    "bcc",
    "cc",
    "content-disposition",
    "content-id",
    "content-transfer-encoding",
    "content-type",
    "date",
    "from",
    "message-id",
    "mime-version",
    "reply-to",
    "sender",
    "subject",
    "to",
];

/// One email message. Build it with [`Email::to`].
///
/// ```
/// use zapline::mail::Email;
///
/// let email = Email::to("ada@example.com")
///     .and_to("grace@example.com")
///     .cc("team@example.com")
///     .bcc("audit@example.com")
///     .header("List-Unsubscribe", "<https://example.com/unsubscribe>")
///     .subject("Hello")
///     .text("plain-text body")
///     .html("<p>HTML body</p>")
///     .attach("notes.txt", "text/plain", "see you there");
/// assert_eq!(email.to.len(), 2);
/// assert_eq!(email.attachments[0].filename, "notes.txt");
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Email {
    /// Recipients (`ada@example.com` or `Ada <ada@example.com>`).
    pub to: Vec<String>,
    /// Copied recipients, visible to everyone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<String>,
    /// Blind copies: they receive the message, but no recipient (and no
    /// header of the sent message) shows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<String>,
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
    /// Extra headers, in order (`List-Unsubscribe`, `X-Entity-Ref-ID`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Attached files and inline images, in order. Stored base64-encoded
    /// when the email is serialized (as a [job](crate::jobs) payload).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_attachments"
    )]
    pub attachments: Vec<Attachment>,
}

/// A file attached to an [`Email`] with [`Email::attach`], or an inline
/// image added with [`Email::inline`].
///
/// ```
/// use zapline::mail::Email;
///
/// let email = Email::to("ada@example.com").attach("a.csv", "text/csv", "id,name\n1,Ada\n");
/// let file = &email.attachments[0];
/// assert_eq!((file.filename.as_str(), file.content_type.as_str()), ("a.csv", "text/csv"));
/// assert_eq!(file.content_id, None);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// The file name shown to the recipient.
    pub filename: String,
    /// Its MIME type, e.g. `application/pdf`.
    pub content_type: String,
    /// For inline parts: the id the HTML refers to as `cid:<id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// The file's bytes.
    #[serde(with = "base64_bytes")]
    pub data: Bytes,
}

impl Email {
    /// A message to `address`.
    ///
    /// ```
    /// let email = zapline::mail::Email::to("ada@example.com");
    /// assert_eq!(email.to, ["ada@example.com"]);
    /// ```
    pub fn to(address: impl Into<String>) -> Self {
        Email {
            to: vec![address.into()],
            ..Email::default()
        }
    }

    /// Add another recipient.
    ///
    /// ```
    /// let email = zapline::mail::Email::to("ada@example.com").and_to("grace@example.com");
    /// assert_eq!(email.to.len(), 2);
    /// ```
    pub fn and_to(mut self, address: impl Into<String>) -> Self {
        self.to.push(address.into());
        self
    }

    /// Add a copied (`Cc`) recipient; call again for more.
    ///
    /// ```
    /// let email = zapline::mail::Email::to("ada@example.com").cc("team@example.com");
    /// assert_eq!(email.cc, ["team@example.com"]);
    /// ```
    pub fn cc(mut self, address: impl Into<String>) -> Self {
        self.cc.push(address.into());
        self
    }

    /// Add a blind-copied recipient: the message is delivered to it, but
    /// the sent headers (and the other recipients) never show it.
    ///
    /// ```
    /// let email = zapline::mail::Email::to("ada@example.com").bcc("audit@example.com");
    /// assert_eq!(email.bcc, ["audit@example.com"]);
    /// ```
    pub fn bcc(mut self, address: impl Into<String>) -> Self {
        self.bcc.push(address.into());
        self
    }

    /// Set the sender (instead of the mailer's default).
    ///
    /// ```
    /// let email = zapline::mail::Email::to("ada@example.com").from("Acme <hi@acme.test>");
    /// ```
    pub fn from(mut self, address: impl Into<String>) -> Self {
        self.from = Some(address.into());
        self
    }

    /// Set the reply-to address.
    ///
    /// ```
    /// let email = zapline::mail::Email::to("ada@example.com").reply_to("support@acme.test");
    /// ```
    pub fn reply_to(mut self, address: impl Into<String>) -> Self {
        self.reply_to = Some(address.into());
        self
    }

    /// Set the subject.
    ///
    /// ```
    /// let email = zapline::mail::Email::to("ada@example.com").subject("Hi");
    /// ```
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = subject.into();
        self
    }

    /// Set the plain-text body.
    ///
    /// ```
    /// let email = zapline::mail::Email::to("ada@example.com").text("Hello!");
    /// ```
    pub fn text(mut self, body: impl Into<String>) -> Self {
        self.text = Some(body.into());
        self
    }

    /// Set the HTML body (for example a rendered [view](crate::view), or
    /// see [`template`](Self::template)).
    ///
    /// ```
    /// let email = zapline::mail::Email::to("ada@example.com").html("<p>Hello!</p>");
    /// ```
    pub fn html(mut self, body: impl Into<String>) -> Self {
        self.html = Some(body.into());
        self
    }

    /// Add a header such as `List-Unsubscribe`. Sending fails for a name
    /// that isn't a valid header name, a value with a line break (header
    /// injection), or a header that has its own method (`To`, `Subject`,
    /// `Content-Type`, ...).
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// use zapline::mail::{Email, Mailer};
    ///
    /// let mailer = Mailer::outbox();
    /// let ok = Email::to("ada@example.com").header("X-Campaign", "spring").text("hi");
    /// assert!(mailer.send(ok).await.is_ok());
    /// let injected = Email::to("ada@example.com").header("X-Note", "a\r\nBcc: eve@example.com");
    /// assert!(mailer.send(injected).await.is_err());
    /// # }
    /// ```
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Attach a file. All attachments together may be at most
    /// [`MAX_ATTACHMENT_BYTES`]; sending (or enqueueing) a larger email
    /// fails with an error that says so.
    ///
    /// ```
    /// use zapline::mail::Email;
    ///
    /// let pdf = vec![b'%', b'P', b'D', b'F'];
    /// let email = Email::to("ada@example.com")
    ///     .text("Your invoice is attached.")
    ///     .attach("invoice-42.pdf", "application/pdf", pdf);
    /// ```
    pub fn attach(
        mut self,
        filename: impl Into<String>,
        content_type: impl Into<String>,
        data: impl Into<Bytes>,
    ) -> Self {
        self.attachments.push(Attachment {
            filename: filename.into(),
            content_type: content_type.into(),
            content_id: None,
            data: data.into(),
        });
        self
    }

    /// Embed an image the HTML body shows with `<img src="cid:{cid}">`
    /// (logos, charts). Counts towards [`MAX_ATTACHMENT_BYTES`].
    ///
    /// ```
    /// use zapline::mail::Email;
    ///
    /// let png = vec![0x89, b'P', b'N', b'G'];
    /// let email = Email::to("ada@example.com")
    ///     .html(r#"<img src="cid:logo" alt="Acme"> Welcome!"#)
    ///     .inline("logo", "image/png", png);
    /// assert_eq!(email.attachments[0].content_id.as_deref(), Some("logo"));
    /// ```
    pub fn inline(
        mut self,
        cid: impl Into<String>,
        content_type: impl Into<String>,
        data: impl Into<Bytes>,
    ) -> Self {
        let cid = cid.into();
        self.attachments.push(Attachment {
            filename: cid.clone(),
            content_type: content_type.into(),
            content_id: Some(cid),
            data: data.into(),
        });
        self
    }

    /// Render the bodies (and the subject) from templates: `{name}.html`
    /// and `{name}.txt` in the app's [`Views`](crate::view::Views)
    /// directory, with `ctx`. Either file may be missing, not both; without
    /// a `.txt`, the text body is made from the HTML. The subject is the
    /// `{% block subject %}` of the HTML template (put it in `<title>`), or
    /// of the text one; without either, the subject set before is kept.
    ///
    /// `templates/mail/welcome.html`:
    ///
    /// ```jinja
    /// <title>{% block subject %}Welcome, {{ name }}{% endblock %}</title>
    /// <p>Hi {{ name }}, <a href="{{ url }}">confirm your address</a>.</p>
    /// ```
    ///
    /// ```no_run
    /// use zapline::mail::{Email, Mailer};
    /// use zapline::prelude::*;
    /// use zapline::view::{context, Views};
    ///
    /// #[post("/signup")]
    /// async fn signup(views: Views, mail: Mailer) -> Result<&'static str> {
    ///     let url = "https://app.test/confirm?t=abc";
    ///     let email = Email::to("ada@example.com")
    ///         .template(&views, "mail/welcome", context! { name => "Ada", url })?;
    ///     mail.send(email).await?;
    ///     Ok("check your inbox")
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// When neither template exists or rendering fails.
    #[cfg(feature = "views")]
    pub fn template(
        mut self,
        views: &crate::view::Views,
        name: &str,
        ctx: impl Serialize,
    ) -> Result<Self> {
        let (subject, html, text) = views.render_mail(name, ctx)?;
        if let Some(subject) = subject {
            self.subject = html_to_text(&subject)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
        }
        self.text = match (text, &html) {
            (Some(text), _) => Some(text.trim().to_owned() + "\n"),
            (None, Some(html)) => Some(html_to_text(html) + "\n"),
            (None, None) => None,
        };
        self.html = html;
        Ok(self)
    }

    /// The message as sent over SMTP (RFC 5322 with MIME parts), for
    /// saving as an `.eml` file or inspecting in tests. `default_from` is
    /// the sender when the email doesn't set one. Blind copies are not in
    /// it: they only go to the SMTP envelope.
    ///
    /// ```
    /// use zapline::mail::Email;
    ///
    /// let raw = Email::to("ada@example.com")
    ///     .cc("team@example.com")
    ///     .bcc("audit@example.com")
    ///     .text("hi")
    ///     .to_mime("app@example.com")?;
    /// let raw = String::from_utf8(raw).unwrap();
    /// assert!(raw.contains("Cc: team@example.com"));
    /// assert!(!raw.contains("audit@example.com"));
    /// # Ok::<(), zapline::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Mailer::send`] would, before trying to send.
    pub fn to_mime(&self, default_from: &str) -> Result<Vec<u8>> {
        Ok(self.to_message(default_from)?.formatted())
    }

    fn to_message(&self, default_from: &str) -> Result<Message> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err(Error::internal("an email needs at least one recipient"));
        }
        check_attachment_size(&self.attachments).map_err(Error::internal)?;
        let from = self.from.as_deref().unwrap_or(default_from);
        let mut builder = Message::builder()
            .from(mailbox(from)?)
            .subject(self.subject.clone());
        for to in &self.to {
            builder = builder.to(mailbox(to)?);
        }
        for cc in &self.cc {
            builder = builder.cc(mailbox(cc)?);
        }
        for bcc in &self.bcc {
            builder = builder.bcc(mailbox(bcc)?);
        }
        if let Some(reply_to) = &self.reply_to {
            builder = builder.reply_to(mailbox(reply_to)?);
        }
        for (name, value) in &self.headers {
            builder = builder.raw_header(custom_header(name, value)?);
        }

        // multipart/mixed { multipart/related { body, inline images }, files }
        let mut body = match (&self.text, &self.html) {
            (Some(text), Some(html)) => Body::Multi(MultiPart::alternative_plain_html(
                text.clone(),
                html.clone(),
            )),
            (None, Some(html)) => Body::Single(
                SinglePart::builder()
                    .header(ContentType::TEXT_HTML)
                    .body(html.clone()),
            ),
            (text, None) => Body::Single(
                SinglePart::builder()
                    .header(ContentType::TEXT_PLAIN)
                    .body(text.clone().unwrap_or_default()),
            ),
        };
        let (inline, files): (Vec<_>, Vec<_>) = self
            .attachments
            .iter()
            .partition(|a| a.content_id.is_some());
        if !inline.is_empty() {
            let mut related = body.into_multipart(MultiPart::related());
            for image in inline {
                related = related.singlepart(image.to_part()?);
            }
            body = Body::Multi(related);
        }
        if !files.is_empty() {
            let mut mixed = body.into_multipart(MultiPart::mixed());
            for file in files {
                mixed = mixed.singlepart(file.to_part()?);
            }
            body = Body::Multi(mixed);
        }
        let built = match body {
            Body::Single(part) => builder.singlepart(part),
            Body::Multi(parts) => builder.multipart(parts),
        };
        built.map_err(|e| Error::internal(format!("cannot build the email: {e}")))
    }

    /// Everyone the message goes to, for logs and the dev inbox.
    fn recipients(&self) -> String {
        let mut all = self.to.clone();
        all.extend(self.cc.iter().cloned());
        all.join(", ")
    }
}

enum Body {
    Single(SinglePart),
    Multi(MultiPart),
}

impl Body {
    fn into_multipart(self, builder: lettre::message::MultiPartBuilder) -> MultiPart {
        match self {
            Body::Single(part) => builder.singlepart(part),
            Body::Multi(parts) => builder.multipart(parts),
        }
    }
}

impl Attachment {
    fn to_part(&self) -> Result<SinglePart> {
        let content_type = ContentType::parse(&self.content_type).map_err(|e| {
            Error::internal(format!(
                "invalid content type `{}` for attachment `{}`: {e}",
                self.content_type, self.filename
            ))
        })?;
        if self.filename.is_empty() || self.filename.chars().any(char::is_control) {
            return Err(Error::internal(format!(
                "invalid attachment file name {:?}",
                self.filename
            )));
        }
        let part = match &self.content_id {
            Some(cid) => {
                let valid = !cid.is_empty()
                    && cid
                        .bytes()
                        .all(|b| b.is_ascii_graphic() && !matches!(b, b'<' | b'>' | b'"'));
                if !valid {
                    return Err(Error::internal(format!(
                        "invalid inline content id {cid:?}: use letters, digits and `.-_@`"
                    )));
                }
                Part::new_inline(cid.clone())
            }
            None => Part::new(self.filename.clone()),
        };
        Ok(part.body(self.data.to_vec(), content_type))
    }
}

fn mailbox(address: &str) -> Result<Mailbox> {
    address
        .parse()
        .map_err(|e| Error::internal(format!("invalid email address `{address}`: {e}")))
}

fn custom_header(name: &str, value: &str) -> Result<HeaderValue> {
    let valid_name = !name.is_empty()
        && name.len() <= 76
        && name.bytes().all(|b| b.is_ascii_graphic() && b != b':');
    if !valid_name {
        return Err(Error::internal(format!(
            "invalid email header name {name:?}"
        )));
    }
    if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(Error::internal(format!(
            "the `{name}` header can't be set with `Email::header`: use its own method"
        )));
    }
    if value.chars().any(|c| c.is_control() && c != '\t') {
        return Err(Error::internal(format!(
            "the value of email header `{name}` contains a line break or control character"
        )));
    }
    let name = HeaderName::new_from_ascii(name.to_owned())
        .map_err(|_| Error::internal(format!("invalid email header name {name:?}")))?;
    Ok(HeaderValue::new(name, value.to_owned()))
}

fn check_attachment_size(attachments: &[Attachment]) -> std::result::Result<(), String> {
    let total: usize = attachments.iter().map(|a| a.data.len()).sum();
    if total > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "the email's attachments total {total} bytes, over the limit of {MAX_ATTACHMENT_BYTES} \
             (zapline::mail::MAX_ATTACHMENT_BYTES): send a link to a stored file instead"
        ));
    }
    Ok(())
}

fn serialize_attachments<S: serde::Serializer>(
    attachments: &[Attachment],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    // Refuse to enqueue what could never be sent.
    check_attachment_size(attachments).map_err(serde::ser::Error::custom)?;
    serializer.collect_seq(attachments)
}

mod base64_bytes {
    use base64::prelude::{Engine as _, BASE64_STANDARD};
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(data: &Bytes, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&BASE64_STANDARD.encode(data))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        BASE64_STANDARD
            .decode(text.as_bytes())
            .map(Bytes::from)
            .map_err(serde::de::Error::custom)
    }
}

/// Plain text from an HTML email body: paragraphs and line breaks kept,
/// links as `label (url)`, lists as `- item`, entities decoded, `<head>`,
/// `<style>` and `<script>` dropped.
#[cfg_attr(not(feature = "views"), allow(dead_code))]
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    // (href, where the link text starts in `out`)
    let mut link: Option<(String, usize)> = None;
    let mut pre = 0usize;
    let mut rest = html;
    while !rest.is_empty() {
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        let is_tag = rest.starts_with('<')
            && rest[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '/' || c == '!');
        if is_tag {
            let end = tag_end(rest);
            let tag = &rest[1..end.saturating_sub(1).max(1)];
            rest = &rest[end..];
            let closing = tag.starts_with('/');
            let tag = tag.trim_start_matches('/');
            let name_len = tag
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .unwrap_or(tag.len());
            let name = tag[..name_len].to_ascii_lowercase();
            if !closing && matches!(name.as_str(), "head" | "style" | "script" | "title") {
                // Skip to the matching close tag.
                let close = format!("</{name}");
                rest = find_ascii_ci(rest, &close).map_or("", |i| {
                    let after = &rest[i..];
                    &after[tag_end(after)..]
                });
                continue;
            }
            match (name.as_str(), closing) {
                ("br", _) => line_breaks(&mut out, 1, true),
                ("p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "table" | "ul" | "ol", _)
                | ("blockquote" | "hr", _) => line_breaks(&mut out, 2, false),
                ("div" | "tr" | "section" | "article" | "header" | "footer", _) => {
                    line_breaks(&mut out, 1, false)
                }
                ("li", false) => {
                    line_breaks(&mut out, 1, false);
                    out.push_str("- ");
                }
                ("td" | "th", true) => out.push_str("  "),
                ("pre", false) => {
                    line_breaks(&mut out, 2, false);
                    pre += 1;
                }
                ("pre", true) => {
                    pre = pre.saturating_sub(1);
                    line_breaks(&mut out, 2, false);
                }
                ("a", false) => link = attribute(tag, "href").map(|href| (href, out.len())),
                ("a", true) => {
                    if let Some((href, start)) = link.take() {
                        let label = out.get(start..).unwrap_or("").trim();
                        let shown = href.strip_prefix("mailto:").unwrap_or(&href);
                        let useful = href.starts_with("http") || href.starts_with("mailto:");
                        if useful && label != shown {
                            if label.is_empty() {
                                out.push_str(shown);
                            } else {
                                out.push_str(" (");
                                out.push_str(shown);
                                out.push(')');
                            }
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let next = rest[1..].find('<').map_or(rest.len(), |i| i + 1);
        let text = decode_entities(&rest[..next]);
        rest = &rest[next..];
        for c in text.chars() {
            if pre > 0 {
                out.push(c);
            } else if c.is_whitespace() && c != '\u{a0}' {
                if !out.is_empty() && !out.ends_with([' ', '\n']) {
                    out.push(' ');
                }
            } else {
                out.push(if c == '\u{a0}' { ' ' } else { c });
            }
        }
    }
    // Tidy: no trailing spaces, at most one blank line in a row.
    let mut tidy = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 || tidy.is_empty() {
                continue;
            }
        } else {
            blank = 0;
        }
        tidy.push_str(line);
        tidy.push('\n');
    }
    tidy.trim_end().to_owned()
}

/// End line(s): `n` newlines at the end of `out` (a forced `<br>` always
/// adds one).
fn line_breaks(out: &mut String, n: usize, force: bool) {
    while out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        return;
    }
    if force {
        out.push('\n');
        return;
    }
    let have = out.len() - out.trim_end_matches('\n').len();
    for _ in have..n {
        out.push('\n');
    }
}

/// Index just past the `>` closing the tag that starts `s`, skipping quoted
/// attribute values.
fn tag_end(s: &str) -> usize {
    let mut quote = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return i + 1,
            _ => {}
        }
    }
    s.len()
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let n = needle.len();
    (0..haystack.len().saturating_sub(n - 1)).find(|&i| {
        haystack
            .as_bytes()
            .get(i..i + n)
            .is_some_and(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    })
}

/// The value of attribute `name` in the inside of a tag (`a href="x"`).
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before_ok = at > 0 && lower.as_bytes()[at - 1].is_ascii_whitespace();
        let after = tag[from..].trim_start();
        if !before_ok || !after.starts_with('=') {
            continue;
        }
        let value = after[1..].trim_start();
        let value = match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or(""),
            _ => value.split(char::is_whitespace).next().unwrap_or(""),
        };
        return Some(decode_entities(value));
    }
    None
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest[1..]
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| {
                let entity = &rest[1..=end];
                let c = match entity {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" | "#39" => '\'',
                    "nbsp" => '\u{a0}',
                    "copy" => '©',
                    "reg" => '®',
                    "trade" => '™',
                    "hellip" => '…',
                    "mdash" => '—',
                    "ndash" => '–',
                    "lsquo" => '‘',
                    "rsquo" => '’',
                    "ldquo" => '“',
                    "rdquo" => '”',
                    "euro" => '€',
                    "middot" => '·',
                    "bull" => '•',
                    _ => {
                        let num = entity.strip_prefix('#')?;
                        let code = match num.strip_prefix(['x', 'X']) {
                            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                            None => num.parse().ok()?,
                        };
                        char::from_u32(code)?
                    }
                };
                Some((c, end + 2))
            });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
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
    /// Kept messages with their ids (for links in the dev inbox).
    outbox: Mutex<Outbox>,
}

#[derive(Default)]
struct Outbox {
    next_id: u64,
    kept: VecDeque<(u64, Email)>,
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
                outbox: Mutex::default(),
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
                if outbox.kept.len() == OUTBOX_SIZE {
                    outbox.kept.pop_front();
                }
                outbox.next_id += 1;
                let id = outbox.next_id;
                outbox.kept.push_back((id, email));
            }
        }
        Ok(())
    }

    /// Messages kept by an [outbox](Self::outbox) mailer, oldest first
    /// (the last 50). Empty for SMTP.
    pub fn sent(&self) -> Vec<Email> {
        self.kept().into_iter().map(|(_, email)| email).collect()
    }

    fn kept(&self) -> Vec<(u64, Email)> {
        let outbox = self.inner.outbox.lock().unwrap_or_else(|e| e.into_inner());
        outbox.kept.iter().cloned().collect()
    }

    fn kept_one(&self, id: u64) -> Option<Email> {
        let outbox = self.inner.outbox.lock().unwrap_or_else(|e| e.into_inner());
        outbox
            .kept
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, email)| email.clone())
    }
}

#[cfg(feature = "tracing")]
fn log_outbox(email: &Email) {
    tracing::info!(
        to = %email.recipients(),
        attachments = email.attachments.len(),
        subject = %email.subject,
        "email kept in the dev outbox ({DEV_INBOX_PATH})"
    );
}

#[cfg(not(feature = "tracing"))]
fn log_outbox(email: &Email) {
    eprintln!(
        "[zapline] email to {} ({}) kept in the dev outbox ({DEV_INBOX_PATH})",
        email.recipients(),
        email.subject
    );
}

impl FromRequestParts for Mailer {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Mailer>().cloned().ok_or_else(|| {
            Error::internal("no mailer registered: add `.mailer(Mailer::from_env()?)` to the app")
                .diagnosed("ZL0070", None)
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
        app.mount(internal(Routes::new().get(DEV_INBOX_PATH, dev_inbox).get(
            &format!("{DEV_INBOX_PATH}/{{id}}/{{part}}"),
            dev_inbox_part,
        )))
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
         small{color:#888}.views>input{position:absolute;opacity:0}\
         .views>label{display:inline-block;padding:.1rem .7rem;border:1px solid #8884;\
         border-radius:6px;margin:0 .3rem .5rem 0;cursor:pointer}\
         .views>input:checked+label{background:#8883}\
         .views>input:focus-visible+label{outline:2px solid}.views>.text{display:none}\
         .views>.t:checked~.text{display:block}.views>.t:checked~.html{display:none}\
         ul.files{padding-left:1.2rem}</style><h1>Dev inbox</h1>\
         <p><small>Mail sent by this app while <code>SMTP_URL</code> is unset. Newest first; \
         the last 50 are kept until restart.</small></p>",
    );
    let kept = mail.kept();
    if kept.is_empty() {
        page.push_str("<p>Nothing sent yet.</p>");
    }
    for (id, email) in kept.iter().rev() {
        let part_url = |n: usize| format!("{DEV_INBOX_PATH}/{id}/{n}");
        page.push_str("<article><h2>");
        page.push_str(&escape(&email.subject));
        page.push_str("</h2><p><small>to ");
        page.push_str(&escape(&email.to.join(", ")));
        if !email.cc.is_empty() {
            page.push_str(" · cc ");
            page.push_str(&escape(&email.cc.join(", ")));
        }
        if let Some(from) = &email.from {
            page.push_str(" · from ");
            page.push_str(&escape(from));
        }
        page.push_str(&format!(
            " · <a href=\"{DEV_INBOX_PATH}/{id}/raw\">.eml</a>"
        ));
        if !email.bcc.is_empty() {
            page.push_str("<br>bcc, not in the sent headers: ");
            page.push_str(&escape(&email.bcc.join(", ")));
        }
        page.push_str("</small></p>");

        // Inline images: point `cid:` references at the part's URL.
        let html = email.html.as_ref().map(|html| {
            let mut html = html.clone();
            for (n, a) in email.attachments.iter().enumerate() {
                if let Some(cid) = &a.content_id {
                    html = html.replace(&format!("cid:{cid}"), &part_url(n));
                }
            }
            html
        });
        let both = html.is_some() && email.text.is_some();
        if both {
            page.push_str(&format!(
                "<div class=views><input type=radio name=v{id} id=v{id}h class=h checked>\
                 <label for=v{id}h>HTML</label><input type=radio name=v{id} id=v{id}t class=t>\
                 <label for=v{id}t>Text</label>"
            ));
        }
        if let Some(html) = &html {
            page.push_str("<iframe class=html sandbox srcdoc=\"");
            page.push_str(&escape(html));
            page.push_str("\"></iframe>");
        }
        if let Some(text) = &email.text {
            page.push_str("<pre class=text>");
            page.push_str(&linkify(&escape(text)));
            page.push_str("</pre>");
        }
        if both {
            page.push_str("</div>");
        }
        if !email.attachments.is_empty() {
            page.push_str("<ul class=files>");
            for (n, a) in email.attachments.iter().enumerate() {
                page.push_str(&format!(
                    "<li><a href=\"{}\" download=\"{}\">{}</a> <small>{} · {}{}</small></li>",
                    part_url(n),
                    escape(&a.filename),
                    escape(&a.filename),
                    escape(&a.content_type),
                    human_size(a.data.len()),
                    if a.content_id.is_some() {
                        " · inline"
                    } else {
                        ""
                    },
                ));
            }
            page.push_str("</ul>");
        }
        page.push_str("</article>");
    }
    Html(page).into_response()
}

/// `/_dev/mail/{id}/{n}`: attachment `n` of a kept email;
/// `/_dev/mail/{id}/raw`: the whole message as an `.eml` file.
async fn dev_inbox_part(req: Request) -> Response {
    if !crate::dev::local(&req) {
        return http::StatusCode::NOT_FOUND.into_response();
    }
    let found = req.state::<Mailer>().and_then(|mail| {
        let id = req.param("id")?.parse().ok()?;
        Some((mail.kept_one(id)?, mail.inner.from.clone(), id))
    });
    let Some((email, from, id)) = found else {
        return http::StatusCode::NOT_FOUND.into_response();
    };
    let part = req.param("part").unwrap_or_default();
    let (bytes, content_type, filename, inline) = if part == "raw" {
        match email.to_mime(&from) {
            Ok(raw) => (
                Bytes::from(raw),
                "message/rfc822".to_owned(),
                format!("email-{id}.eml"),
                false,
            ),
            Err(e) => return e.into_response(),
        }
    } else {
        let Some(a) = part
            .parse::<usize>()
            .ok()
            .and_then(|n| email.attachments.get(n))
        else {
            return http::StatusCode::NOT_FOUND.into_response();
        };
        // Only raster images show inline (the HTML preview's `cid:` images).
        let image = matches!(
            a.content_type.to_ascii_lowercase().as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        );
        (
            a.data.clone(),
            a.content_type.clone(),
            a.filename.clone(),
            image,
        )
    };
    let mut res = bytes.into_response();
    let headers = res.headers_mut();
    let content_type = http::HeaderValue::from_str(&content_type)
        .unwrap_or(http::HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_TYPE, content_type);
    let safe_name: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let disposition = format!(
        "{}; filename=\"{safe_name}\"",
        if inline { "inline" } else { "attachment" }
    );
    if let Ok(value) = http::HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        http::HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        http::HeaderValue::from_static("nosniff"),
    );
    res
}

fn human_size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
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

    #[test]
    fn html_becomes_readable_text() {
        let html = "<!doctype html><html><head><title>Hi</title><style>p{color:red}</style></head>\
            <body><h1>Welcome,   Ada &amp; co</h1><!-- hidden --><p>Click\n <a href=\"https://x.test/c?a=1&amp;b=2\">here</a> \
            or <a href=\"https://x.test/\">https://x.test/</a>.<br>Thanks</p>\
            <ul><li>one</li><li>two</li></ul><p>a < b &#8212; &#x41;&nbsp;&unknown;</p>\
            <a href=\"mailto:help@x.test\">help@x.test</a><script>alert(1)</script></body></html>";
        assert_eq!(
            html_to_text(html),
            "Welcome, Ada & co\n\nClick here (https://x.test/c?a=1&b=2) or https://x.test/.\n\
             Thanks\n\n- one\n- two\n\na < b — A &unknown;\n\nhelp@x.test"
        );
        assert_eq!(html_to_text("plain"), "plain");
        assert_eq!(html_to_text(""), "");
        assert_eq!(html_to_text("<p>unclosed <b"), "unclosed");
    }

    #[test]
    fn headers_are_checked() {
        let send = |name: &str, value: &str| {
            Email::to("a@x.test")
                .header(name, value)
                .to_message("b@x.test")
                .map(|m| String::from_utf8(m.formatted()).unwrap())
        };
        let raw = send("List-Unsubscribe", "<https://x.test/u>").unwrap();
        assert!(
            raw.contains("List-Unsubscribe: <https://x.test/u>\r\n"),
            "{raw}"
        );
        for (name, value) in [
            ("X-A", "line\r\nBcc: eve@x.test"),
            ("X-A", "line\nbreak"),
            ("Bad Name", "x"),
            ("Bad:Name", "x"),
            ("", "x"),
            ("Subject", "x"),
            ("content-type", "text/html"),
        ] {
            assert!(send(name, value).is_err(), "{name:?}: {value:?}");
        }
    }

    #[test]
    fn oversized_attachments_are_refused_before_sending_or_enqueueing() {
        let big = Email::to("a@x.test").attach(
            "big.bin",
            "application/octet-stream",
            vec![0; MAX_ATTACHMENT_BYTES + 1],
        );
        let err = big.to_message("b@x.test").unwrap_err();
        assert!(
            err.message().contains("over the limit"),
            "{}",
            err.message()
        );
        #[cfg(feature = "json")]
        {
            let err = serde_json::to_string(&big).unwrap_err();
            assert!(err.to_string().contains("MAX_ATTACHMENT_BYTES"), "{err}");
        }
    }
}
