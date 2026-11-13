//! Calling large language models (feature `ai`): Anthropic's Claude through
//! the Messages API, or any OpenAI-compatible chat API (a local Ollama or
//! vLLM, other providers).
//!
//! [`Llm`] holds the provider, key and model; [`Llm::chat`] builds one
//! request. [`Chat::send`] waits for the whole [`Reply`]; [`Chat::stream`]
//! hands out text as it is generated, and [`TokenStream::into_sse`] relays it
//! to the browser as server-sent events.
//!
//! ```no_run
//! use zapline::ai::Llm;
//! use zapline::prelude::*;
//!
//! async fn summarize(llm: Llm, body: String) -> Result<String> {
//!     let reply = llm
//!         .chat()
//!         .system("Summarize the text in one sentence.")
//!         .user(body)
//!         .send()
//!         .await?;
//!     Ok(reply.text)
//! }
//!
//! async fn ask(llm: Llm, question: String) -> Result<impl IntoResponse> {
//!     Ok(llm.chat().user(question).stream().await?.into_sse())
//! }
//!
//! let app = App::new()
//!     .state(Llm::from_env().expect("ANTHROPIC_API_KEY"))
//!     .post("/summarize", summarize)
//!     .post("/ask", ask);
//! ```
//!
//! Errors are [`Error`]s a handler can return: `502` (code `llm_error`, or
//! the HTTP client's `upstream_*` codes), `504` (`upstream_timeout`), `429`
//! when the provider is rate limiting after the retries (`llm_rate_limited`).

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use http::{HeaderValue, StatusCode};
use serde_json::{json, Value};

use crate::extract::FromRequestParts;
use crate::outbound::{HttpClient, HttpResponse};
use crate::request::Request;
use crate::response::{IntoResponse, Response};
use crate::sse::{Event, Sse};
use crate::{Error, Result};

const ANTHROPIC_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2026-10-21";
/// The default Claude model.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
/// Models that accept `fallbacks: "default"` (server-side refusal fallback).
const FALLBACK_MODELS: [&str; 5] = [
    "claude-fable-5-2",
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Anthropic,
    OpenAi,
}

/// How hard a Claude model thinks before answering (`output_config.effort`):
/// lower is faster and cheaper, higher is more thorough.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Effort {
    /// Quick answers: chat, classification, simple extraction.
    Low,
    /// The default for Claude Opus 5.5.
    Medium,
    /// Thorough: most coding and multi-step work.
    High,
    /// More thorough still: hard coding and agentic tasks.
    XHigh,
    /// As much as the model can spend.
    Max,
}

impl Effort {
    fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// A language model: provider, API key and model. Cheap to clone. As a
/// handler argument it is the one registered with `App::state`.
#[derive(Clone)]
pub struct Llm {
    inner: Arc<Inner>,
}

#[derive(Clone)]
struct Inner {
    provider: Provider,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: Option<u32>,
    effort: Option<Effort>,
    fallbacks: bool,
    retries: u32,
    http: HttpClient,
}

impl fmt::Debug for Llm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Llm")
            .field("provider", &self.inner.provider)
            .field("base_url", &self.inner.base_url)
            .field("model", &self.inner.model)
            .field("api_key", &"[hidden]")
            .finish_non_exhaustive()
    }
}

impl Llm {
    fn new(provider: Provider, base_url: &str, api_key: String, model: &str) -> Self {
        Llm {
            inner: Arc::new(Inner {
                provider,
                base_url: base_url.trim_end_matches('/').to_owned(),
                api_key,
                model: model.to_owned(),
                max_tokens: None,
                effort: None,
                fallbacks: true,
                retries: 2,
                // Long generations: a generous wait for the first byte and
                // between streamed chunks. POSTs are never retried by the
                // HTTP client itself; `Llm` retries what is safe below.
                http: HttpClient::new()
                    .timeout(Duration::from_secs(600))
                    .retries(0),
            }),
        }
    }

