//! CSRF protection with the (signed) double-submit cookie pattern (feature
//! `cookies`).

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use http::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use sha2::Sha256;

use super::constant_time_eq;
use crate::body::ReqBody;
use crate::cookies::{random_token, Cookie, Key, SameSite};
use crate::extract::FromRequestParts;
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::session::Session;
use crate::{Error, IntoResponse, Request, Response, Result};

/// Default token cookie name. The `__Host-` prefix makes browsers refuse the
/// cookie unless it is `Secure`, has `Path=/` and no `Domain`, so a sibling
/// subdomain cannot plant one.
const HOST_COOKIE_NAME: &str = "__Host-zapline.csrf";
/// Default token cookie name without `Secure` (plain-HTTP development),
/// where the `__Host-` prefix would make browsers drop the cookie.
const COOKIE_NAME: &str = "zapline.csrf";

/// Most cookie pairs inspected per request (as for `CookieJar`).
const MAX_COOKIES: usize = 256;

/// Random bytes in a token. Signed tokens append a 16-byte binding tag and a
/// 32-byte HMAC-SHA256 over both.
const RANDOM_LEN: usize = 32;
const TAG_LEN: usize = 16;
const SIGNED_LEN: usize = RANDOM_LEN + TAG_LEN + 32;
/// Characters in a signed token (base64url without padding).
const SIGNED_CHARS: usize = (SIGNED_LEN * 4).div_ceil(3);

/// Prefixed to the random bytes and tag before computing a token's MAC. It
/// is not UTF-8 (`0xff`), while signed cookies MAC their UTF-8 value with
/// the same key, so no signed cookie can ever double as a CSRF token.
const MAC_CONTEXT: &[u8] = b"\xffzapline.csrf.v2\x00";
/// Prefixed to the session id when deriving a binding tag (domain-separated
/// from [`MAC_CONTEXT`] and from signed cookies the same way).
const BIND_CONTEXT: &[u8] = b"\xffzapline.csrf.session.v1\x00";
/// The tag of a token that is not bound to a session.
const UNBOUND: [u8; TAG_LEN] = [0; TAG_LEN];

#[derive(Clone)]
struct Config {
    /// `None`: [`HOST_COOKIE_NAME`] when `secure`, else [`COOKIE_NAME`].
    cookie_name: Option<String>,
    header: HeaderName,
    field: Arc<str>,
    exempt: Vec<Exempt>,
    secure: bool,
    same_site: SameSite,
    /// Signs tokens instead of the app's key.
    key: Option<Key>,
}

#[derive(Clone)]
enum Exempt {
    Exact(String),
    Prefix(String),
}

/// Signed double-submit-cookie CSRF protection.
///
/// Every response to a client without a valid token gets a token cookie
/// (default `__Host-zapline.csrf`, readable by JavaScript). Requests with
/// unsafe methods (anything but `GET`, `HEAD`, `OPTIONS`, `TRACE`) must echo
/// that token in the `x-csrf-token` header or, for
/// `application/x-www-form-urlencoded` bodies, the `csrf_token` field;
/// otherwise they get `403` with code `csrf_failed`. Comparison is
/// constant-time. Handlers render the token with [`CsrfToken`].
///
/// **Register a cookie key** ([`App::cookie_key`](crate::App::cookie_key),
/// or [`key`](Self::key)). Tokens are then `random || tag ||
/// HMAC-SHA256(random || tag)` and only tokens this server minted are
/// accepted, so a cookie planted by a
/// sibling subdomain (same site, so `SameSite` does not stop it) with a
/// value of the attacker's choosing is useless. Without a key, tokens are
/// unsigned random values and the `__Host-` cookie prefix is the only
/// defense against planted cookies. When the request carries several token
/// cookies (a planted one plus the real one), unsafe requests are rejected
/// unless they all hold the same token.
///
/// **With [`SessionLayer`](crate::session::SessionLayer)** (in either wrap
/// order), tokens are also bound to the session: each carries an HMAC tag
/// of the session id the client presents, so a token minted for one session
/// (or for a visitor without one) is rejected for any other, even when it
/// was planted next to the victim's session cookie. The token rotates
/// whenever the session id changes (a new session, `regenerate()` on login,
/// `destroy()` on logout): the response carries the new token cookie. When
/// neither `Csrf` nor the app has a key, the session's key signs the tokens.
///
/// Use it as global middleware; combine with `SameSite` cookies.
///
/// ```
/// use zapline::auth::{Csrf, CsrfToken};
/// use zapline::cookies::Key;
/// use zapline::prelude::*;
///
/// async fn form(token: CsrfToken) -> Html<String> {
///     Html(format!(r#"<form method="post">{}<button>Send</button></form>"#, token.hidden_input()))
/// }
///
/// let app = App::new()
///     .cookie_key(Key::generate()) // a stable secret key in production
///     .wrap(Csrf::new().exempt("/webhooks/*"))
///     .get("/form", form)
///     .post("/form", || async { "saved" });
/// ```
#[derive(Clone)]
pub struct Csrf {
    config: Arc<Config>,
}

