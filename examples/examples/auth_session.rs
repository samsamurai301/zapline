//! Server-rendered login with sessions, CSRF protection and a flash cookie.
//!
//!     cargo run -p zapline-examples --example auth_session
//!     open http://localhost:8080   (log in as ada / ada-pass)

use serde::Deserialize;
use zapline::auth::{constant_time_eq, Csrf, CsrfToken};
use zapline::cookies::{secure_cookie, CookieJar, Key};
use zapline::prelude::*;
use zapline::session::{MemoryStore, Session, SessionLayer};

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

fn page(body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><meta charset=utf-8><title>zapline sessions</title>{body}"
    ))
}

async fn home(session: Session, jar: CookieJar, csrf: CsrfToken) -> (CookieJar, Html<String>) {
    // A one-shot "flash" message: read it, then delete the cookie. The cookie
    // holds a code, never HTML, so it cannot inject markup.
    let flash = match jar.get("flash").map(|c| c.value()) {
        Some("welcome") => "<p><em>Welcome back!</em></p>",
        Some("bad_login") => "<p><em>Wrong username or password.</em></p>",
        _ => "",
    };
    let jar = jar.remove("flash");
    let body = match session.get::<String>("user") {
        Some(user) => format!(
            r#"{flash}<p>Hello, {user}!</p>
               <form method="post" action="/logout">{}<button>Log out</button></form>"#,
            csrf.hidden_input()
        ),
        None => format!(r#"{flash}<p>You are not logged in. <a href="/login">Log in</a></p>"#),
    };
    (jar, page(&body))
}

async fn login_form(csrf: CsrfToken) -> Html<String> {
    page(&format!(
        r#"<form method="post" action="/login">{}
             <input name="username" placeholder="username">
             <input name="password" type="password" placeholder="password">
             <button>Log in</button>
           </form>"#,
        csrf.hidden_input()
    ))
}

async fn login(
    session: Session,
    jar: CookieJar,
    Form(creds): Form<Credentials>,
) -> Result<(CookieJar, Redirect)> {
    let ok =
        constant_time_eq(&creds.username, "ada") & constant_time_eq(&creds.password, "ada-pass");
    if !ok {
        let jar = jar.add(secure_cookie("flash", "bad_login"));
        return Ok((jar, Redirect::to("/login")));
    }
    session.regenerate(); // new session id on login (prevents fixation)
    session.insert("user", creds.username)?;
    Ok((
        jar.add(secure_cookie("flash", "welcome")),
        Redirect::to("/"),
    ))
}

async fn logout(session: Session) -> Redirect {
    session.destroy();
    Redirect::to("/")
}

pub fn app() -> App {
    App::new()
        // Use a stable secret key in production (e.g. `Key::from(&bytes)`
        // loaded from a secret store) so sessions survive restarts.
        .cookie_key(Key::generate())
        .wrap(SessionLayer::new(MemoryStore::new()))
        .wrap(Csrf::new())
        .get("/", home)
        .get("/login", login_form)
        .post("/login", login)
        .post("/logout", logout)
}

fn main() -> std::io::Result<()> {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    // Session cookies are `Secure`: browsers accept them on http://localhost.
    println!("listening on http://localhost:{port}");
    app().run(format!("0.0.0.0:{port}"))
}
