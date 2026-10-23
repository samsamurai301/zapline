# Dev console

One call gives a running app a page that shows what it is made of and what it is doing:

```rust
use zapline::prelude::*;

#[get("/")]
async fn index() -> &'static str { "hi" }

fn main() -> std::io::Result<()> {
    App::new()
        .dev_console()
        .mount(index)
        .run("127.0.0.1:8080")
}
```

Open <http://localhost:8080/_dev>. Projects created with `zapline new` (except `minimal`) already have it.

| Panel | Shows |
|---|---|
| Recent errors | The last 20 errors (500s, panics and rejected requests), with their [diagnostic code](errors.md#diagnostic-codes), the real message and the fix. |
| Recent requests | The last 50 requests: status (4xx and 5xx highlighted), method, path with query, and how long the app took. |
| Routes | Every route with its summary and tags (from doc comments and `#[get(.., tag = "..")]`). Parameterless `GET` routes are links. |
| Jobs | When [durable jobs](jobs.md) are registered: queued, running and failed counts, the registered job names, the latest failures with their errors, and a **Retry failed jobs** button. |
| Mail | When the [mailer](mail.md) is the dev outbox: how many emails were kept, with a link to the inbox at `/_dev/mail`. |

## Safe to leave in

- **Debug builds only.** In a release build (`cargo build --release`, the Dockerfiles) `.dev_console()` does nothing: no route, no middleware, no cost.
- **This machine only.** Requests from any other address get a `404`, so a debug build bound to `0.0.0.0` doesn't show its routes or jobs to the network.

The console's own requests are not listed under recent requests.

`.dev_console()` also turns on the [dev error page](errors.md#the-dev-error-page): a 500 or a panic answers your browser with the error, its code, the fix and a **Copy fix prompt** block for your AI assistant.

## JSON for tools

`GET /_dev/api` serves the same data as JSON, newest first, for scripts and for the [live tools of `zapline mcp`](cli.md#live-tools-debugging-the-running-app):

```json
{"requests":[{"method":"GET","path":"/orders","status":500,"micros":812,"code":"ZL0002"}],
 "errors":[{"at_ms":1767225600000,"method":"GET","path":"/orders","status":500,"code":"ZL0002",
            "error_code":"missing_dependency","message":"no provider registered for `app::Repo`",
            "fix":".provide(value) // value: app::Repo; ..."}],
 "routes":[{"method":"GET","path":"/orders"}]}
```

`location` (`src/routes/orders.rs:42:17`) is set for panics. Like the page, it answers only this machine.
