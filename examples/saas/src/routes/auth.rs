//! Accounts: sign up, log in, log out, password reset by email, and the
//! [`CurrentUser`] extractor.
//!
//! Passwords are hashed with Argon2id; sessions live in the database (table
//! `zapline_sessions`) behind a signed cookie, keyed by `SESSION_SECRET`
//! (debug builds fall back to a development key). Protect a handler by
//! taking `CurrentUser` (401 when logged out), or `Option<CurrentUser>` to
//! treat visitors differently.
//!
//! Reset links are emailed through `SMTP_URL` and point at `APP_URL`. While
//! developing, without `SMTP_URL`, they appear at <http://localhost:8080/_dev/mail>.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zapline::auth::password;
use zapline::cookies::key_from_env;
use zapline::db::{sqlx, Db};
use zapline::extract::FromRequestParts;
use zapline::jobs::Jobs;
use zapline::mail::{Email, Mailer};
use zapline::middleware::rate_limit::TrustedProxies;
use zapline::middleware::{Next, RateLimit, SameOrigin};
use zapline::prelude::*;
use zapline::session::Session;
use zapline::tasks::BackgroundTasks;

use crate::jobs::send_welcome::SendWelcome;
use zapline::view::{context, Hx, Views};

/// Session key holding the logged-in user's id.
const USER_ID: &str = "user_id";
/// Session key holding `users.session_version` at log-in. Changing the
/// password bumps the column, which logs out every other session.
const SESSION_VERSION: &str = "session_version";

/// How long a password-reset link works.
const RESET_TTL: Duration = Duration::from_secs(60 * 60);

/// A registered user.
#[model]
#[derive(sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub email: String,
}

/// Email and password, to sign up or log in (JSON or an HTML form).
#[model]
pub struct Credentials {
    #[validate(email)]
    pub email: String,
    #[validate(length(min = 8, max = 128))]
    pub password: String,
}

/// Asks for a password-reset link.
#[model]
pub struct ForgotPassword {
    #[validate(email)]
    pub email: String,
}

/// Sets a new password with the token from a reset link.
#[model]
pub struct ResetPassword {
    pub token: String,
    #[validate(length(min = 8, max = 128))]
    pub password: String,
}

/// The logged-in user, as a handler argument. Rejects with
/// `401 login_required` when nobody is logged in.
#[derive(Clone, Debug)]
pub struct CurrentUser(pub User);

impl FromRequestParts for CurrentUser {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        let session = Session::from_request_parts(req).await?;
        let db = Db::from_request_parts(req).await?;
        let Some(id) = session.get::<i64>(USER_ID) else {
            return Err(login_required());
        };
        let row = sqlx::query_as::<_, (i64, String, i64)>(
            "SELECT id, email, session_version FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&*db)
        .await?;
        match row {
            Some((id, email, version)) if session.get::<i64>(SESSION_VERSION) == Some(version) => {
                Ok(CurrentUser(User { id, email }))
            }
            // The account was deleted, or its password changed since this
            // session logged in: forget the session.
            _ => {
                session.destroy();
                Err(login_required())
            }
        }
    }
}

/// At most 10 sign-ups, log-ins and reset requests a minute per client IP:
/// slows password guessing and floods of reset emails. Behind a proxy, list
/// it in `TRUSTED_PROXIES`, or every client shares the proxy's allowance.
fn throttle() -> RateLimit {
    let proxies = TrustedProxies::from_env().unwrap_or_else(|e| panic!("{}", e.message()));
    RateLimit::per_ip(10, Duration::from_secs(60)).trusted_proxies(proxies)
}

fn login_required() -> Error {
    Error::unauthorized().with_code("login_required")
}

/// Adds sessions, the mailer and the account routes.
pub fn mount(app: App) -> App {
    let key = key_from_env("SESSION_SECRET").unwrap_or_else(|e| panic!("{}", e.message()));
    let mailer = Mailer::from_env().unwrap_or_else(|e| panic!("{}", e.message()));
    app.cookie_key(key)
        // Form posts and fetches from other sites can't act with a user's
        // session cookie (403): CSRF protection without tokens in forms.
        .wrap(SameOrigin::new())
        // `Secure` cookies in release builds (HTTPS); plain HTTP while developing.
        .database_sessions(|layer| layer.secure(!cfg!(debug_assertions)))
        .mailer(mailer)
        // Sign-up, log-in and password resets, rate-limited per client.
        .nest(
            "/",
            Routes::new()
                .mount((signup, login, forgot, reset))
                .mount((signup_form, login_form, forgot_form, reset_form))
                .wrap(throttle()),
        )
        .mount((logout, me))
        .mount((
            signup_page,
            login_page,
            logout_form,
            forgot_page,
            reset_page,
        ))
        .admin(|admin| admin.protect(admins_only).exclude(["password_resets"]))
}

