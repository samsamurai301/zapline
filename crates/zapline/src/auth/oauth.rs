//! "Log in with GitHub / Google": the OAuth 2.0 authorization-code flow
//! with PKCE and a state cookie, ending in a verified [`OAuthUser`].
//!
//! ```no_run
//! use zapline::auth::oauth::{OAuth, OAuthLogin};
//! use zapline::prelude::*;
//!
//! // The provider redirects here after the user agrees.
//! #[get("/auth/github/callback")]
//! async fn github_done(login: OAuthLogin) -> Result<Redirect> {
//!     let user = login.user;
//!     // Find or create the account (by provider + id, or by verified email),
//!     // start a session...
//!     println!("{} {:?} verified={}", user.id, user.email, user.email_verified);
//!     Ok(Redirect::to(login.next.as_deref().unwrap_or("/")))
//! }
//!
//! fn main() -> zapline::Result<()> {
//!     App::new()
//!         // Links to /auth/github start the login.
//!         .oauth(OAuth::from_env("github")?)
//!         .mount(github_done)
//!         .run("0.0.0.0:8080")?;
//!     Ok(())
//! }
//! ```
//!
//! [`App::oauth`] serves `GET /auth/{provider}`, which redirects to the
//! provider (`?next=/path` is carried through to [`OAuthLogin::next`]). The
//! provider sends the browser back to `/auth/{provider}/callback`, where
//! your handler takes an [`OAuthLogin`]: the extractor checks the state,
//! exchanges the code (with the PKCE verifier) and fetches the profile.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, SET_COOKIE, USER_AGENT};
use http::{HeaderValue, Method, Request as HttpRequest};
use http_body_util::Full;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::dev::internal;
use crate::extract::FromRequestParts;
use crate::http_client::fetch;
use crate::response::{IntoResponse, Redirect, Response};
use crate::{App, Error, Request, Result, Routes};

/// How long a started login may take, in seconds.
const LOGIN_TTL: u32 = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    GitHub,
    Google,
    Custom,
}

/// An OAuth provider: where to send users, and the app's credentials there.
#[derive(Clone)]
pub struct OAuth {
    name: String,
    flavor: Flavor,
    client_id: String,
    client_secret: String,
    auth_url: String,
    token_url: String,
    userinfo_url: String,
    scopes: Vec<String>,
    redirect_url: Option<String>,
}

impl fmt::Debug for OAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuth")
            .field("name", &self.name)
            .field("client_id", &self.client_id)
            .field("auth_url", &self.auth_url)
            .finish_non_exhaustive()
    }
}

impl OAuth {
    /// GitHub (create an app at <https://github.com/settings/developers>
    /// with callback `https://your.app/auth/github/callback`). Asks for the
    /// profile and email addresses.
    ///
    /// ```
    /// let github = zapline::auth::oauth::OAuth::github("client-id", "client-secret");
    /// ```
    pub fn github(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        OAuth {
            name: "github".into(),
            flavor: Flavor::GitHub,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            auth_url: "https://github.com/login/oauth/authorize".into(),
            token_url: "https://github.com/login/oauth/access_token".into(),
            userinfo_url: "https://api.github.com/user".into(),
            scopes: vec!["read:user".into(), "user:email".into()],
            redirect_url: None,
        }
    }

    /// Google (create an OAuth client at
    /// <https://console.cloud.google.com/apis/credentials> with redirect URI
    /// `https://your.app/auth/google/callback`). Asks for `openid email profile`.
    ///
    /// ```
    /// let google = zapline::auth::oauth::OAuth::google("client-id", "client-secret");
    /// ```
    pub fn google(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        OAuth {
            name: "google".into(),
            flavor: Flavor::Google,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            userinfo_url: "https://openidconnect.googleapis.com/v1/userinfo".into(),
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
            redirect_url: None,
        }
    }

    /// Any other OAuth 2.0 / OpenID Connect provider (GitLab, Microsoft,
    /// Discord, your own...). The profile is read from the usual fields:
    /// `sub` or `id`, `email`, `email_verified`, `name`, `picture` or
    /// `avatar_url`.
    ///
    /// ```
    /// use zapline::auth::oauth::OAuth;
    ///
    /// let gitlab = OAuth::custom(
    ///     "gitlab",
    ///     "https://gitlab.com/oauth/authorize",
    ///     "https://gitlab.com/oauth/token",
    ///     "https://gitlab.com/oauth/userinfo",
    /// )
    /// .credentials("client-id", "client-secret")
    /// .scopes(["openid", "email", "profile"]);
    /// ```
    pub fn custom(
        name: impl Into<String>,
        auth_url: impl Into<String>,
        token_url: impl Into<String>,
        userinfo_url: impl Into<String>,
    ) -> Self {
        OAuth {
            name: name.into(),
            flavor: Flavor::Custom,
            client_id: String::new(),
            client_secret: String::new(),
            auth_url: auth_url.into(),
            token_url: token_url.into(),
            userinfo_url: userinfo_url.into(),
            scopes: Vec::new(),
            redirect_url: None,
        }
    }