impl Default for Csrf {
    fn default() -> Self {
        Self::new()
    }
}

impl Csrf {
    /// Protection with the defaults described above.
    pub fn new() -> Self {
        Csrf {
            config: Arc::new(Config {
                cookie_name: None,
                header: HeaderName::from_static("x-csrf-token"),
                field: "csrf_token".into(),
                exempt: Vec::new(),
                secure: true,
                same_site: SameSite::Lax,
                key: None,
            }),
        }
    }

    fn config(&mut self) -> &mut Config {
        Arc::make_mut(&mut self.config)
    }

    /// Token cookie name (default `__Host-zapline.csrf`, or `zapline.csrf`
    /// with [`secure(false)`](Self::secure)). A `__Host-` name needs
    /// `Secure`.
    pub fn cookie_name(mut self, name: &str) -> Self {
        assert!(!name.is_empty(), "CSRF cookie name must not be empty");
        self.config().cookie_name = Some(name.to_owned());
        self
    }

    /// Request header carrying the token (default `x-csrf-token`).
    ///
    /// # Panics
    /// If `name` is not a valid header name.
    pub fn header_name(mut self, name: &str) -> Self {
        self.config().header = HeaderName::try_from(name)
            .unwrap_or_else(|_| panic!("invalid CSRF header name: {name:?}"));
        self
    }

    /// Form field carrying the token (default `csrf_token`).
    pub fn form_field(mut self, name: &str) -> Self {
        self.config().field = name.into();
        self
    }

    /// Skip the check for `path`, or for every path under a prefix when it
    /// ends in `*` (e.g. `"/webhooks/*"`).
    pub fn exempt(mut self, path: &str) -> Self {
        let rule = match path.strip_suffix('*') {
            Some(prefix) => Exempt::Prefix(prefix.to_owned()),
            None => Exempt::Exact(path.to_owned()),
        };
        self.config().exempt.push(rule);
        self
    }

    /// The token cookie's `Secure` attribute (default `true`). Turn it off
    /// only for development over plain HTTP; the default cookie name then
    /// drops its `__Host-` prefix (browsers require `Secure` for it).
    pub fn secure(mut self, secure: bool) -> Self {
        self.config().secure = secure;
        self
    }

    /// The token cookie's `SameSite` attribute (default `Lax`).
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        self.config().same_site = same_site;
        self
    }

    /// Sign tokens with this key instead of the app's
    /// ([`App::cookie_key`](crate::App::cookie_key)).
    pub fn key(mut self, key: Key) -> Self {
        self.config().key = Some(key);
        self
    }
}

impl Config {
    fn is_exempt(&self, path: &str) -> bool {
        self.exempt.iter().any(|e| match e {
            Exempt::Exact(p) => p == path,
            Exempt::Prefix(p) => path.starts_with(p.as_str()),
        })
    }

    fn cookie_name(&self) -> &str {
        match &self.cookie_name {
            Some(name) => name,
            None if self.secure => HOST_COOKIE_NAME,
            None => COOKIE_NAME,
        }
    }

    fn cookie(&self, token: &str) -> Option<HeaderValue> {
        let mut c = Cookie::new(self.cookie_name().to_owned(), token.to_owned());
        c.set_path("/");
        c.set_secure(self.secure);
        c.set_same_site(self.same_site);
        HeaderValue::try_from(c.encoded().to_string()).ok()
    }
}