    /// Claude through Anthropic's Messages API, with `api_key`. The model is
    /// [`DEFAULT_CLAUDE_MODEL`] (`claude-opus-5-5`) unless
    /// [`model`](Self::model) says otherwise.
    ///
    /// ```
    /// use zapline::ai::Llm;
    /// let llm = Llm::anthropic("sk-ant-...");
    /// assert_eq!(llm.model_name(), "claude-opus-5-5");
    /// ```
    pub fn anthropic(api_key: impl Into<String>) -> Self {
        Self::new(
            Provider::Anthropic,
            ANTHROPIC_URL,
            api_key.into(),
            DEFAULT_CLAUDE_MODEL,
        )
    }

    /// Any OpenAI-compatible chat completions API at `base_url` (the part
    /// before `/chat/completions`, e.g. `http://localhost:11434/v1` for
    /// Ollama), with `api_key` (sent as a bearer token; may be empty) and
    /// `model`.
    ///
    /// ```
    /// use zapline::ai::Llm;
    /// let llm = Llm::openai_compatible("http://localhost:11434/v1", "", "llama3.3");
    /// ```
    pub fn openai_compatible(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
        model: impl AsRef<str>,
    ) -> Self {
        Self::new(
            Provider::OpenAi,
            base_url.as_ref(),
            api_key.into(),
            model.as_ref(),
        )
    }

    /// Claude with the key in `ANTHROPIC_API_KEY`, and the model in
    /// `LLM_MODEL` when set.
    ///
    /// # Errors
    ///
    /// When `ANTHROPIC_API_KEY` is not set.
    ///
    /// ```no_run
    /// use zapline::ai::Llm;
    /// let llm = Llm::from_env()?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| Error::internal("ANTHROPIC_API_KEY is not set"))?;
        let llm = Self::anthropic(key);
        Ok(match std::env::var("LLM_MODEL") {
            Ok(model) if !model.is_empty() => llm.model(model),
            _ => llm,
        })
    }

    fn with(self, f: impl FnOnce(&mut Inner)) -> Self {
        let mut inner = (*self.inner).clone();
        f(&mut inner);
        Llm {
            inner: Arc::new(inner),
        }
    }

    /// Use `model` (e.g. `claude-sonnet-5-5`, `claude-haiku-5-5`).
    ///
    /// ```
    /// use zapline::ai::Llm;
    /// let llm = Llm::anthropic("key").model("claude-haiku-5-5");
    /// assert_eq!(llm.model_name(), "claude-haiku-5-5");
    /// ```
    #[must_use]
    pub fn model(self, model: impl Into<String>) -> Self {
        self.with(|i| i.model = model.into())
    }

    /// The model requests go to.
    pub fn model_name(&self) -> &str {
        &self.inner.model
    }

    /// Send requests to `url` instead of the provider's default endpoint
    /// (a gateway, a proxy, or a test double).
    ///
    /// ```
    /// use zapline::ai::Llm;
    /// let llm = Llm::anthropic("key").base_url("https://llm-gateway.internal");
    /// ```
    #[must_use]
    pub fn base_url(self, url: impl AsRef<str>) -> Self {
        let url = url.as_ref().trim_end_matches('/').to_owned();
        self.with(|i| i.base_url = url)
    }

    /// Use `http` for the calls, for example one that may reach a model
    /// server on your private network
    /// ([`allow_private_networks`](HttpClient::allow_private_networks)).
    ///
    /// ```
    /// use zapline::ai::Llm;
    /// use zapline::outbound::HttpClient;
    /// let local = Llm::openai_compatible("http://10.0.0.5:8000/v1", "", "qwen")
    ///     .http_client(HttpClient::new().allow_private_networks(true));
    /// ```
    #[must_use]
    pub fn http_client(self, http: HttpClient) -> Self {
        self.with(|i| i.http = http.retries(0))
    }

    /// The longest reply, in tokens (default 16 000, or 64 000 when
    /// streaming). Requests can override it.
    ///
    /// ```
    /// use zapline::ai::Llm;
    /// let llm = Llm::anthropic("key").max_tokens(4096);
    /// ```
    #[must_use]
    pub fn max_tokens(self, n: u32) -> Self {
        self.with(|i| i.max_tokens = Some(n))
    }

    /// The default [`Effort`] for Claude requests (the model's own default
    /// when unset: `medium` on Claude Opus 5.5).
    ///
    /// ```
    /// use zapline::ai::{Effort, Llm};
    /// let llm = Llm::anthropic("key").effort(Effort::Low);
    /// ```
    #[must_use]
    pub fn effort(self, effort: Effort) -> Self {
        self.with(|i| i.effort = Some(effort))
    }

    /// Whether Claude requests opt into server-side refusal fallback
    /// (default on, for the models that support it): when the model's
    /// safety classifiers decline a request, the API retries it on a
    /// fallback model within the same call. Turn it off to get
    /// [`StopReason::Refusal`] back instead.
    ///
    /// ```
    /// use zapline::ai::Llm;
    /// let llm = Llm::anthropic("key").refusal_fallbacks(false);
    /// ```
    #[must_use]
    pub fn refusal_fallbacks(self, enabled: bool) -> Self {
        self.with(|i| i.fallbacks = enabled)
    }

    /// How many times a request is retried after `429`, `500`, `502`,
    /// `503`, `504` or `529` before any output arrived (default 2; waits as
    /// `retry-after` says, or 1 s, 2 s, ...).
    ///
    /// ```
    /// use zapline::ai::Llm;
    /// let llm = Llm::anthropic("key").retries(5);
    /// ```
    #[must_use]
    pub fn retries(self, n: u32) -> Self {
        self.with(|i| i.retries = n)
    }

    /// Start a request.
    ///
    /// ```
    /// use zapline::ai::Llm;
    /// let chat = Llm::anthropic("key").chat().user("Hello");
    /// ```
    pub fn chat(&self) -> Chat {
        Chat {
            llm: self.clone(),
            system: None,
            messages: Vec::new(),
            max_tokens: self.inner.max_tokens,
            effort: self.inner.effort,
        }
    }
}