    /// `github` or `google` with credentials from `{NAME}_CLIENT_ID` and
    /// `{NAME}_CLIENT_SECRET` (e.g. `GITHUB_CLIENT_ID`).
    ///
    /// # Errors
    ///
    /// For another name, or when a variable is missing.
    ///
    /// ```no_run
    /// let github = zapline::auth::oauth::OAuth::from_env("github")?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn from_env(name: &str) -> Result<Self> {
        let var = |suffix: &str| {
            let key = format!("{}_{suffix}", name.to_ascii_uppercase());
            std::env::var(&key)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::internal(format!("{name} login needs {key}")))
        };
        let (id, secret) = (var("CLIENT_ID")?, var("CLIENT_SECRET")?);
        match name {
            "github" => Ok(Self::github(id, secret)),
            "google" => Ok(Self::google(id, secret)),
            _ => Err(Error::internal(format!(
                "no built-in OAuth provider {name:?}: use OAuth::custom"
            ))),
        }
    }

    /// The app's client id and secret at the provider.
    ///
    /// ```
    /// # use zapline::auth::oauth::OAuth;
    /// let p = OAuth::custom("x", "https://x/auth", "https://x/token", "https://x/me").credentials("id", "secret");
    /// ```
    #[must_use]
    pub fn credentials(
        mut self,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        self.client_id = client_id.into();
        self.client_secret = client_secret.into();
        self
    }

    /// Replace the provider's URLs: for GitHub Enterprise, a regional
    /// endpoint, or a test double.
    ///
    /// ```
    /// # use zapline::auth::oauth::OAuth;
    /// let ghe = OAuth::github("id", "secret").endpoints(
    ///     "https://github.acme.dev/login/oauth/authorize",
    ///     "https://github.acme.dev/login/oauth/access_token",
    ///     "https://github.acme.dev/api/v3/user",
    /// );
    /// ```
    #[must_use]
    pub fn endpoints(
        mut self,
        auth_url: impl Into<String>,
        token_url: impl Into<String>,
        userinfo_url: impl Into<String>,
    ) -> Self {
        self.auth_url = auth_url.into();
        self.token_url = token_url.into();
        self.userinfo_url = userinfo_url.into();
        self
    }

    /// Replace the requested scopes.
    ///
    /// ```
    /// # use zapline::auth::oauth::OAuth;
    /// let github = OAuth::github("id", "secret").scopes(["read:user", "user:email", "repo"]);
    /// ```
    #[must_use]
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// The callback URL registered at the provider. Default:
    /// `{APP_URL}/auth/{name}/callback`, or the request's own origin when
    /// `APP_URL` is unset.
    ///
    /// ```
    /// # use zapline::auth::oauth::OAuth;
    /// let github = OAuth::github("id", "secret").redirect_url("https://acme.dev/auth/github/callback");
    /// ```
    #[must_use]
    pub fn redirect_url(mut self, url: impl Into<String>) -> Self {
        self.redirect_url = Some(url.into());
        self
    }

    /// The name in the app's URLs: `/auth/{name}`.
    ///
    /// ```
    /// # use zapline::auth::oauth::OAuth;
    /// assert_eq!(OAuth::github("id", "secret").name(), "github");
    /// ```
    pub fn name(&self) -> &str {
        &self.name
    }

    fn callback_url(&self, req: &Request) -> String {
        if let Some(url) = &self.redirect_url {
            return url.clone();
        }
        let base = std::env::var("APP_URL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| origin(req));
        format!("{}/auth/{}/callback", base.trim_end_matches('/'), self.name)
    }

    fn cookie_name(&self) -> String {
        format!("zapline_oauth_{}", self.name)
    }
}

/// `scheme://host` of the request, honoring `X-Forwarded-Proto`.
fn origin(req: &Request) -> String {
    let headers = req.headers();
    let host = headers
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().scheme_str())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

/// The profile of someone who logged in.
#[derive(Clone, Debug, serde::Serialize)]
pub struct OAuthUser {
    /// The provider's name (`github`, `google`, ...).
    pub provider: String,
    /// Their stable id at the provider. Link accounts by `(provider, id)`.
    pub id: String,
    /// Their email address, if the provider shared one.
    pub email: Option<String>,
    /// Whether the provider verified that email. Only match existing
    /// accounts by email when this is `true`.
    pub email_verified: bool,
    /// Display name.
    pub name: Option<String>,
    /// Avatar URL.
    pub avatar_url: Option<String>,
    /// The provider's whole profile response.
    pub raw: Value,
}

