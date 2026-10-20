//! CSRF protection with the double-submit cookie pattern (feature `cookies`).

use std::fmt;
use std::sync::Arc;

use http::header::{CONTENT_TYPE, SET_COOKIE};
use http::{HeaderName, HeaderValue, Method, StatusCode};

use super::constant_time_eq;
use crate::body::ReqBody;
use crate::cookies::{random_token, Cookie, CookieJar, SameSite};
use crate::extract::FromRequestParts;
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::{Error, IntoResponse, Request, Response, Result};

#[derive(Clone)]
struct Config {
    cookie_name: String,
    header: HeaderName,
    field: Arc<str>,
    exempt: Vec<Exempt>,
    secure: bool,
    same_site: SameSite,
}

#[derive(Clone)]
enum Exempt {
    Exact(String),
    Prefix(String),
}

/// Double-submit-cookie CSRF protection.
///
/// Every response to a client without a token gets a random token cookie
/// (default `zapline.csrf`, readable by JavaScript). Requests with unsafe
/// methods (anything but `GET`, `HEAD`, `OPTIONS`, `TRACE`) must echo that
/// token in the `x-csrf-token` header or, for
/// `application/x-www-form-urlencoded` bodies, the `csrf_token` field;
/// otherwise they get `403` with code `csrf_failed`. Comparison is
/// constant-time. Handlers render the token with [`CsrfToken`].
///
/// Use it as global middleware; combine with `SameSite` cookies. In
/// production consider `.cookie_name("__Host-csrf")`.
///
/// ```
/// use zapline::auth::{Csrf, CsrfToken};
/// use zapline::prelude::*;
///
/// async fn form(token: CsrfToken) -> Html<String> {
///     Html(format!(r#"<form method="post">{}<button>Send</button></form>"#, token.hidden_input()))
/// }
///
/// let app = App::new()
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
                cookie_name: "zapline.csrf".into(),
                header: HeaderName::from_static("x-csrf-token"),
                field: "csrf_token".into(),
                exempt: Vec::new(),
                secure: true,
                same_site: SameSite::Lax,
            }),
        }
    }

    fn config(&mut self) -> &mut Config {
        Arc::make_mut(&mut self.config)
    }

    /// Token cookie name (default `zapline.csrf`).
    pub fn cookie_name(mut self, name: &str) -> Self {
        assert!(!name.is_empty(), "CSRF cookie name must not be empty");
        self.config().cookie_name = name.to_owned();
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

    /// The token cookie's `Secure` attribute (default `true`).
    pub fn secure(mut self, secure: bool) -> Self {
        self.config().secure = secure;
        self
    }

    /// The token cookie's `SameSite` attribute (default `Lax`).
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        self.config().same_site = same_site;
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

    fn cookie(&self, token: &str) -> Option<HeaderValue> {
        let mut c = Cookie::new(self.cookie_name.clone(), token.to_owned());
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

/// Tokens are base64url of 32 random bytes.
fn is_token(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn rejected() -> Error {
    Error::new(StatusCode::FORBIDDEN, "CSRF token missing or invalid").with_code("csrf_failed")
}

impl Middleware for Csrf {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let cfg = self.config.clone();
        Box::pin(async move {
            match check(&cfg, req).await {
                Ok((req, token, fresh)) => {
                    let mut res = next.run(req).await;
                    if fresh {
                        if let Some(v) = cfg.cookie(&token) {
                            res.headers_mut().append(SET_COOKIE, v);
                        }
                    }
                    res
                }
                Err(e) => e.into_response(),
            }
        })
    }
}

/// Verify `req` and attach its [`CsrfToken`]. Returns the request, the token
/// and whether the token is new (needs a cookie).
async fn check(cfg: &Config, mut req: Request) -> Result<(Request, String, bool)> {
    let cookie = CookieJar::from_headers(req.headers())
        .get(&cfg.cookie_name)
        .map(|c| c.value().to_owned())
        .filter(|t| is_token(t));
    if !is_safe(req.method()) && !cfg.is_exempt(req.path()) {
        let expected = cookie.as_deref().ok_or_else(rejected)?;
        let provided = submitted(cfg, &mut req).await?;
        if !provided.is_some_and(|p| constant_time_eq(p, expected)) {
            return Err(rejected());
        }
    }
    let (token, fresh) = match cookie {
        Some(t) => (t, false),
        None => (random_token()?, true),
    };
    req.extensions_mut().insert(CsrfToken {
        token: token.clone(),
        field: cfg.field.clone(),
    });
    Ok((req, token, fresh))
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
