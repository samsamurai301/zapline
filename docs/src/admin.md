# Admin panel

`App::admin` serves an admin panel for every table in your database: browse, search, sort and page through rows, and create, edit and delete them through forms. There is no code per table. It reads the schema on each request, so a table added by a new migration shows up on the next reload.

```rust
App::new()
    .database(Database::from_env())
    .admin(|admin| admin) // http://localhost:8080/admin
```

It works on SQLite and Postgres and needs no extra Cargo feature. The `fullstack` template turns it on.

## Who can open it

Without `protect`, the panel behaves like the [dev console](dev-console.md): it exists only in debug builds and answers only requests from the same machine. In a release build that call does nothing, so it can stay in the code.

To use it in production, protect it with a [middleware](middleware.md) that decides who is an admin. The panel is then served in every build, to whoever the middleware lets through:

```rust
use zapline::middleware::Next;

async fn admins_only(mut req: Request, next: Next) -> Response {
    let user = match CurrentUser::from_request_parts(&mut req).await {
        Ok(CurrentUser(user)) => user,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let admins = std::env::var("ADMIN_EMAILS").unwrap_or_default();
    if !admins.split(',').any(|e| e.trim().eq_ignore_ascii_case(&user.email)) {
        return Error::forbidden().into_response();
    }
    next.run(req).await
}

app.admin(|admin| admin.protect(admins_only))
```

[`zapline g auth`](auth.md#accounts-in-one-command-zapline-g-auth) generates exactly this: the accounts listed in `ADMIN_EMAILS` get in, others get `403`, and visitors are sent to `/login`. Calling `admin` again replaces the earlier setup, which is how the generated `auth::mount` protects the panel that the template turned on.

Form posts must come from the same site: a request whose `Origin` doesn't match its `Host` gets `403 cross_origin`, on top of the `SameSite=Lax` session cookie.

## What it shows

| Shown | Not shown |
|---|---|
| Tables whose names are plain identifiers (letters, digits, `_`) | The framework's tables (`_sqlx_migrations`, `zapline_sessions`, `zapline_jobs`) and those passed to `.exclude([...])` |
| Every other column, typed: checkboxes for booleans, number inputs for numbers, text areas for JSON and long text, binary columns as their size | Columns whose names contain `password`, `secret` or `token` or end in `_hash`, and those passed to `.hide([...])` (`"table.column"` or `"column"`). Hidden columns are never read or written. |

Rows are edited by their primary key; tables without a single-column primary key are browse-only, and `.read_only()` makes the whole panel browse-only.

| Setting | Effect |
|---|---|
| `.path("/backoffice")` | Serve it somewhere other than `/admin` |
| `.protect(middleware)` | Put `middleware` in front of every admin route, and serve it in release builds and to other machines |
| `.exclude(["audit_log"])` | Leave tables out |
| `.hide(["users.email", "ip_address"])` | Leave columns out |
| `.read_only()` | No create, edit or delete |

## Lists and forms

The list shows 50 rows per page, newest first (by primary key). Click a column header to sort by it, again to reverse. The search box matches text, JSON and date columns case-insensitively (`%` and `_` are literal), and a number also matches the primary key.

In the forms, an empty field means `NULL` when the column allows it, and the column's default when creating a row. Numbers are checked before anything is written. If the database refuses a change (a unique email taken, a missing required value), the form comes back with the database's message and what you typed.

With both `postgres` and `sqlite` enabled, `admin` uses Postgres; use `admin_in::<sqlx::Sqlite>(..)` for SQLite.