impl FromRequestParts for Llm {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.state::<Llm>().cloned().ok_or_else(|| {
            Error::internal(
                "no language model registered: add `.state(Llm::from_env()?)` to the app",
            )
        })
    }
}

/// Who wrote a message in the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The person (or your code) asking.
    User,
    /// The model's earlier answers.
    Assistant,
}

/// One request: a system prompt and the conversation so far.
#[derive(Clone, Debug)]
#[must_use = "a chat does nothing until it is sent"]
pub struct Chat {
    llm: Llm,
    system: Option<String>,
    messages: Vec<(Role, String)>,
    max_tokens: Option<u32>,
    effort: Option<Effort>,
}

impl Chat {
    /// Instructions for the model: its role, the output format, rules.
    pub fn system(mut self, text: impl Into<String>) -> Self {
        self.system = Some(text.into());
        self
    }

    /// A user message.
    pub fn user(mut self, text: impl Into<String>) -> Self {
        self.messages.push((Role::User, text.into()));
        self
    }

    /// An earlier answer from the model (for multi-turn conversations).
    pub fn assistant(mut self, text: impl Into<String>) -> Self {
        self.messages.push((Role::Assistant, text.into()));
        self
    }

    /// Append a whole conversation, oldest first.
    pub fn messages<I, S>(mut self, messages: I) -> Self
    where
        I: IntoIterator<Item = (Role, S)>,
        S: Into<String>,
    {
        self.messages
            .extend(messages.into_iter().map(|(r, t)| (r, t.into())));
        self
    }

