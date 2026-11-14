//! Tools a model can call: typed Rust functions with a JSON schema derived
//! from their argument type, run by [`Chat::run`](super::Chat::run) or
//! served to MCP clients by [`App::mcp`](crate::App::mcp).

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures_util::future::join_all;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use super::{Chat, Msg, Provider, Reply, Usage};
use crate::extract::FromRequestParts;
use crate::handler::BoxFuture;
use crate::request::{Request, Shared};
use crate::{Error, Result};

type CallFn = dyn Fn(Value, ToolContext) -> BoxFuture<Result<Value>> + Send + Sync;

struct Tool {
    name: String,
    description: String,
    schema: Value,
    call: Box<CallFn>,
}

/// A set of tools. Cheap to clone.
///
/// ```
/// use zapline::ai::{ToolContext, Tools};
/// use zapline::prelude::*;
///
/// #[model]
/// struct Weather {
///     /// City name, e.g. "Paris".
///     city: String,
/// }
///
/// let tools = Tools::new().tool(
///     "get_weather",
///     "The current weather in a city.",
///     |args: Weather, _cx: ToolContext| async move { Ok(format!("Sunny in {}", args.city)) },
/// );
/// assert_eq!(tools.names(), ["get_weather"]);
/// ```
#[derive(Clone, Default)]
pub struct Tools {
    tools: Vec<Arc<Tool>>,
}

impl fmt::Debug for Tools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// The JSON schema of `A`, self-contained (no `$ref`s), as tool APIs expect.
fn schema_of<A: JsonSchema>() -> Value {
    let generator = schemars::generate::SchemaSettings::draft2020_12()
        .with(|s| s.inline_subschemas = true)
        .into_generator();
    let mut schema = serde_json::to_value(generator.into_root_schema_for::<A>())
        .unwrap_or_else(|_| json!({"type": "object"}));
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    schema
}

impl Tools {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. The model sees `name`, `description` and the JSON
    /// schema of `A` (doc comments on its fields become their
    /// descriptions); a call deserializes the model's arguments into `A`
    /// and runs `f`. What `f` returns is sent back as text (a `String` as is,
    /// anything else as JSON). An `Err` goes back as a failed call, so the
    /// model can recover.
    ///
    /// # Panics
    ///
    /// When `name` isn't 1 to 64 letters, digits, `_` or `-` (what tool APIs
    /// accept), or is already taken. Tools are set up at startup, so this
    /// fails fast instead of at the first request.
    pub fn tool<A, F, Fut, T>(mut self, name: &str, description: &str, f: F) -> Self
    where
        A: DeserializeOwned + JsonSchema + Send + 'static,
        F: Fn(A, ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
        T: Serialize + 'static,
    {
        assert!(
            (1..=64).contains(&name.len())
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "tool name `{name}`: 1 to 64 letters, digits, `_` or `-`"
        );
        assert!(
            !self.tools.iter().any(|t| t.name == name),
            "tool `{name}` is defined twice"
        );
        let f = Arc::new(f);
        let call: Box<CallFn> = Box::new(move |args: Value, cx: ToolContext| {
            let f = f.clone();
            Box::pin(async move {
                let args: A = serde_json::from_value(args).map_err(|e| {
                    Error::bad_request(format!("invalid arguments: {e}"))
                        .with_code("tool_arguments")
                })?;
                let out = f(args, cx).await?;
                serde_json::to_value(out)
                    .map_err(|e| Error::internal("a tool's result isn't JSON").with_source(e))
            })
        });
        self.tools.push(Arc::new(Tool {
            name: name.to_owned(),
            description: description.to_owned(),
            schema: schema_of::<A>(),
            call,
        }));
        self
    }

    /// The tools' names, in the order added.
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name.as_str()).collect()
    }

    /// Whether there are no tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Run tool `name` with the model's `args`. Returns the text to send
    /// back and whether it is an error. Unknown tools and bad arguments are
    /// errors the model sees; a tool's server error (5xx) is reported only
    /// as "the tool failed", so internal details don't reach the model.
    pub async fn call(&self, name: &str, args: Value, cx: &ToolContext) -> (String, bool) {
        let Some(tool) = self.tools.iter().find(|t| t.name == name) else {
            return (format!("there is no tool named `{name}`"), true);
        };
        match (tool.call)(args, cx.clone()).await {
            Ok(Value::String(s)) => (s, false),
            Ok(v) => (v.to_string(), false),
            Err(e) if e.status().is_server_error() => {
                crate::tasks::log_warn(format_args!("tool `{name}` failed: {e}"));
                ("the tool failed".to_owned(), true)
            }
            Err(e) => (e.message().to_owned(), true),
        }
    }

    /// The tools as `{name, description, input_schema}` (Anthropic).
    pub(crate) fn anthropic_json(&self) -> Value {
        Value::Array(
            self.tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}))
                .collect(),
        )
    }

    /// The tools as OpenAI-style functions.
    pub(crate) fn openai_json(&self) -> Value {
        Value::Array(
            self.tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {
                        "name": t.name, "description": t.description, "parameters": t.schema,
                    }})
                })
                .collect(),
        )
    }

    /// The tools as MCP `tools/list` entries.
    pub(crate) fn mcp_json(&self) -> Value {
        Value::Array(
            self.tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.schema}))
                .collect(),
        )
    }
}