fn is_safe(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

/// Whether `s` has the shape of a token: base64url of 32 random bytes
/// (unsigned) or of a signed token. Used when no key is known yet; a session
/// layer later checks signed tokens with its own key.
fn is_well_formed(s: &str) -> bool {
    (s.len() == 43 || s.len() == SIGNED_CHARS)
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Mints and verifies signed tokens: base64url of `random || tag || mac`,
/// where `tag` binds the token to a session ([`UNBOUND`] when there is
/// none) and `mac` is HMAC-SHA256 of `random || tag` under the key's
/// signing half.
#[derive(Clone)]
struct Signer {
    /// Keyed and fed [`MAC_CONTEXT`]; cloned per token.
    mac: Hmac<Sha256>,
    /// Keyed and fed [`BIND_CONTEXT`]; cloned per binding tag.
    bind: Hmac<Sha256>,
}

impl Signer {
    fn new(key: &Key) -> Result<Self> {
        let keyed = || {
            Hmac::<Sha256>::new_from_slice(key.signing())
                .map_err(|_| Error::internal("invalid CSRF signing key"))
        };
        let mut mac = keyed()?;
        mac.update(MAC_CONTEXT);
        let mut bind = keyed()?;
        bind.update(BIND_CONTEXT);
        Ok(Signer { mac, bind })
    }

    fn mac(&self, random: &[u8], tag: &[u8]) -> Hmac<Sha256> {
        let mut mac = self.mac.clone();
        mac.update(random);
        mac.update(tag);
        mac
    }

    /// The binding tag for the session id the client presents (`None`: no
    /// session cookie). A truncated HMAC, so the token reveals nothing about
    /// the id.
    fn session_tag(&self, session_id: Option<&str>) -> [u8; TAG_LEN] {
        let mut mac = self.bind.clone();
        match session_id {
            Some(id) => {
                mac.update(&[1]);
                mac.update(id.as_bytes());
            }
            None => mac.update(&[0]),
        }
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&mac.finalize().into_bytes()[..TAG_LEN]);
        tag
    }

    fn mint(&self, tag: &[u8; TAG_LEN]) -> Result<String> {
        let mut buf = [0u8; SIGNED_LEN];
        let (random, rest) = buf.split_at_mut(RANDOM_LEN);
        let (tag_out, mac) = rest.split_at_mut(TAG_LEN);
        fill_random(random)?;
        tag_out.copy_from_slice(tag);
        mac.copy_from_slice(&self.mac(random, tag).finalize().into_bytes());
        Ok(BASE64_URL_SAFE_NO_PAD.encode(buf))
    }

    /// The binding tag of `token` if this server minted it (constant-time
    /// MAC check), else `None`.
    fn verify(&self, token: &str) -> Option<[u8; TAG_LEN]> {
        if token.len() != SIGNED_CHARS {
            return None;
        }
        let bytes = BASE64_URL_SAFE_NO_PAD.decode(token).ok()?;
        if bytes.len() != SIGNED_LEN {
            return None;
        }
        let (random, rest) = bytes.split_at(RANDOM_LEN);
        let (tag, mac) = rest.split_at(TAG_LEN);
        self.mac(random, tag).verify_slice(mac).ok()?;
        let mut out = [0u8; TAG_LEN];
        out.copy_from_slice(tag);
        Some(out)
    }
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|e| {
        Error::internal("secure random numbers are unavailable")
            .with_source(std::io::Error::other(e.to_string()))
    })
}

/// The values of every cookie named `name`, in request order. Browsers send
/// all cookies whose domain and path match (most specific path first), so a
/// cookie planted by a sibling subdomain can come ahead of ours.
fn cookie_values<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = String> + 'a {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(Cookie::split_parse_encoded)
        .take(MAX_COOKIES)
        .flatten()
        .filter(move |c| c.name() == name)
        .map(|c| c.value().to_owned())
}

fn rejected() -> Error {
    Error::new(StatusCode::FORBIDDEN, "CSRF token missing or invalid").with_code("csrf_failed")
}

impl Middleware for Csrf {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let cfg = self.config.clone();
        Box::pin(async move {
            let (req, binding) = match check(cfg, req).await {
                Ok(checked) => checked,
                Err(e) => return e.into_response(),
            };
            let mut res = next.run(req).await;
            // A session layer that bound the token writes the cookie when it
            // commits, since the session id may change during the request.
            if let Some(v) = binding.unbound_cookie() {
                res.headers_mut().append(SET_COOKIE, v);
            }
            res
        })
    }
}