/// A finished login, extracted in the handler at
/// `/auth/{provider}/callback`. Fails with `400` when the state doesn't
/// match (a forged or expired attempt), `401` when the user declined, and
/// `503` when the provider can't be reached.
#[derive(Clone, Debug)]
pub struct OAuthLogin {
    /// Who logged in.
    pub user: OAuthUser,
    /// The provider's access token, to call its API for this user.
    pub access_token: String,
    /// The `?next=` path given to `/auth/{provider}`, if any (always a
    /// local path).
    pub next: Option<String>,
}

#[derive(Clone, Default)]
struct Providers(Arc<RwLock<HashMap<String, OAuth>>>);

impl Providers {
    fn get(&self, name: &str) -> Option<OAuth> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }
}

impl App {
    /// Offer logging in with `provider`: `GET /auth/{name}` starts it, and
    /// your handler at `/auth/{name}/callback` takes an [`OAuthLogin`].
    /// Call once per provider.
    ///
    /// ```
    /// use zapline::auth::oauth::OAuth;
    ///
    /// let app = zapline::App::new()
    ///     .oauth(OAuth::github("id", "secret"))
    ///     .oauth(OAuth::google("id", "secret"));
    /// ```
    pub fn oauth(mut self, provider: OAuth) -> Self {
        let providers = self
            .extensions_mut()
            .get_or_insert_default::<Providers>()
            .clone();
        let name = provider.name.clone();
        providers
            .0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.clone(), provider);
        self.state(providers)
            .mount(internal(Routes::new().get(&format!("/auth/{name}"), start)))
    }
}

async fn start(req: Request) -> Response {
    let Some(provider) = provider_for(&req, "") else {
        return Error::not_found().into_response();
    };
    let state = random_token();
    let verifier = random_token();
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let next = query_param(&req, "next").filter(|n| is_local_path(n));

    let mut url = format!(
        "{}{}response_type=code&client_id={}&redirect_uri={}&state={state}\
         &code_challenge={challenge}&code_challenge_method=S256",
        provider.auth_url,
        if provider.auth_url.contains('?') {
            '&'
        } else {
            '?'
        },
        enc(&provider.client_id),
        enc(&provider.callback_url(&req)),
    );
    if !provider.scopes.is_empty() {
        url.push_str("&scope=");
        url.push_str(&enc(&provider.scopes.join(" ")));
    }

    let secure = origin(&req).starts_with("https://")
        || std::env::var("APP_URL").is_ok_and(|u| u.starts_with("https://"));
    let cookie = format!(
        "{}={state}.{verifier}.{}; Path=/auth; Max-Age={LOGIN_TTL}; HttpOnly; SameSite=Lax{}",
        provider.cookie_name(),
        enc(next.as_deref().unwrap_or("")),
        if secure { "; Secure" } else { "" }
    );
    let mut res = Redirect::to(&url).into_response();
    if let Ok(cookie) = HeaderValue::try_from(cookie) {
        res.headers_mut().insert(SET_COOKIE, cookie);
    }
    res
}

/// The provider registered for this request's path, `/auth/{name}{suffix}`.
fn provider_for(req: &Request, suffix: &str) -> Option<OAuth> {
    let name = req.path().strip_prefix("/auth/")?.strip_suffix(suffix)?;
    req.state::<Providers>()?.get(name)
}

