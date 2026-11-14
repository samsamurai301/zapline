//! Tools: the agent loop against fake Claude and OpenAI-compatible
//! servers, tools reading app state, and the MCP endpoint.
#![cfg(feature = "ai")]

mod server_support;

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use server_support::TestServer;
use zapline::ai::{Llm, ToolContext, Tools};
use zapline::outbound::HttpClient;
use zapline::prelude::*;

#[model]
struct City {
    /// The city.
    city: String,
}

#[model]
struct Add {
    a: i64,
    b: i64,
}

/// The app's own data, which a tool reads through its context.
#[derive(Clone)]
struct Greeting(&'static str);

fn tools() -> Tools {
    Tools::new()
        .tool(
            "get_weather",
            "The weather in a city.",
            |c: City, _cx: ToolContext| async move { Ok(format!("Sunny in {}", c.city)) },
        )
        .tool(
            "add",
            "Add two numbers.",
            |x: Add, _cx: ToolContext| async move { Ok(x.a + x.b) },
        )
        .tool(
            "greet",
            "The app's greeting.",
            |_c: City, cx: ToolContext| async move { Ok(cx.require::<Greeting>()?.0.to_owned()) },
        )
        .tool(
            "broken",
            "Always fails.",
            |_c: City, _cx: ToolContext| async move {
                Err::<String, _>(Error::internal("secret internal detail"))
            },
        )
}

async fn serve(app: App) -> String {
    let mut app = app;
    app.server_config_mut().banner = false;
    let server = TestServer::spawn(app).await;
    let base = format!("http://{}", server.addr);
    std::mem::forget(server);
    base
}

type Bodies = Arc<Mutex<Vec<Value>>>;

/// A fake Messages API: answers each request with the next of `replies`.
async fn fake_claude(replies: Vec<Value>) -> (String, Bodies) {
    let bodies: Bodies = Arc::default();
    let log = bodies.clone();
    let replies = Arc::new(Mutex::new(replies.into_iter()));
    let base = serve(
        App::new().post("/v1/messages", move |Json(body): Json<Value>| {
            log.lock().unwrap().push(body);
            let next = replies.lock().unwrap().next();
            async move { Json(next.unwrap_or_else(|| json!({"error": "no more replies"}))) }
        }),
    )
    .await;
    (base, bodies)
}

fn local(llm: Llm, base: &str) -> Llm {
    llm.base_url(base)
        .http_client(HttpClient::new().allow_private_networks(true))
}

#[tokio::test]
async fn claude_calls_tools_until_it_answers() {
    let (base, bodies) = fake_claude(vec![
        json!({
            "model": "claude-opus-5-5",
            "content": [
                {"type": "thinking", "thinking": "", "signature": "sig-1"},
                {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "Paris"}},
                {"type": "tool_use", "id": "t2", "name": "add", "input": {"a": 2, "b": 3}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5},
        }),
        json!({
            "model": "claude-opus-5-5",
            "content": [{"type": "text", "text": "Sunny, and 5."}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 20, "output_tokens": 4},
        }),
    ])
    .await;
    let reply = local(Llm::anthropic("k"), &base)
        .chat()
        .user("Weather in Paris, and 2+3?")
        .run(&tools(), &ToolContext::none())
        .await
        .unwrap();
    assert_eq!(reply.text, "Sunny, and 5.");
    assert_eq!(
        (reply.usage.input_tokens, reply.usage.output_tokens),
        (30, 9)
    );
    assert_eq!(reply.tool_calls.len(), 2);
    assert_eq!(reply.tool_calls[0].output, "Sunny in Paris");
    assert_eq!(reply.tool_calls[1].output, "5");

    let bodies = bodies.lock().unwrap();
    // The tools were offered, with schemas from the argument types.
    let offered = &bodies[0]["tools"];
    assert_eq!(offered[0]["name"], "get_weather");
    assert_eq!(
        offered[0]["input_schema"]["properties"]["city"]["description"],
        "The city."
    );
    // The second request: the model's turn unchanged (thinking block and
    // all), then both results in one user message.
    let messages = bodies[1]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"][0]["signature"], "sig-1");
    let results = &messages[2]["content"];
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        results[0],
        json!({"type": "tool_result", "tool_use_id": "t1", "content": "Sunny in Paris", "is_error": false})
    );
    assert_eq!(results[1]["tool_use_id"], "t2");
    assert_eq!(results[1]["content"], "5");
}

#[tokio::test]
async fn a_model_that_never_stops_calling_tools_is_cut_off() {
    let call = json!({
        "content": [{"type": "tool_use", "id": "t", "name": "add", "input": {"a": 1, "b": 1}}],
        "stop_reason": "tool_use", "usage": {},
    });
    let (base, bodies) = fake_claude(vec![call; 20]).await;
    let err = local(Llm::anthropic("k"), &base)
        .chat()
        .user("loop")
        .run(&tools(), &ToolContext::none())
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("llm_tool_limit"));
    assert_eq!(bodies.lock().unwrap().len(), 10, "at most 10 turns");
}

