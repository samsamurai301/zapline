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
| `ValidForm(form): ValidForm<T>` | A validated form post as `Result<T, Invalid>`: never rejects bad input ([Forms](#forms-errors-next-to-each-field)) |
| `views.render_invalid(name, ctx, invalid)` | `422` with `errors` and `old` in the context; the `form` block for htmx |
| `flash: Flash` | `flash.success(msg)` / `.error` / `.info` / `.warning`, `flash.redirect(to)`; `{% for m in flash %}` in templates ([Flash messages](#flash-messages)) |
| `HxRequest` | Extractor: `htmx`, `boosted`, `history_restore`, `target`, `trigger` from the `HX-*` headers |
| `Hx::redirect / refresh / trigger / retarget / push_url` | htmx response headers; `.with(response)` adds them to any response |
| `context! { a, b => expr }` | Build a template context; any `Serialize` value works too |

Values are HTML-escaped automatically in `.html` templates. Use `{{ value | safe }}` only for HTML you produced yourself.

## Forms: errors next to each field

A rejected form should come back with a message next to each field and what the user typed still in the inputs. `ValidForm<T>` parses and validates a form post *without* rejecting it, and `views.render_invalid` shows the form again:

```rust,ignore
use serde::{Deserialize, Serialize};
use validator::Validate;
use zapline::prelude::*;
use zapline::session::Flash;
use zapline::view::{context, FormErrors, Invalid, ValidForm, Views};

#[derive(Deserialize, Serialize, Validate)]
struct NewPost {
    #[validate(length(min = 1, max = 200))]
    title: String,
    views: i64,
}

#[get("/posts/new")]
async fn new_post(views: Views) -> Result<Html<String>> {
    views.render("posts/new.html", context! {})
}

#[post("/posts")]
async fn create(views: Views, flash: Flash, db: Db, ValidForm(form): ValidForm<NewPost>) -> Result<Response> {
    let post = match form {
        Ok(post) => post,
        // 422 with `errors` and `old` in the template context.
        Err(invalid) => return views.render_invalid("posts/new.html", context! {}, invalid),
    };
    if title_taken(&db, &post.title).await? {
        // Checks only the handler can make render the same way.
        let errors = FormErrors::one("title", "is already used");
        return views.render_invalid("posts/new.html", context! {}, Invalid::new(errors, &post));
    }
    insert(&db, &post).await?;
    flash.success("Post created");
    Ok(flash.redirect("/posts")) // 303, or HX-Redirect for htmx
}
```

`templates/posts/new.html`:

```html
{% extends "base.html" %}
{% block content %}
<form id="post-form" hx-post="/posts">
  {% block form %}
  {% if errors._form %}<p role="alert">{{ errors._form }}</p>{% endif %}
  <label>Title <input name="title" value="{{ old.title }}" aria-invalid="{{ 'true' if errors.title else 'false' }}"></label>
  {% if errors.title %}<small class="error">{{ errors.title }}</small>{% endif %}
  <label>Views <input name="views" type="number" value="{{ old.views }}"></label>
  {% if errors.views %}<small class="error">{{ errors.views }}</small>{% endif %}
  <button>Save</button>
  {% endblock %}
</form>
{% endblock %}
```

- **`ValidForm<T>`** gives the handler `Ok(T)`, or `Err(Invalid { errors, old })`. Fields that don't parse get a message (`views=abc`: "must be a number"; empty or missing: "is required"); once every field parses, the `validator` rules run, with the same messages as the JSON 422 ([Validation](validation.md)). Only a request that isn't a form at all (wrong `Content-Type`, too large) is rejected, like `Form<T>`.
- **`errors`** is a `FormErrors`: `{{ errors.title }}` is the first message for `title`, undefined (falsy) when the field is fine; `{{ errors._form }}` holds errors about the whole form. Build one by hand with `FormErrors::one(field, msg)` / `.add(field, msg)`, or from any error with `FormErrors::from_error(&err)`.
- **`old`** holds the submitted values by field name, HTML-escaped like any value. Fields whose names look secret (`password`, `token`, `secret`, `*_hash`) are never echoed back.
- Both are empty globals in every template, so the same template renders the blank form and the rejected one.
- **`render_invalid`** answers `422 Unprocessable Entity`. For a normal form post it renders the whole page. For htmx it renders only `{% block form %}` (the *inside* of the `<form>`; the whole template if it has no such block). When the form has an `id` but posts somewhere else (`hx-target="#list"`, a create form above a list), it adds `HX-Retarget: #post-form` and `HX-Reswap: innerHTML`, so the messages land in the form instead of the list.

htmx does not swap error responses by default. Allow `422` once, in the layout:

```html
<script>
  // Invalid forms come back as 422 with the form re-rendered: swap it in.
  document.addEventListener("htmx:beforeSwap", (e) => {
    if (e.detail.xhr.status === 422) { e.detail.shouldSwap = true; e.detail.isError = false; }
  });
</script>
```

Apps created with `zapline new --template fullstack` have this in `templates/base.html`, and `zapline g resource` and `zapline g auth` generate forms that work this way.

## Flash messages

With sessions (`cookies` feature and a [`SessionLayer`](sessions.md)), `Flash` queues one-time messages for the next page, and every template sees the waiting ones as `flash`:

```rust,ignore
use zapline::session::Flash;

#[post("/posts/{id}/delete")]
async fn delete(flash: Flash, db: Db, Path(id): Path<i64>) -> Result<Response> {
    delete_post(&db, id).await?;
    flash.success("Post deleted"); // also .error(..), .info(..), .warning(..)
    Ok(flash.redirect("/posts"))
}
```

```html
{# base.html #}
<div id="flash">
  {% for m in flash %}<p role="status" class="flash flash-{{ m.level }}">{{ m.message }}</p>{% endfor %}
</div>
```

- A message stays in the session until something reads it: the first template that uses `flash` (later reads in the same request see the same list), or `flash.take()` in a handler (for JSON clients). A message queued while rendering a page shows on that page.
- `flash.redirect(to)` answers `303 See Other`, or `HX-Redirect` for htmx (which then loads the whole page, where the flash shows).
- An htmx request answered in place (a fragment, no redirect) gets its new messages right away as `HX-Trigger: {"flash": {"messages": [{"level": "success", "message": "Saved"}]}}`, merged with any `HX-Trigger` the handler set. Show them as toasts with a few lines of script listening for the `flash` event (`e.detail.messages`); the fullstack layout does.
- No cost when unused: `flash` is a lazy template global. A page reads the session store only when a small marker cookie (`zapline.sid.flash`) says a message is waiting.

## Hot reload

In debug builds templates are read from disk on every render: edit a template, refresh the browser, no rebuild. Release builds load each template once. Copy `templates/` next to the binary when you deploy (the generated Dockerfiles do).

## Errors

A template error (missing template, syntax error, unknown filter) is a 500. Under `zapline dev` (which sets `ZAPLINE_DEV=1`) the response carries the full message with file and line; in production it is logged and the client gets a generic message.