    /// The longest reply, in tokens, for this request.
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// The [`Effort`] for this request (Claude only).
    pub fn effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }

    fn body(&self, stream: bool) -> Value {
        let inner = &self.llm.inner;
        let max_tokens = self
            .max_tokens
            .unwrap_or(if stream { 64_000 } else { 16_000 });
        let role = |r: Role| match r {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        match inner.provider {
            Provider::Anthropic => {
                let messages: Vec<Value> = self
                    .messages
                    .iter()
                    .map(|(r, t)| json!({"role": role(*r), "content": t}))
                    .collect();
                let mut body = json!({
                    "model": inner.model,
                    "max_tokens": max_tokens,
                    "messages": messages,
                });
                if let Some(system) = &self.system {
                    body["system"] = json!(system);
                }
                if let Some(effort) = self.effort {
                    body["output_config"] = json!({"effort": effort.as_str()});
                }
                if self.fallbacks() {
                    body["fallbacks"] = json!("default");
                }
                if stream {
                    body["stream"] = json!(true);
                }
                body
            }
            Provider::OpenAi => {
                let mut messages = Vec::new();
                if let Some(system) = &self.system {
                    messages.push(json!({"role": "system", "content": system}));
                }
                messages.extend(
                    self.messages
                        .iter()
                        .map(|(r, t)| json!({"role": role(*r), "content": t})),
                );
                let mut body = json!({
                    "model": inner.model,
                    "max_tokens": max_tokens,
                    "messages": messages,
                });
                if stream {
                    body["stream"] = json!(true);
                    body["stream_options"] = json!({"include_usage": true});
                }
                body
            }
        }
    }

    fn fallbacks(&self) -> bool {
        let inner = &self.llm.inner;
        inner.provider == Provider::Anthropic
            && inner.fallbacks
            && FALLBACK_MODELS.contains(&inner.model.as_str())
    }

    async fn post(&self, stream: bool) -> Result<HttpResponse> {
        if self.messages.is_empty() {
            return Err(Error::internal("a chat needs at least one message"));
        }
        let inner = &self.llm.inner;
        let body = self.body(stream);
        let mut attempt = 0u32;
        loop {
            let req = match inner.provider {
                Provider::Anthropic => {
                    let mut req = inner
                        .http
                        .post(&format!("{}/v1/messages", inner.base_url))
                        .header("x-api-key", &inner.api_key)
                        .header("anthropic-version", ANTHROPIC_VERSION);
                    if self.fallbacks() {
                        req = req.header("anthropic-beta", FALLBACK_BETA);
                    }
                    req
                }
                Provider::OpenAi => {
                    let req = inner
                        .http
                        .post(&format!("{}/chat/completions", inner.base_url));
                    if inner.api_key.is_empty() {
                        req
                    } else {
                        req.bearer(&inner.api_key)
                    }
                }
            };
            let res = req.json(&body).send().await?;
            let status = res.status();
            if status.is_success() {
                return Ok(res);
            }
            let retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529);
            if retryable && attempt < inner.retries {
                let wait = res
                    .header("retry-after")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map_or(Duration::from_secs(1 << attempt.min(5)), |s| {
                        Duration::from_secs(s.min(30))
                    });
                attempt += 1;
                tokio::time::sleep(wait).await;
                continue;
            }
            return Err(provider_error(status, res).await);
        }
    }

    /// Send the request and wait for the whole reply.
    ///
    /// # Errors
    ///
    /// As described in the [module docs](self).
    pub async fn send(self) -> Result<Reply> {
        let provider = self.llm.inner.provider;
        let res = self.post(false).await?;
        let body: Value = res.json().await?;
        Ok(match provider {
            Provider::Anthropic => Reply::from_anthropic(&body),
            Provider::OpenAi => Reply::from_openai(&body),
        })
    }

    /// Send the request and stream the reply as it is generated.
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send), for failures before the first token.
    pub async fn stream(self) -> Result<TokenStream> {
        let provider = self.llm.inner.provider;
        let res = self.post(true).await?;
        Ok(TokenStream {
            res,
            provider,
            buf: BytesMut::new(),
            pending: VecDeque::new(),
            usage: Usage::default(),
            stop: None,
            done: false,
            failed: None,
        })
    }
}

async fn provider_error(status: StatusCode, res: HttpResponse) -> Error {
    let detail = match res.json::<Value>().await {
        Ok(v) => v["error"]["message"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .unwrap_or("")
            .to_owned(),
        Err(_) => String::new(),
    };
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    if status == StatusCode::TOO_MANY_REQUESTS {
        Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("the language model is rate limiting{detail}"),
        )
        .with_code("llm_rate_limited")
    } else {
        Error::new(
            StatusCode::BAD_GATEWAY,
            format!("the language model answered {status}{detail}"),
        )
        .with_code("llm_error")
    }
}

/// Why the model stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopReason {
    /// It finished its answer.
    EndTurn,
    /// It reached `max_tokens`: the answer is cut off.
    MaxTokens,
    /// It stopped at a stop sequence.
    StopSequence,
    /// It declined the request (safety classifiers, or the provider's
    /// content filter). The text may be empty; don't show it as an answer.
    Refusal,
    /// It wants to call a tool.
    ToolUse,
    /// Anything else, as the provider named it.
    Other(String),
}