/// Verify `req`, attach its [`CsrfToken`] and, when a session layer already
/// ran, bind the token to the session.
async fn check(cfg: Arc<Config>, mut req: Request) -> Result<(Request, CsrfBinding)> {
    let key = cfg.key.clone().or_else(|| req.state::<Key>().cloned());
    let signer = key.as_ref().map(Signer::new).transpose()?;
    // Only well-formed tokens count; with a key, only ones this server
    // minted, so planted values of an attacker's choosing are ignored.
    // Several different tokens mean one was planted: trust none of them.
    let (cookie, ambiguous) = {
        let mut tokens =
            cookie_values(req.headers(), cfg.cookie_name()).filter(|t| match &signer {
                Some(signer) => signer.verify(t).is_some(),
                None => is_well_formed(t),
            });
        let first = tokens.next();
        let ambiguous = first.as_ref().is_some_and(|c| tokens.any(|t| t != *c));
        (first, ambiguous)
    };
    let enforce = !is_safe(req.method()) && !cfg.is_exempt(req.path());
    if enforce {
        let expected = cookie
            .as_deref()
            .filter(|_| !ambiguous)
            .ok_or_else(rejected)?;
        let provided = submitted(&cfg, &mut req).await?;
        if !provided.is_some_and(|p| constant_time_eq(p, expected)) {
            return Err(rejected());
        }
    }
    let (token, fresh) = match (cookie, &signer) {
        (Some(t), _) => (t, false),
        (None, Some(signer)) => (signer.mint(&UNBOUND)?, true),
        (None, None) => (random_token()?, true),
    };
    let binding = CsrfBinding(Arc::new(Binding {
        cfg,
        key,
        enforce,
        state: Mutex::new(BindState {
            token,
            fresh,
            bound: None,
        }),
    }));
    binding.attach(&mut req);
    match Session::unloaded(&req) {
        // The session layer ran first: bind now.
        Some(session) => binding.bind(&mut req, &session)?,
        // A session layer installed inside `Csrf` binds when it runs.
        None => {
            req.extensions_mut().insert(binding.clone());
        }
    }
    Ok((req, binding))
}

/// The per-request CSRF state, shared with
/// [`SessionLayer`](crate::session::SessionLayer) so tokens are bound to the
/// session whichever of the two middleware runs first.
#[derive(Clone)]
pub(crate) struct CsrfBinding(Arc<Binding>);

struct Binding {
    cfg: Arc<Config>,
    /// From [`Csrf::key`] or the app; without either, the session's key.
    key: Option<Key>,
    /// The request was checked (unsafe method, path not exempt), so its
    /// token must also be bound to the session.
    enforce: bool,
    state: Mutex<BindState>,
}

struct BindState {
    /// The token in effect: the client's, or a newly minted one.
    token: String,
    /// The token needs a `Set-Cookie`.
    fresh: bool,
    /// Set once a session bound the token: the signer and the tag the token
    /// carries. The session layer then writes the cookie.
    bound: Option<(Signer, [u8; TAG_LEN])>,
}

impl CsrfBinding {
    fn state(&self) -> MutexGuard<'_, BindState> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hand the current token to handlers ([`CsrfToken`]).
    fn attach(&self, req: &mut Request) {
        let token = self.state().token.clone();
        req.extensions_mut().insert(CsrfToken {
            token,
            field: self.0.cfg.field.clone(),
        });
    }

    /// Bind the token to the session id the client presents. A checked
    /// request whose token was minted for another session (or for none)
    /// fails with `403 csrf_failed`; otherwise a token that is not bound to
    /// this session is replaced by one that is.
    pub(crate) fn bind(&self, req: &mut Request, session: &Session) -> Result<()> {
        let key = self.0.key.as_ref().unwrap_or_else(|| session.cookie_key());
        let signer = Signer::new(key)?;
        let tag = signer.session_tag(session.presented_id().as_deref());
        {
            let mut st = self.state();
            if st.bound.is_some() {
                return Ok(());
            }
            let current = signer.verify(&st.token);
            if !current.is_some_and(|t| constant_time_eq(t, tag)) {
                if self.0.enforce {
                    return Err(rejected());
                }
                st.token = signer.mint(&tag)?;
                st.fresh = true;
            }
            st.bound = Some((signer, tag));
        }
        self.attach(req);
        session.attach_csrf(self.clone());
        Ok(())
    }