/// What a tool can reach besides its arguments: the app's state (the same
/// values `App::state` registered and handlers get). As a handler argument
/// it carries this request's app; [`ToolContext::none`] carries nothing.
#[derive(Clone, Default)]
pub struct ToolContext {
    shared: Option<Arc<Shared>>,
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("has_app", &self.shared.is_some())
            .finish()
    }
}

impl ToolContext {
    /// A context with no app state, for tools that don't need any.
    pub fn none() -> Self {
        Self::default()
    }

    /// A value registered with `App::state`, like [`Request::state`].
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.shared.as_ref()?.state::<T>()
    }

    /// [`state`](Self::state), or a `500` naming the missing type.
    ///
    /// # Errors
    ///
    /// When `T` wasn't registered.
    pub fn require<T: Clone + Send + Sync + 'static>(&self) -> Result<T> {
        self.state::<T>().cloned().ok_or_else(|| {
            Error::internal(format!(
                "a tool needs `{}`, which the app didn't register",
                std::any::type_name::<T>()
            ))
        })
    }

    pub(crate) fn from_request(req: &Request) -> Self {
        ToolContext {
            shared: Some(req.shared.clone()),
        }
    }
}

impl FromRequestParts for ToolContext {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        Ok(ToolContext::from_request(req))
    }
}

/// A tool call made during [`Chat::run`](super::Chat::run).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ToolCall {
    /// The tool's name.
    pub name: String,
    /// The arguments the model passed.
    pub input: Value,
    /// What went back to the model.
    pub output: String,
    /// Whether it went back as an error.
    pub is_error: bool,
}

/// Most model turns in one [`Chat::run`](super::Chat::run).
const MAX_ROUNDS: usize = 10;
/// Most tool calls in one run, across turns.
const MAX_CALLS: usize = 50;

impl Chat {
    /// Send the request with `tools` available, run the tools the model
    /// calls, send the results back, and repeat until the model answers
    /// (at most 10 turns and 50 tool calls). Calls in one turn run
    /// concurrently. The [`Reply`] has the final answer, the usage summed
    /// over every turn, and each call in [`tool_calls`](Reply::tool_calls).
    ///
    /// Tool results are data the model reads, not instructions you wrote:
    /// a tool that returns text from users or the web passes that text to
    /// the model, so don't give tools more power than any user of the app
    /// should have.
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send); `502 llm_tool_limit` when the model keeps
    /// calling tools past the limits.
    ///
    /// ```no_run
    /// # async fn demo(llm: zapline::ai::Llm, tools: zapline::ai::Tools, cx: zapline::ai::ToolContext) -> zapline::Result<()> {
    /// let reply = llm.chat().user("What's the weather in Paris?").run(&tools, &cx).await?;
    /// println!("{} ({} tool calls)", reply.text, reply.tool_calls.len());
    /// # Ok(()) }
    /// ```
    pub async fn run(mut self, tools: &Tools, cx: &ToolContext) -> Result<Reply> {
        let provider = self.llm.inner.provider;
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut usage = Usage::default();
        for _ in 0..MAX_ROUNDS {
            let body = self.body(false, Some(tools));
            let res = self.post_body(&body).await?;
            let v: Value = res.json().await?;
            let (requested, assistant) = match provider {
                Provider::Anthropic => anthropic_calls(&v),
                Provider::OpenAi => openai_calls(&v),
            };
            let mut reply = match provider {
                Provider::Anthropic => Reply::from_anthropic(&v),
                Provider::OpenAi => Reply::from_openai(&v),
            };
            usage.input_tokens += reply.usage.input_tokens;
            usage.output_tokens += reply.usage.output_tokens;
            if requested.is_empty() {
                reply.usage = usage;
                reply.tool_calls = calls;
                return Ok(reply);
            }
            if calls.len() + requested.len() > MAX_CALLS {
                break;
            }
            // The model's turn goes back unchanged (thinking blocks included).
            self.messages.push(Msg::Raw(assistant));
            let results = join_all(
                requested
                    .iter()
                    .map(|(_, name, input)| tools.call(name, input.clone(), cx)),
            )
            .await;
            let mut blocks = Vec::new();
            for ((id, name, input), (output, is_error)) in requested.into_iter().zip(results) {
                match provider {
                    Provider::Anthropic => blocks.push(json!({
                        "type": "tool_result", "tool_use_id": id,
                        "content": output, "is_error": is_error,
                    })),
                    Provider::OpenAi => self.messages.push(Msg::Raw(json!({
                        "role": "tool", "tool_call_id": id, "content": output,
                    }))),
                }
                calls.push(ToolCall {
                    name,
                    input,
                    output,
                    is_error,
                });
            }
            if provider == Provider::Anthropic {
                // Every result in one user message.
                self.messages
                    .push(Msg::Raw(json!({"role": "user", "content": blocks})));
            }
        }
        Err(Error::new(
            http::StatusCode::BAD_GATEWAY,
            format!(
                "the model kept calling tools ({} calls); stopped at the limit",
                calls.len()
            ),
        )
        .with_code("llm_tool_limit"))
    }
}

