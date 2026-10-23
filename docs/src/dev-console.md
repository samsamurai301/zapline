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
| Recent requests | The last 50 requests: status (4xx and 5xx highlighted), method, path with query, and how long the app took. |
| Recording | Whether requests are being recorded, the latest recorded ones, and buttons to start or stop recording, **Save as snapshot test** and clear. See [below](#recording-requests-as-tests). |
| Routes | Every route with its summary and tags (from doc comments and `#[get(.., tag = "..")]`). Parameterless `GET` routes are links. |
| Jobs | When [durable jobs](jobs.md) are registered: queued, running and failed counts, the registered job names, the latest failures with their errors, and a **Retry failed jobs** button. |
| Mail | When the [mailer](mail.md) is the dev outbox: how many emails were kept, with a link to the inbox at `/_dev/mail`. |

## Safe to leave in

- **Debug builds only.** In a release build (`cargo build --release`, the Dockerfiles) `.dev_console()` does nothing: no route, no middleware, no cost.
- **This machine only.** Requests from any other address get a `404`, so a debug build bound to `0.0.0.0` doesn't show its routes or jobs to the network.

The console's own requests are not listed under recent requests.

## Recording requests as tests

The console can record full request/response pairs and save them as [snapshot tests](testing.md#snapshot-tests-from-real-traffic) that replay on every `cargo test`:

- **Start recording** with the button, or start the app with `ZAPLINE_RECORD=1` (`zapline dev --record`). With the variable, recording starts with the app and is kept in `target/zapline/recording.jsonl`, so the restarts `zapline dev` does after each change don't lose it. Recording started from the button lives in memory.
- **Save** with the console's form or `zapline snap <name>`: the recording goes to `tests/snapshots/<name>.json`, plus `tests/snapshots.rs` (which replays them) if the project has none. Saving empties the recording, so the next snapshot starts fresh.
- `Authorization`, `Cookie` and API key header values are never recorded. Static files, the console's own requests and WebSocket upgrades are skipped.

While not recording, the console's middleware checks one flag per request and does nothing else. In release builds there is no console and no recorder at all.

The console's routes for this, all local-only: `GET /_dev/recordings` returns the current recording as snapshot JSON; `POST /_dev/recordings/start`, `/stop`, `/clear`, and `/save` (form field `name`; answers JSON when the request accepts `application/json`, which is what `zapline snap` uses). The `POST` routes also refuse cross-site requests, so a web page can't drive them.