    /// Called by the session layer once the session id the client will
    /// present next is known (`None`: no session cookie). Rotates the token
    /// when the id changed (login, logout, a new session) and writes the
    /// cookie if needed.
    pub(crate) fn commit(&self, session_id: Option<&str>, res: &mut Response) -> Result<()> {
        let mut st = self.state();
        let Some((signer, tag)) = st.bound.clone() else {
            return Ok(());
        };
        let next = signer.session_tag(session_id);
        if next != tag {
            st.token = signer.mint(&next)?;
            st.fresh = true;
            st.bound = Some((signer, next));
        }
        if std::mem::take(&mut st.fresh) {
            if let Some(v) = self.0.cfg.cookie(&st.token) {
                res.headers_mut().append(SET_COOKIE, v);
            }
        }
        Ok(())
    }

    /// The cookie to write when no session bound the token.
    fn unbound_cookie(&self) -> Option<HeaderValue> {
        let st = self.state();
        if st.fresh && st.bound.is_none() {
            self.0.cfg.cookie(&st.token)
        } else {
            None
        }
    }
}

/// The token sent with the request: the header, else the form field
/// (the buffered body is put back for the handler).
async fn submitted(cfg: &Config, req: &mut Request) -> Result<Option<String>> {
    let mut values = req.headers().get_all(&cfg.header).iter();
    if let Some(v) = values.next() {
        if values.next().is_some() {
            return Ok(None);
        }
        return Ok(v.to_str().ok().map(str::to_owned));
    }
    let is_form = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    if !is_form {
        return Ok(None);
    }
    let body = req.body_bytes().await?;
    let token = form_field(&body, &cfg.field);
    req.body = Some(ReqBody::Full(body));
    Ok(token)
}

/// The first value of `name` in a urlencoded body.
fn form_field(body: &[u8], name: &str) -> Option<String> {
    body.split(|&b| b == b'&').find_map(|pair| {
        let mut kv = pair.splitn(2, |&b| b == b'=');
        let k = decode(kv.next()?)?;
        (k == name)
            .then(|| decode(kv.next().unwrap_or_default()))
            .flatten()
    })
}

fn decode(raw: &[u8]) -> Option<String> {
    let plus: Vec<u8> = raw
        .iter()
        .map(|&b| if b == b'+' { b' ' } else { b })
        .collect();
    percent_encoding::percent_decode(&plus)
        .decode_utf8()
        .ok()
        .map(|s| s.into_owned())
}

/// The CSRF token for this request, set by the [`Csrf`] middleware (`500`
/// without it). Embed it in forms with [`hidden_input`](Self::hidden_input)
/// or hand it to JavaScript, which sends it back in the `x-csrf-token`
/// header.
#[derive(Clone)]
pub struct CsrfToken {
    token: String,
    field: Arc<str>,
}

impl CsrfToken {
    /// The token value.
    pub fn as_str(&self) -> &str {
        &self.token
    }

    /// The form field name the middleware reads.
    pub fn field_name(&self) -> &str {
        &self.field
    }

    /// `<input type="hidden" name="csrf_token" value="...">`.
    pub fn hidden_input(&self) -> String {
        format!(
            r#"<input type="hidden" name="{}" value="{}">"#,
            html_escape(&self.field),
            html_escape(&self.token)
        )
    }
}

impl fmt::Display for CsrfToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.token)
    }
}

impl fmt::Debug for CsrfToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CsrfToken(<redacted>)")
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

impl FromRequestParts for CsrfToken {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.extensions().get::<CsrfToken>().cloned().ok_or_else(|| {
            Error::internal("no CSRF token: install the `Csrf` middleware")
                .with_code("csrf_layer_missing")
                .diagnosed("ZL0074", None)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::form_field;

    #[test]
    fn parses_form_fields() {
        assert_eq!(
            form_field(b"a=1&csrf_token=x%2By+z&b", "csrf_token").as_deref(),
            Some("x+y z")
        );
        assert_eq!(form_field(b"csrf_token", "csrf_token").as_deref(), Some(""));
        assert_eq!(form_field(b"a=1", "csrf_token"), None);
    }
}