impl FromRequestParts for OAuthLogin {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        // Not configured (say, no GOOGLE_CLIENT_ID): the page doesn't exist.
        let provider = provider_for(req, "/callback").ok_or_else(Error::not_found)?;
        if let Some(error) = query_param(req, "error") {
            let detail = query_param(req, "error_description").unwrap_or(error);
            return Err(Error::new(
                http::StatusCode::UNAUTHORIZED,
                format!("login with {} failed: {detail}", provider.name),
            )
            .with_code("oauth_denied"));
        }
        let bad_state = || {
            Error::bad_request("this login link expired or was not started here; try again")
                .with_code("invalid_oauth_state")
        };
        let code = query_param(req, "code").ok_or_else(bad_state)?;
        let state = query_param(req, "state").ok_or_else(bad_state)?;
        let cookie = cookie_value(req, &provider.cookie_name()).ok_or_else(bad_state)?;
        let mut parts = cookie.splitn(3, '.');
        let (Some(saved_state), Some(verifier)) = (parts.next(), parts.next()) else {
            return Err(bad_state());
        };
        if !super::constant_time_eq(saved_state, &state) {
            return Err(bad_state());
        }
        let next = parts
            .next()
            .map(|n| {
                percent_encoding::percent_decode_str(n)
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .filter(|n| is_local_path(n));

        let access_token = exchange(&provider, req, &code, verifier).await?;
        let user = profile(&provider, &access_token).await?;
        Ok(OAuthLogin {
            user,
            access_token,
            next,
        })
    }
}

async fn exchange(provider: &OAuth, req: &Request, code: &str, verifier: &str) -> Result<String> {
    let form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}&code_verifier={}",
        enc(code),
        enc(&provider.callback_url(req)),
        enc(&provider.client_id),
        enc(&provider.client_secret),
        enc(verifier),
    );
    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(&provider.token_url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, "zapline")
        .body(Full::new(Bytes::from(form)))
        .map_err(|e| Error::internal("invalid OAuth token URL").with_source(e))?;
    let res = fetch(request, "the OAuth provider").await?;
    let body: Value = serde_json::from_slice(&res.body).unwrap_or(Value::Null);
    match body.get("access_token").and_then(Value::as_str) {
        Some(token) if res.status.is_success() => Ok(token.to_owned()),
        _ => {
            let reason = body
                .get("error_description")
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("no access token");
            Err(Error::new(
                http::StatusCode::UNAUTHORIZED,
                format!("login with {} failed: {reason}", provider.name),
            )
            .with_code("oauth_failed"))
        }
    }
}

async fn get_json(url: &str, token: &str) -> Result<Value> {
    let request = HttpRequest::builder()
        .uri(url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, "zapline")
        .body(Full::new(Bytes::new()))
        .map_err(|e| Error::internal("invalid OAuth profile URL").with_source(e))?;
    let res = fetch(request, "the OAuth provider").await?;
    if !res.status.is_success() {
        return Err(Error::service_unavailable(format!(
            "the OAuth provider answered {} for the profile",
            res.status
        )));
    }
    serde_json::from_slice(&res.body)
        .map_err(|e| Error::service_unavailable("the OAuth profile is not JSON").with_source(e))
}

async fn profile(provider: &OAuth, token: &str) -> Result<OAuthUser> {
    let raw = get_json(&provider.userinfo_url, token).await?;
    let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_owned);
    let id = match raw.get("sub").or_else(|| raw.get("id")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => {
            return Err(Error::service_unavailable(
                "the OAuth profile has no id (`sub` or `id`)",
            ))
        }
    };
    let mut email = text("email");
    let mut email_verified = matches!(raw.get("email_verified"), Some(Value::Bool(true)))
        || raw.get("email_verified").and_then(Value::as_str) == Some("true");
    if provider.flavor == Flavor::GitHub {
        // GitHub's profile email is whatever the user made public; the
        // emails endpoint says which one is primary and verified.
        let emails = get_json(&format!("{}/emails", provider.userinfo_url), token).await?;
        let primary = emails.as_array().and_then(|list| {
            list.iter().find(|e| {
                e.get("primary") == Some(&Value::Bool(true))
                    && e.get("verified") == Some(&Value::Bool(true))
            })
        });
        match primary.and_then(|e| e.get("email")).and_then(Value::as_str) {
            Some(address) => {
                email = Some(address.to_owned());
                email_verified = true;
            }
            None => email_verified = false,
        }
    }
    let name = text("name").or_else(|| text("login"));
    let avatar_url = text("picture").or_else(|| text("avatar_url"));
    Ok(OAuthUser {
        provider: provider.name.clone(),
        id,
        email,
        email_verified,
        name,
        avatar_url,
        raw,
    })
}

fn query_param(req: &Request, name: &str) -> Option<String> {
    req.query_string()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| {
            percent_encoding::percent_decode_str(&v.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        })
    })
}

fn cookie_value(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            let (k, v) = c.trim().split_once('=')?;
            (k == name).then(|| v.to_owned())
        })
}

/// `/path` but not `//host` or `/\host`: safe to redirect to.
fn is_local_path(p: &str) -> bool {
    p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\")
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&fastrand::u64(..).to_le_bytes());
        }
    }
    b64url(&bytes)
}

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_7636() {
        // RFC 7636, appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            b64url(&Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn b64url_has_no_padding() {
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn only_local_paths_are_followed() {
        assert!(is_local_path("/dashboard?tab=1"));
        for bad in ["//evil.com", "/\\evil.com", "https://evil.com", "evil"] {
            assert!(!is_local_path(bad), "{bad}");
        }
    }
}