/// Lets only the accounts listed in `ADMIN_EMAILS` into the admin panel,
/// once their owner has proved the address with a reset link: anyone can
/// sign up with any address.
async fn admins_only(mut req: Request, next: Next) -> Response {
    let user = match CurrentUser::from_request_parts(&mut req).await {
        Ok(CurrentUser(user)) => user,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let admins = std::env::var("ADMIN_EMAILS").unwrap_or_default();
    if !admins
        .split(',')
        .any(|email| email.trim().eq_ignore_ascii_case(&user.email))
    {
        return Error::forbidden().into_response();
    }
    let Ok(db) = Db::from_request_parts(&mut req).await else {
        return Error::forbidden().into_response();
    };
    let verified: Option<bool> =
        sqlx::query_scalar("SELECT email_verified FROM users WHERE id = $1")
            .bind(user.id)
            .fetch_optional(&*db)
            .await
            .unwrap_or(None);
    if verified != Some(true) {
        return Error::forbidden()
            .with_code("email_unverified")
            .into_response();
    }
    next.run(req).await
}

// ---- accounts -----------------------------------------------------------------

/// Creates the account and logs it in.
async fn register(db: &Db, jobs: &Jobs, session: &Session, creds: &Credentials) -> Result<User> {
    let email = creds.email.trim().to_lowercase();
    let hash = password::hash(creds.password.as_str()).await?;
    let user = sqlx::query_as::<_, User>(
        "INSERT INTO users (email, password_hash) VALUES ($1, $2) RETURNING id, email",
    )
    .bind(&email)
    .bind(&hash)
    .fetch_one(&**db)
    .await
    .map_err(|e| match Error::from(e) {
        e if e.code() == Some("unique_violation") => {
            Error::conflict("this email is already registered").with_code("email_taken")
        }
        e => e,
    })?;
    // Sent in the background by a durable job, retried if SMTP is down.
    jobs.enqueue(SendWelcome { user_id: user.id }).await?;
    start_session(db, session, &user).await?;
    Ok(user)
}

/// Checks the password and logs the user in.
async fn authenticate(db: &Db, session: &Session, creds: &Credentials) -> Result<User> {
    let email = creds.email.trim().to_lowercase();
    let row = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT id, email, password_hash FROM users WHERE email = $1",
    )
    .bind(&email)
    .fetch_optional(&**db)
    .await?;
    let Some((id, email, hash)) = row else {
        // Take as long as a real check, so timing doesn't reveal unknown emails.
        password::verify_missing(creds.password.as_str()).await?;
        return Err(invalid_credentials());
    };
    if !password::verify(creds.password.as_str(), hash).await? {
        return Err(invalid_credentials());
    }
    let user = User { id, email };
    start_session(db, session, &user).await?;
    Ok(user)
}

async fn start_session(db: &Db, session: &Session, user: &User) -> Result<()> {
    let version: i64 = sqlx::query_scalar("SELECT session_version FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(&**db)
        .await?;
    // A fresh session id on login prevents session fixation.
    session.regenerate();
    session.insert(USER_ID, user.id)?;
    session.insert(SESSION_VERSION, version)
}

fn invalid_credentials() -> Error {
    Error::new(StatusCode::UNAUTHORIZED, "wrong email or password").with_code("invalid_credentials")
}

// ---- password reset --------------------------------------------------------------

/// Emails a reset link if `email` has an account; does nothing otherwise, so
/// callers answer the same either way.
async fn send_reset_link(
    db: &Db,
    mail: &Mailer,
    tasks: &BackgroundTasks,
    email: &str,
) -> Result<()> {
    let email = email.trim().to_lowercase();
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&**db)
        .await?;
    let Some(id) = id else {
        return Ok(());
    };
    let token = password::new_token()?;
    sqlx::query(
        "INSERT INTO password_resets (token_hash, user_id, expires_at) VALUES ($1, $2, $3)",
    )
    .bind(password::hash_token(&token))
    .bind(id)
    .bind(unix_now() + RESET_TTL.as_secs() as i64)
    .execute(&**db)
    .await?;
    let link = format!("{}/reset?token={token}", app_url());
    let message = Email::to(email)
        .subject("Reset your password")
        .text(format!(
            "Someone asked to reset the password for this account.\n\n\
         Choose a new password here (the link works for an hour, once):\n{link}\n\n\
         If it wasn't you, ignore this email: your password stays the same.\n"
        ));
    let mail = mail.clone();
    // Sent after the response, so its timing doesn't reveal which emails have accounts.
    tasks.spawn(async move { mail.send(message).await });
    Ok(())
}

