# Views & htmx

`zapline::view` (feature `views`) renders HTML with [MiniJinja](https://docs.rs/minijinja) templates (Jinja2 syntax: `{{ value }}`, `{% for %}`, `{% extends %}`, `{% block %}`). It is built for [htmx](https://htmx.org): the same template renders a whole page for a normal visit and just one block for an htmx request, so there is no separate "partials" folder to keep in sync.

```rust,ignore
use zapline::prelude::*;
use zapline::view::{context, Hx, HxRequest, Views};

#[get("/todos")]
async fn todos(views: Views, hx: HxRequest, db: Db) -> Result<Html<String>> {
    let todos = load_todos(&db).await?;
    views.page_or_block(&hx, "todos.html", "list", context! { todos })
}

#[post("/todos")]
async fn add(db: Db, Form(new): Form<NewTodo>) -> Result<Response> {
    insert_todo(&db, new).await?;
    Ok(Hx::trigger("todos-changed").with(StatusCode::CREATED))
}

fn main() -> std::io::Result<()> {
    App::new().views("templates").mount((todos, add)).run("0.0.0.0:8080")
}
```

`templates/todos.html`:

```html
{% extends "base.html" %}
{% block content %}
  <form hx-post="/todos" hx-swap="none">
    <input name="title" required> <button>Add</button>
  </form>
  <ul id="list" hx-get="/todos" hx-trigger="todos-changed from:body">
    {% block list %}
      {% for t in todos %}<li>{{ t.title }}</li>{% endfor %}
    {% endblock %}
  </ul>
{% endblock %}
```

## API

| | |
|---|---|
| `App::views("templates")` | Load templates from a directory (relative to the project root) |
| `App::views_with("templates", \|env\| { .. })` | Same, plus filters, functions and globals on the MiniJinja `Environment` |
| `views.render(name, ctx)` | The whole template → `Html<String>` |
| `views.render_block(name, block, ctx)` | Only `{% block block %}` (with the same context) |
| `views.page_or_block(&hx, name, block, ctx)` | `render_block` for htmx requests, `render` otherwise (boosted links and history restores get the full page) |
| `HxRequest` | Extractor: `htmx`, `boosted`, `history_restore`, `target`, `trigger` from the `HX-*` headers |
| `Hx::redirect / refresh / trigger / retarget / push_url` | htmx response headers; `.with(response)` adds them to any response |
| `context! { a, b => expr }` | Build a template context; any `Serialize` value works too |

Values are HTML-escaped automatically in `.html` templates. Use `{{ value | safe }}` only for HTML you produced yourself.

## Hot reload

In debug builds templates are read from disk on every render: edit a template and refresh, no rebuild. Under [`zapline dev`](cli.md#the-dev-loop-zapline-dev) you don't even refresh: open pages reload by themselves when a template changes (also with `--release`, since `ZAPLINE_DEV=1` keeps reloading on). Release builds load each template once. Copy `templates/` next to the binary when you deploy (the generated Dockerfiles do).

## Errors

A template error (missing template, syntax error, unknown filter) is a 500. Under `zapline dev` (which sets `ZAPLINE_DEV=1`) the response carries the full message with file and line; in production it is logged and the client gets a generic message.
