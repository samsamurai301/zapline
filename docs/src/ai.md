# AI & language models

The `ai` feature adds `zapline::ai::Llm`. It calls Claude through Anthropic's Messages API, or any OpenAI-compatible chat API (a local Ollama or vLLM, other providers). It returns whole replies, or streams the text to the browser as it's written.

```rust
use zapline::ai::Llm;
use zapline::prelude::*;

async fn summarize(llm: Llm, body: String) -> Result<String> {
    let reply = llm
        .chat()
        .system("Summarize the text in one sentence.")
        .user(body)
        .send()
        .await?;
    Ok(reply.text)
}

async fn ask(llm: Llm, question: String) -> Result<impl IntoResponse> {
    // Text appears in the browser as the model writes it.
    Ok(llm.chat().user(question).stream().await?.into_sse())
}

fn app() -> App {
    App::new()
        .state(Llm::from_env().expect("set ANTHROPIC_API_KEY"))
        .post("/summarize", summarize)
        .post("/ask", ask)
}
```

## Choosing a model

| Constructor | Calls |
|---|---|
| `Llm::anthropic(key)` | Claude, model `claude-opus-5-5` by default; change it with `.model("claude-sonnet-5-5")` or `.model("claude-haiku-5-5")` |
| `Llm::from_env()` | Claude with `ANTHROPIC_API_KEY`, and `LLM_MODEL` when set |
| `Llm::openai_compatible(base_url, key, model)` | `POST {base_url}/chat/completions`, e.g. `http://localhost:11434/v1` for Ollama (the key may be empty) |

A model server on your own network is a private address, which the HTTP client refuses by default ([Calling other services](outbound.md#server-side-request-forgery)). Allow it explicitly for that client:

```rust
# use zapline::ai::Llm;
# use zapline::outbound::HttpClient;
let local = Llm::openai_compatible("http://10.0.0.5:8000/v1", "", "qwen")
    .http_client(HttpClient::new().allow_private_networks(true));
```

## Requests and replies

`llm.chat()` starts a request: `.system(..)` (instructions), `.user(..)` and `.assistant(..)` (the conversation so far), `.max_tokens(n)` (default 16 000, or 64 000 when streaming) and, for Claude, `.effort(Effort::Low | Medium | High | XHigh | Max)`. Lower effort is faster and cheaper; Claude Opus 5.5 defaults to `Medium`.

`.send().await?` returns a `Reply`:

| Field | What |
|---|---|
| `text` | The answer |
| `stop` | Why it stopped: `EndTurn`, `MaxTokens` (cut off), `Refusal` (declined: don't show `text` as an answer), `StopSequence`, `ToolUse` |
| `usage` | `input_tokens` and `output_tokens`, what the provider bills |
| `model` | The model that answered |

`.stream().await?` returns a `TokenStream`: call `.next().await?` for each piece of text until `None`, then read `.stop_reason()` and `.usage()`. Or hand it to the browser with `.into_sse()`, which sends one `token` event per piece, then `done` with `{"stop", "input_tokens", "output_tokens"}`, or `error`:

```js
const events = new EventSource("/ask?q=...");
events.addEventListener("token", (e) => (answer.textContent += e.data));
events.addEventListener("done", () => events.close());
```

## Reliability

- **Retries:** `429`, `500`, `502`, `503`, `504` and `529` (overloaded) are retried before any output arrives, twice by default (`.retries(n)`). The wait is what `retry-after` says, or 1 s, then 2 s. A request that keeps failing returns `429` (code `llm_rate_limited`) or `502` (`llm_error`, with the provider's message).
- **Timeouts:** each call waits up to 10 minutes for the first byte and between streamed pieces. Long generations are normal; stream them so users see progress.
- **Refusals:** Claude's safety classifiers can decline a request. For the models that support it (Claude Opus 5.5, Opus 5, Sonnet 5.5, Fable 5.1 and 5.2), `Llm` opts into Anthropic's server-side fallback by default: a declined request is retried on a fallback model in the same call, and `reply.model` says which model answered. Turn it off with `.refusal_fallbacks(false)` to get `StopReason::Refusal` back instead.
- **Secrets:** the API key never appears in `Debug` output or logs.