/// Uses up the reset token, sets the new password and logs the user in.
async fn reset_password(db: &Db, session: &Session, form: &ResetPassword) -> Result<User> {
    // Deleting the token first means it works exactly once, even with two tabs.
    let user_id: Option<i64> = sqlx::query_scalar(
        "DELETE FROM password_resets WHERE token_hash = $1 AND expires_at > $2 RETURNING user_id",
    )
    .bind(password::hash_token(form.token.trim()))
    .bind(unix_now())
    .fetch_optional(&**db)
    .await?;
    let Some(user_id) = user_id else {
        return Err(
            Error::bad_request("this reset link is invalid or has expired")
                .with_code("invalid_token"),
        );
    };
    let hash = password::hash(form.password.as_str()).await?;
    let user = sqlx::query_as::<_, User>(
        // Bumping session_version logs out every session of this account.
        // The link came by email, so the address is the user's.
        "UPDATE users SET password_hash = $1, session_version = session_version + 1, \
         email_verified = TRUE WHERE id = $2 RETURNING id, email",
    )
    .bind(&hash)
    .bind(user_id)
    .fetch_one(&**db)
    .await?;
    // Other links sent to this account stop working too.
    sqlx::query("DELETE FROM password_resets WHERE user_id = $1")
        .bind(user_id)
        .execute(&**db)
        .await?;
    start_session(db, session, &user).await?;
    Ok(user)
}

