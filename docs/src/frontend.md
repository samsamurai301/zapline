# Typed frontends (TypeScript & no-build JS)

Your Rust handlers are the API contract. zapline turns the [OpenAPI document](openapi.md) it already generates into a typed client, and keeps that client current while you work: change a struct, save, and the frontend's type checker points at every call that no longer fits. There is no schema language to maintain, no generator to install, and no `npm` package in between.

Two flavors come from the same document:

| You have | You get | How |
|---|---|---|
| A bundled frontend (React, Vue, Svelte, Solid...) | `api.ts`: interfaces plus a typed `createClient()` | `zapline dev` with a `frontend/` directory, or `zapline client -o web/src/api.ts` |
| Plain pages, htmx, `<script type="module">` | `api.js`: the same client as an ES module with JSDoc types | `OpenApi::new(..).client_js()` serves `/_zapline/api.js` |

Both use `fetch` only, so they run in browsers, Node 18+, Deno and Bun, and both pass `tsc --strict --exactOptionalPropertyTypes --noUncheckedIndexedAccess`.

## Live TypeScript with `zapline dev`

Put your frontend in `frontend/` and run `zapline dev`. After every successful build, before the new binary starts, zapline exports the spec from it and writes `frontend/src/api.ts`. The file is only rewritten when it changes, so vite, `tsc --watch` and your editor react exactly once, and dev tells you what moved:

```text
    Building my-app
      Client updated frontend/src/api.ts (+archiveTask ~createTask)
     Running target/debug/my-app (pid 4242)
```

(`+` added, `-` removed, `~` changed arguments or result.) A Rust change that breaks the frontend now shows up as a red squiggle in the `.tsx` file within a second of saving the `.rs` file.

To put the client somewhere else, or to also write the JavaScript module to disk, add a `zapline.toml` next to `Cargo.toml` (or the same keys under `[package.metadata.zapline.client]` in `Cargo.toml`):

```toml
[client]
ts = "web/src/api.ts"     # TypeScript client
js = "static/js/api.js"   # ES module with JSDoc types
# enabled = false         # turn off the frontend/ default
```

`zapline dev --client path/to/api.ts` overrides the settings for one run and `--no-client` turns clients off. `zapline client` (no `-o`) writes the configured files once, which is what you want in CI. If the project has a `frontend/` directory but the app has no OpenAPI plugin, dev says so once and carries on.

To fail CI when a committed client is stale, compare it with a fresh one in a Rust test:

```rust,ignore
use zapline::openapi::{codegen, spec_for};

#[test]
fn frontend_client_is_current() {
    let spec = spec_for(my_app::app(), my_app::api());
    let fresh = codegen::typescript(&spec).unwrap().source;
    let committed = std::fs::read_to_string("frontend/src/api.ts").unwrap();
    assert!(committed == fresh, "run `zapline client` and commit frontend/src/api.ts");
}
```

## Using the client

Methods are named after operation ids (the handler name: `create_task` becomes `createTask`). Path parameters come first, then the body, then a `query` object, then per-call options:

```ts
import { api, createClient, isValidationError, type Task } from "./api";

const page = await api.listTasks({ status: "todo", limit: 20 }); // query is typed: status is "todo" | "doing" | "done"
const task: Task = await api.getTask(42);
await api.updateTask(42, { status: "done" });
await api.upload(42, { file: input.files![0]!, note: "scan" }); // multipart: a FormData or a plain object
const res = await api.events();                                 // SSE / NDJSON: the raw Response, to stream
const csv: Blob = await api.export();                           // binary bodies resolve to a Blob

// Your own instance: base URL, auth headers, cookies, a custom fetch.
const remote = createClient({
  baseUrl: "https://api.example.com",
  headers: async () => ({ authorization: `Bearer ${await token()}` }),
  credentials: "include",
});
await remote.getTask(1, { signal: AbortSignal.timeout(5000) });
```

`api` is a ready same-origin instance. Each method's doc comment carries the route (`POST /tasks`), the handler's doc comment, and the error responses it documents, so hovering a call shows what the server says about it. Field constraints from `#[validate(..)]` and `#[serde(default)]` appear on the interfaces as `@minLength`, `@maximum`, `@default` and so on.

What the generator maps:

| In Rust (via the spec) | In TypeScript |
|---|---|
| structs, `Option<T>` fields | interfaces, `field?: T \| null` |
| unit enums | string literal unions (`"todo" \| "doing" \| "done"`), with each variant's doc |
| tagged enums (`#[serde(tag = "kind")]`) | discriminated unions: `{ kind: "circle"; radius: number } \| { kind: "rect"; ... }` |
| `HashMap<String, T>`, tuples, `Vec<T>` | `Record<string, T>`, `[A, B]`, `T[]` |
| `Path<(String, u32)>`, `{*rest}` catch-alls | positional arguments; catch-alls keep their slashes |
| `Query<T>` (also optional, also free-form maps) | one typed `query` object |
| `Json<T>`, `Form<T>`, `Multipart`, `String`, `Bytes` bodies | JSON, URL-encoded, `FormData`, text and binary bodies |
| `Json<T>`, `String`, `Bytes`, `Sse`, `()` / `StatusCode` results | `T`, `string`, `Blob`, `Response`, `void` |

Routes that answer with HTML (htmx pages and fragments) are left out.

## Errors, typed

Every non-2xx response throws an `ApiError` with `status`, zapline's `code` and `details`, the decoded `body`, and the `operation` that failed. A `422` that lists field errors (from [`Valid<T>`](validation.md), or a body field of the wrong type) throws a `ValidationError` instead, whose `fields` are keyed by the request body's field names:

```ts
try {
  await api.createTask(form);
} catch (e) {
  if (isValidationError(e, "createTask")) {
    e.fields.title?.[0]?.message;  // "length must be between 1 and 80"; `title` autocompletes
    e.messageFor("priority");      // the first message for a field, or undefined
    e.errors;                      // every FieldError { field, code, message, params }
  } else if (e instanceof ApiError && e.status === 404) {
    // ...
  }
}
```

`isValidationError(e, "createTask")` only matches errors thrown by that method, which is what lets TypeScript know the field names (they are listed per method in the exported `ValidationFields` interface). Nested fields use paths such as `address.zip` and `items[0].name`.

In a browser, unsafe requests (`POST`, `PUT`, `PATCH`, `DELETE`) copy zapline's CSRF cookie into the `x-csrf-token` header, so pages behind [`Csrf`](sessions.md) work without extra code. Pass `csrf: { cookie, header }` if you renamed them, or `csrf: false`.

## No build step: `/_zapline/api.js`

For server-rendered pages, add `.client_js()` to the OpenAPI plugin:

```rust,ignore
App::new()
    .mount((list_tasks, create_task))
    .openapi(OpenApi::new("Tasks", "1.0.0").client_js())
```

The app then serves `/_zapline/api.js`, generated once at startup from the same spec (nothing per request, and nothing at all without `.client_js()`). It is public like `/openapi.json`, even behind `Authenticate::required()`. Use it from any page:

```html
<script type="module">
  import { api, isValidationError } from "/_zapline/api.js";

  const form = document.querySelector("#new-task");
  const title = form.querySelector("[name=title]");
  title.addEventListener("input", () => title.setCustomValidity(""));
  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    try {
      const task = await api.createTask({ title: title.value });
      location.hash = `task-${task.id}`;
    } catch (e) {
      if (isValidationError(e, "createTask")) {
        title.setCustomValidity(e.messageFor("title") ?? "");
        form.reportValidity();
      }
    }
  });
</script>
```

The module carries the same types as JSDoc (`@typedef`, `@param`, `@returns`) and starts with `// @ts-check`. Editors read JSDoc from files on disk, so for autocomplete in your own `.js` files also write a copy next to them with `js = "static/js/api.js"` in `zapline.toml` (or `zapline client -o static/js/api.js`) and import that path. With `// @ts-check` at the top of your script, VS Code then flags a wrong field name or a missing argument just as it would in TypeScript.

## Limitations

- Error responses are typed as `ApiError` / `ValidationError`; a handler's own error `details` stay `unknown`.
- Only the lowest documented 2xx response is typed. A handler that answers 200 or 201 with different bodies needs an explicit `.response::<T>(..)` per status and gets the first.
- Streaming responses (SSE, NDJSON) resolve to the raw `Response`; for SSE in a browser, `new EventSource(url)` is usually simpler.
- Editors cannot see a module that only exists at a URL: `/_zapline/api.js` gives a page the client at runtime, and the on-disk copy gives your editor the types.