impl StopReason {
    fn from_anthropic(s: &str) -> Self {
        match s {
            "end_turn" => StopReason::EndTurn,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "refusal" => StopReason::Refusal,
            "tool_use" => StopReason::ToolUse,
            other => StopReason::Other(other.to_owned()),
        }
    }

    fn from_openai(s: &str) -> Self {
        match s {
            "stop" => StopReason::EndTurn,
            "length" => StopReason::MaxTokens,
            "content_filter" => StopReason::Refusal,
            "tool_calls" => StopReason::ToolUse,
            other => StopReason::Other(other.to_owned()),
        }
    }
}

/// Tokens a request used (what providers bill for).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Tokens read: the prompt and the conversation.
    pub input_tokens: u64,
    /// Tokens generated.
    pub output_tokens: u64,
}

/// A whole reply.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Reply {
    /// The answer's text.
    pub text: String,
    /// Why the model stopped. Check for [`StopReason::Refusal`] and
    /// [`StopReason::MaxTokens`] before using `text`.
    pub stop: StopReason,
    /// Tokens used.
    pub usage: Usage,
    /// The model that answered (with fallbacks, possibly not the one asked).
    pub model: String,
}

impl Reply {
    fn from_anthropic(v: &Value) -> Self {
        let text = v["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<String>()
            })
            .unwrap_or_default();
        Reply {
            text,
            stop: StopReason::from_anthropic(v["stop_reason"].as_str().unwrap_or("")),
            usage: Usage {
                input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            },
            model: v["model"].as_str().unwrap_or("").to_owned(),
        }
    }

    fn from_openai(v: &Value) -> Self {
        let choice = &v["choices"][0];
        Reply {
            text: choice["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            stop: StopReason::from_openai(choice["finish_reason"].as_str().unwrap_or("")),
            usage: Usage {
                input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            },
            model: v["model"].as_str().unwrap_or("").to_owned(),
        }
    }
}

/// A reply arriving piece by piece.
pub struct TokenStream {
    res: HttpResponse,
    provider: Provider,
    buf: BytesMut,
    pending: VecDeque<String>,
    usage: Usage,
    stop: Option<StopReason>,
    done: bool,
    /// An error the provider reported mid-reply, raised once the text
    /// before it has been handed out.
    failed: Option<Error>,
}