/// The public address of the app, for links in emails (`APP_URL`).
pub fn app_url() -> String {
    let url = std::env::var("APP_URL").unwrap_or_else(|_| "http://localhost:8080".into());
    url.trim_end_matches('/').to_string()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ---- JSON API ------------------------------------------------------------------

/// Create an account and log in.
#[post("/api/auth/signup", tag = "auth", status = 201)]
async fn signup(
    db: Db,
    jobs: Jobs,
    session: Session,
    Valid(Json(creds)): Valid<Json<Credentials>>,
) -> Result<(StatusCode, Json<User>)> {
    let user = register(&db, &jobs, &session, &creds).await?;
    Ok((StatusCode::CREATED, Json(user)))
}

/// Log in.
#[post("/api/auth/login", tag = "auth")]
async fn login(db: Db, session: Session, Json(creds): Json<Credentials>) -> Result<Json<User>> {
    Ok(Json(authenticate(&db, &session, &creds).await?))
}

/// Log out.
#[post("/api/auth/logout", tag = "auth", status = 204)]
async fn logout(session: Session) -> StatusCode {
    session.destroy();
    StatusCode::NO_CONTENT
}

/// The logged-in user.
#[get("/api/auth/me", tag = "auth")]
async fn me(CurrentUser(user): CurrentUser) -> Json<User> {
    Json(user)
}

/// Email a password-reset link. Answers 202 whether or not the account exists.
#[post("/api/auth/forgot", tag = "auth", status = 202)]
async fn forgot(
    db: Db,
    mail: Mailer,
    tasks: BackgroundTasks,
    Valid(Json(form)): Valid<Json<ForgotPassword>>,
) -> Result<StatusCode> {
    send_reset_link(&db, &mail, &tasks, &form.email).await?;
    Ok(StatusCode::ACCEPTED)
}

/// Set a new password with a reset token, and log in.
#[post("/api/auth/reset", tag = "auth")]
async fn reset(
    db: Db,
    session: Session,
    Valid(Json(form)): Valid<Json<ResetPassword>>,
) -> Result<Json<User>> {
    Ok(Json(reset_password(&db, &session, &form).await?))
}

// ---- HTML (htmx) ---------------------------------------------------------------

/// The sign-up page.
#[get("/signup", tag = "pages")]
async fn signup_page(views: Views) -> Result<Html<String>> {
    views.render("auth/signup.html", context! {})
}

/// The log-in page.
#[get("/login", tag = "pages")]
async fn login_page(views: Views) -> Result<Html<String>> {
    views.render("auth/login.html", context! {})
}

/// Sign up from the page's form: go home, or show what went wrong.
#[post("/signup", tag = "pages")]
async fn signup_form(
    views: Views,
    db: Db,
    jobs: Jobs,
    session: Session,
    Form(creds): Form<Credentials>,
) -> Result<Response> {
    let result = match creds.validate() {
        Ok(()) => register(&db, &jobs, &session, &creds).await.map(|_| ()),
        Err(_) => Err(Error::unprocessable(
            "enter a valid email and a password of at least 8 characters",
        )),
    };
    form_result(&views, "auth/signup.html", &creds.email, result)
}

/// Log in from the page's form.
#[post("/login", tag = "pages")]
async fn login_form(
    views: Views,
    db: Db,
    session: Session,
    Form(creds): Form<Credentials>,
) -> Result<Response> {
    let result = authenticate(&db, &session, &creds).await.map(|_| ());
    form_result(&views, "auth/login.html", &creds.email, result)
}

/// Log out and go to the log-in page.
#[post("/logout", tag = "pages")]
async fn logout_form(session: Session) -> Response {
    session.destroy();
    Hx::redirect("/login").into_response()
}

/// The "forgot your password?" page.
#[get("/forgot", tag = "pages")]
async fn forgot_page(views: Views) -> Result<Html<String>> {
    views.render("auth/forgot.html", context! {})
}

/// Send the reset link and say so, whether or not the account exists.
#[post("/forgot", tag = "pages")]
async fn forgot_form(
    views: Views,
    db: Db,
    mail: Mailer,
    tasks: BackgroundTasks,
    Form(form): Form<ForgotPassword>,
) -> Result<Html<String>> {
    if form.validate().is_err() {
        let error = "enter a valid email";
        return views.render_block(
            "auth/forgot.html",
            "form",
            context! { error, email => form.email },
        );
    }
    send_reset_link(&db, &mail, &tasks, &form.email).await?;
    views.render_block(
        "auth/forgot.html",
        "form",
        context! { sent => true, email => form.email },
    )
}

/// The link in the reset email: a form for the new password.
#[get("/reset", tag = "pages")]
async fn reset_page(views: Views, Query(link): Query<ResetLink>) -> Result<Html<String>> {
    views.render("auth/reset.html", context! { token => link.token })
}

/// The `?token=` of a reset link.
#[model]
pub struct ResetLink {
    pub token: String,
}

/// Set the new password from the page's form, then go home logged in.
#[post("/reset", tag = "pages")]
async fn reset_form(
    views: Views,
    db: Db,
    session: Session,
    Form(form): Form<ResetPassword>,
) -> Result<Response> {
    let result = match form.validate() {
        Ok(()) => reset_password(&db, &session, &form).await.map(|_| ()),
        Err(_) => Err(Error::unprocessable(
            "choose a password of at least 8 characters",
        )),
    };
    match result {
        Ok(()) => Ok(Hx::redirect("/").into_response()),
        Err(e) if e.status().is_client_error() => {
            let error = e.message().to_string();
            Ok(views
                .render_block(
                    "auth/reset.html",
                    "form",
                    context! { error, token => form.token },
                )?
                .into_response())
        }
        Err(e) => Err(e),
    }
}

/// Success sends htmx home; a client error re-renders the form's `form`
/// block with the message (status 200, so htmx swaps it in).
fn form_result(views: &Views, template: &str, email: &str, result: Result<()>) -> Result<Response> {
    match result {
        Ok(()) => Ok(Hx::redirect("/").into_response()),
        Err(e) if e.status().is_client_error() => {
            let error = e.message().to_string();
            Ok(views
                .render_block(template, "form", context! { error, email })?
                .into_response())
        }
        Err(e) => Err(e),
    }
}
