# saas: a complete zapline app

A multi-user todo app that shows the whole [zapline](https://github.com/samsamurai301/zapline)
stack working together, with no Node, no build step and no database server
to install (SQLite until you set `DATABASE_URL`):

- **Accounts**: sign-up, log-in, log-out, and password reset by email, with
  Argon2id hashes and sessions stored in the database.
- **Per-user data**: every query is scoped to the logged-in user; another
  user's todo is a 404.
- **A durable job**: the welcome email is sent by a job stored in the
  database, so it is retried (not lost) when the mail server is down.
- **A scheduled task** that purges expired reset links every hour.
- **Email** over SMTP in production, and a dev inbox at
  <http://localhost:8080/_dev/mail> while developing.
- **htmx pages** rendered on the server, plus a **JSON API** with OpenAPI
  docs at <http://localhost:8080/docs>.
- **A dev console** at <http://localhost:8080/_dev>: every route, the last
  requests, the job queue (retry failed jobs with one click) and the inbox.
- **An admin panel** at <http://localhost:8080/admin> for every table, open to
  the accounts listed in `ADMIN_EMAILS`.
- **13 end-to-end tests**, each on its own in-memory database.

```sh
zapline dev          # http://localhost:8080 , rebuilds and restarts on change
cargo test
```

## How it was built

Three commands generated most of it:

```sh
zapline new saas --template fullstack   # app, todos, migrations, tests, Dockerfile
zapline g auth                          # accounts + password reset, pages, tests
zapline g job send_welcome              # a durable job, registered and tested
```

Then about 100 lines of edits: todos got a `user_id` and a `CurrentUser`
argument (`src/routes/todos.rs`), sign-up enqueues `SendWelcome`
(`src/routes/auth.rs`), the job sends the email (`src/jobs/send_welcome.rs`),
and `src/lib.rs` schedules the cleanup.

## Layout

| Path | What it is |
|---|---|
| `src/lib.rs` | `app(db)`: builds the `App` (database, jobs, templates, static files, docs, schedule) |
| `src/main.rs` | Serves `app(..)` with `DATABASE_URL` or `./app.db` |
| `src/routes/` | One module per resource; each exposes `mount(app)`. `auth.rs` holds accounts |
| `src/jobs/` | Durable jobs, registered in `jobs/mod.rs` |
| `templates/` | MiniJinja (Jinja2) templates; `{% block %}`s double as htmx fragments |
| `migrations/` | SQL migrations, applied in order at startup |
| `static/` | CSS, images and scripts served under `/static` |
| `tests/` | In-process tests with `app(Database::sqlite_memory()).test()` |

## Common tasks

```sh
zapline db new add_due_date          # new migration in migrations/
zapline db migrate                   # apply migrations without starting the server
zapline generate resource project    # CRUD module + tests, registered for you
zapline routes                       # the route table
```

## Postgres

Switch zapline's `sqlite` feature to `postgres` in `Cargo.toml` and set
`DATABASE_URL=postgres://user:pass@host/db`. The queries use `$1` placeholders,
which both SQLite and Postgres understand. In the migration, write
`id BIGSERIAL PRIMARY KEY` instead of `INTEGER PRIMARY KEY AUTOINCREMENT`, and
point the tests at a Postgres with `Database::postgres(url)`.

## Docker

```sh
docker build -t saas .
docker run --rm -p 8080:8080 -v saas-data:/home/app/data saas
```