impl fmt::Debug for TokenStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenStream")
            .field("usage", &self.usage)
            .field("stop", &self.stop)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl TokenStream {
    /// The next piece of text, or `None` when the reply is complete.
    ///
    /// # Errors
    ///
    /// When the connection fails or the provider reports an error mid-way
    /// (`502`, code `llm_error`).
    pub async fn next(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(text) = self.pending.pop_front() {
                return Ok(Some(text));
            }
            if let Some(err) = self.failed.take() {
                return Err(err);
            }
            if self.done {
                return Ok(None);
            }
            match self.res.chunk().await? {
                Some(bytes) => {
                    self.buf.extend_from_slice(&bytes);
                    self.drain_events();
                }
                None => {
                    self.done = true;
                }
            }
        }
    }

    /// Tokens used so far (complete once the stream has ended).
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Why the model stopped, once the stream has ended.
    pub fn stop_reason(&self) -> Option<&StopReason> {
        self.stop.as_ref()
    }

    /// Parse every complete SSE event in the buffer.
    fn drain_events(&mut self) {
        while !self.done {
            let text = String::from_utf8_lossy(&self.buf);
            let Some(end) = text.find("\n\n").or_else(|| text.find("\r\n\r\n")) else {
                return;
            };
            let sep = if text[end..].starts_with("\r\n\r\n") {
                4
            } else {
                2
            };
            let event = text[..end].to_owned();
            drop(text);
            let _ = self.buf.split_to(end + sep);
            let data: String = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                self.done = true;
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            match self.provider {
                Provider::Anthropic => self.anthropic_event(&v),
                Provider::OpenAi => self.openai_event(&v),
            }
        }
    }

    fn anthropic_event(&mut self, v: &Value) {
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &v["message"]["usage"];
                self.usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_delta" if v["delta"]["type"] == "text_delta" => {
                if let Some(t) = v["delta"]["text"].as_str() {
                    if !t.is_empty() {
                        self.pending.push_back(t.to_owned());
                    }
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(StopReason::from_anthropic(s));
                }
                if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = n;
                }
            }
            "message_stop" => self.done = true,
            "error" => {
                self.done = true;
                let msg = v["error"]["message"].as_str().unwrap_or("stream error");
                self.failed = Some(
                    Error::new(
                        StatusCode::BAD_GATEWAY,
                        format!("the language model failed mid-reply: {msg}"),
                    )
                    .with_code("llm_error"),
                );
            }
            _ => {}
        }
    }

    fn openai_event(&mut self, v: &Value) {
        if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
            if !t.is_empty() {
                self.pending.push_back(t.to_owned());
            }
        }
        if let Some(s) = v["choices"][0]["finish_reason"].as_str() {
            self.stop = Some(StopReason::from_openai(s));
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
            self.usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
        }
    }

    /// Relay the reply to the browser as server-sent events: one `token`
    /// event per piece of text (`data` is the text), then a `done` event
    /// with `{"stop": ..., "input_tokens": ..., "output_tokens": ...}`, or an
    /// `error` event with `{"error": ...}`.
    ///
    /// ```js
    /// const events = new EventSource("/ask?q=...");
    /// events.addEventListener("token", (e) => (out.textContent += e.data));
    /// events.addEventListener("done", () => events.close());
    /// ```
    pub fn into_sse(self) -> Response {
        let stream = futures_util::stream::unfold(Some(self), |state| async move {
            let mut tokens = state?;
            match tokens.next().await {
                Ok(Some(text)) => Some((token_event(&text), Some(tokens))),
                Ok(None) => {
                    let stop = match tokens.stop_reason() {
                        Some(StopReason::EndTurn) => "end_turn".to_owned(),
                        Some(StopReason::MaxTokens) => "max_tokens".to_owned(),
                        Some(StopReason::StopSequence) => "stop_sequence".to_owned(),
                        Some(StopReason::Refusal) => "refusal".to_owned(),
                        Some(StopReason::ToolUse) => "tool_use".to_owned(),
                        Some(StopReason::Other(s)) => s.clone(),
                        None => "unknown".to_owned(),
                    };
                    let u = tokens.usage();
                    let done = Event::default().event("done").data(
                        json!({
                            "stop": stop,
                            "input_tokens": u.input_tokens,
                            "output_tokens": u.output_tokens,
                        })
                        .to_string(),
                    );
                    Some((done, None))
                }
                Err(e) => {
                    let err = Event::default()
                        .event("error")
                        .data(json!({"error": e.to_string()}).to_string());
                    Some((err, None))
                }
            }
        });
        let mut res = Sse::new(stream).into_response();
        res.headers_mut()
            .insert("x-llm-stream", HeaderValue::from_static("1"));
        res
    }
}

/// A `token` event; SSE data can't hold raw newlines, so each line of the
/// text becomes its own `data:` line, which browsers join back with `\n`.
fn token_event(text: &str) -> Event {
    Event::default().event("token").data(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_body_has_fallbacks_effort_and_no_sampling() {
        let llm = Llm::anthropic("k").effort(Effort::High);
        let body = llm.chat().system("be brief").user("hi").body(false);
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["max_tokens"], 16_000);
        assert_eq!(body["system"], "be brief");
        assert_eq!(
            body["messages"][0],
            json!({"role": "user", "content": "hi"})
        );
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["fallbacks"], "default");
        assert!(body.get("temperature").is_none());
        let streaming = llm.chat().user("hi").body(true);
        assert_eq!(streaming["max_tokens"], 64_000);
        assert_eq!(streaming["stream"], true);
        // Models without server-side fallback don't get the field.
        let haiku = Llm::anthropic("k").model("claude-haiku-5-5");
        assert!(haiku
            .chat()
            .user("x")
            .body(false)
            .get("fallbacks")
            .is_none());
    }

    #[test]
    fn openai_body_puts_the_system_prompt_first() {
        let llm = Llm::openai_compatible("http://x/v1", "", "m");
        let body = llm.chat().system("s").user("u").body(true);
        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": "s"})
        );
        assert_eq!(body["messages"][1], json!({"role": "user", "content": "u"}));
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn debug_hides_the_key() {
        let s = format!("{:?}", Llm::anthropic("sk-ant-secret"));
        assert!(!s.contains("secret"), "{s}");
    }
}