/// `(id, name, input)` of each `tool_use` block, and the assistant message.
fn anthropic_calls(v: &Value) -> (Vec<(String, String, Value)>, Value) {
    let content = v["content"].clone();
    let calls = content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| {
                    (
                        b["id"].as_str().unwrap_or("").to_owned(),
                        b["name"].as_str().unwrap_or("").to_owned(),
                        b["input"].clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    (calls, json!({"role": "assistant", "content": content}))
}

/// The same for an OpenAI-style `tool_calls` list (arguments arrive as a
/// JSON string).
fn openai_calls(v: &Value) -> (Vec<(String, String, Value)>, Value) {
    let message = v["choices"][0]["message"].clone();
    let calls = message["tool_calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .map(|c| {
                    let args = c["function"]["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(Value::Null);
                    (
                        c["id"].as_str().unwrap_or("").to_owned(),
                        c["function"]["name"].as_str().unwrap_or("").to_owned(),
                        args,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    (calls, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        /// The city.
        city: String,
        days: Option<u32>,
    }

    fn tools() -> Tools {
        Tools::new()
            .tool(
                "echo",
                "Echo the city.",
                |a: Args, _cx: ToolContext| async move { Ok(a.city) },
            )
            .tool(
                "boom",
                "Always fails.",
                |_a: Args, _cx: ToolContext| async move {
                    Err::<String, _>(Error::internal("db password is hunter2"))
                },
            )
            .tool(
                "nope",
                "A client error.",
                |_a: Args, _cx: ToolContext| async move {
                    Err::<String, _>(Error::bad_request("no such city"))
                },
            )
    }

    #[test]
    fn schemas_are_self_contained_and_described() {
        let json = tools().anthropic_json();
        let schema = &json[0]["input_schema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["city"]["description"], "The city.");
        assert!(schema.get("$schema").is_none() && schema.get("title").is_none());
        assert_eq!(schema["required"], json!(["city"]));
        assert_eq!(tools().openai_json()[0]["function"]["name"], "echo");
        assert!(tools().mcp_json()[0]["inputSchema"].is_object());
    }

    #[tokio::test]
    async fn calls_report_errors_without_leaking_internals() {
        let t = tools();
        let cx = ToolContext::none();
        assert_eq!(
            t.call("echo", json!({"city": "Oslo"}), &cx).await,
            ("Oslo".into(), false)
        );
        let (out, err) = t.call("echo", json!({"town": "x"}), &cx).await;
        assert!(err && out.contains("invalid arguments"), "{out}");
        assert_eq!(
            t.call("boom", json!({"city": "x"}), &cx).await,
            ("the tool failed".into(), true)
        );
        assert_eq!(
            t.call("nope", json!({"city": "x"}), &cx).await,
            ("no such city".into(), true)
        );
        let (out, err) = t.call("missing", json!({}), &cx).await;
        assert!(err && out.contains("no tool named"), "{out}");
    }

    #[test]
    #[should_panic(expected = "defined twice")]
    fn duplicate_names_fail_fast() {
        let _ = tools().tool("echo", "again", |a: Args, _cx: ToolContext| async move {
            Ok(a.city)
        });
    }
}