#[tokio::test]
async fn openai_compatible_tool_calls() {
    let bodies: Bodies = Arc::default();
    let log = bodies.clone();
    let turn = Arc::new(Mutex::new(0));
    let base = serve(
        App::new().post("/v1/chat/completions", move |Json(body): Json<Value>| {
            log.lock().unwrap().push(body);
            let mut n = turn.lock().unwrap();
            *n += 1;
            let reply = if *n == 1 {
                json!({"choices": [{"finish_reason": "tool_calls", "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{"id": "c1", "type": "function",
                        "function": {"name": "add", "arguments": "{\"a\": 40, \"b\": 2}"}}],
                }}]})
            } else {
                json!({"choices": [{"finish_reason": "stop", "message": {"content": "42"}}]})
            };
            async move { Json(reply) }
        }),
    )
    .await;
    let llm = Llm::openai_compatible(format!("{base}/v1"), "", "local")
        .http_client(HttpClient::new().allow_private_networks(true));
    let reply = llm
        .chat()
        .user("40+2?")
        .run(&tools(), &ToolContext::none())
        .await
        .unwrap();
    assert_eq!(reply.text, "42");
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies[0]["tools"][1]["function"]["name"], "add");
    let messages = bodies[1]["messages"].as_array().unwrap();
    assert_eq!(messages[1]["tool_calls"][0]["id"], "c1");
    assert_eq!(
        messages[2],
        json!({"role": "tool", "tool_call_id": "c1", "content": "42"})
    );
}

#[tokio::test]
async fn tools_read_app_state_through_their_context() {
    let (base, _) = fake_claude(vec![
        json!({"content": [{"type": "tool_use", "id": "g", "name": "greet", "input": {"city": "x"}}],
               "stop_reason": "tool_use", "usage": {}}),
        json!({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn", "usage": {}}),
    ])
    .await;
    let llm = local(Llm::anthropic("k"), &base);
    let c = App::new()
        .state(Greeting("hello from the app"))
        .state(llm)
        .get("/ask", |llm: Llm, cx: ToolContext| async move {
            let reply = llm.chat().user("greet").run(&tools(), &cx).await?;
            Ok::<_, Error>(reply.tool_calls[0].output.clone())
        })
        .test();
    assert_eq!(c.get("/ask").send().await.text(), "hello from the app");
}

/// A JSON-RPC request to the MCP endpoint.
async fn rpc(c: &zapline::test::TestClient, body: Value) -> zapline::test::TestResponse {
    c.post("/mcp").json(&body).send().await
}

#[tokio::test]
async fn mcp_lists_and_calls_tools() {
    let c = App::new().state(Greeting("hi")).mcp("/mcp", tools()).test();

    let init = rpc(&c, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}))
    .await;
    let init: Value = init.json();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    // An unknown version gets the newest one.
    let newest: Value = rpc(
        &c,
        json!({"jsonrpc": "2.0", "id": 2, "method": "initialize",
        "params": {"protocolVersion": "1999-01-01"}}),
    )
    .await
    .json();
    assert_eq!(newest["result"]["protocolVersion"], "2025-06-18");

    let note = rpc(
        &c,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(note.status(), 202);

    let list: Value = rpc(
        &c,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
    )
    .await
    .json();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["get_weather", "add", "greet", "broken"]);
    assert_eq!(
        list["result"]["tools"][1]["inputSchema"]["required"],
        json!(["a", "b"])
    );

    let call: Value = rpc(
        &c,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "add", "arguments": {"a": 2, "b": 2}}}),
    )
    .await
    .json();
    assert_eq!(
        call["result"],
        json!({"content": [{"type": "text", "text": "4"}], "isError": false})
    );
    // Tools see the app's state.
    let greet: Value = rpc(
        &c,
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {"name": "greet", "arguments": {"city": "x"}}}),
    )
    .await
    .json();
    assert_eq!(greet["result"]["content"][0]["text"], "hi");
    // A failing tool is a tool error, without its internals.
    let broken: Value = rpc(
        &c,
        json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {"name": "broken", "arguments": {"city": "x"}}}),
    )
    .await
    .json();
    assert_eq!(broken["result"]["isError"], true);
    assert!(!broken.to_string().contains("secret"), "{broken}");

    let unknown: Value = rpc(
        &c,
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": {"name": "nope"}}),
    )
    .await
    .json();
    assert_eq!(unknown["error"]["code"], -32602);
    let method: Value = rpc(
        &c,
        json!({"jsonrpc": "2.0", "id": 8, "method": "resources/list"}),
    )
    .await
    .json();
    assert_eq!(method["error"]["code"], -32601);
    let ping: Value = rpc(&c, json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}))
        .await
        .json();
    assert_eq!(ping["result"], json!({}));
}

#[tokio::test]
async fn mcp_refuses_other_origins_and_bad_messages() {
    let c = App::new().mcp("/mcp", tools()).test();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    let evil = c
        .post("/mcp")
        .header("origin", "https://evil.example")
        .json(&body)
        .send()
        .await;
    assert_eq!(evil.status(), 403);
    let host = c.post("/mcp").json(&body).send().await;
    assert_eq!(host.status(), 200);

    let garbage = c
        .post("/mcp")
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await;
    assert_eq!(garbage.status(), 400);
    assert_eq!(garbage.json::<Value>()["error"]["code"], -32700);
    let batch = c.post("/mcp").json(&json!([body])).send().await;
    assert_eq!(batch.json::<Value>()["error"]["code"], -32600);
    assert_eq!(c.get("/mcp").send().await.status(), 405);
}

#[tokio::test]
async fn mcp_can_require_a_login() {
    let c = App::new()
        .nest(
            "/",
            Routes::new()
                .mcp("/mcp", tools())
                .wrap(|req: Request, next: Next| async move {
                    if req.headers().contains_key("authorization") {
                        next.run(req).await
                    } else {
                        Error::unauthorized().into_response()
                    }
                }),
        )
        .test();
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    assert_eq!(c.post("/mcp").json(&ping).send().await.status(), 401);
    assert_eq!(
        c.post("/mcp").bearer("t").json(&ping).send().await.status(),
        200
    );
}
