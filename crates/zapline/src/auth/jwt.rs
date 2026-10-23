//! JSON Web Tokens (feature `jwt`), signed and verified with `jsonwebtoken`
//! (RustCrypto backend).

use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::HeaderValue;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::Header;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Validation};

use super::authenticate::{Authenticate, Authenticator};
use super::bearer::bearer;
use super::guard::HasRoles;
use super::{missing_credentials, unauthorized};
use crate::extract::FromRequestParts;
use crate::{App, Error, Request, Result};

/// Longest token accepted, in bytes.
const MAX_TOKEN_LEN: usize = 16 * 1024;
/// Minimum HMAC secret length (RFC 7518 §3.2: at least the hash size).
const MIN_HMAC_SECRET: usize = 32;

const CHALLENGE: HeaderValue = HeaderValue::from_static("Bearer");
const CHALLENGE_INVALID: HeaderValue = HeaderValue::from_static("Bearer error=\"invalid_token\"");
const CHALLENGE_EXPIRED: HeaderValue =
    HeaderValue::from_static("Bearer error=\"invalid_token\", error_description=\"token expired\"");

/// How tokens are signed and validated. Cheap to clone; register it with
/// [`App::jwt`] so the [`Jwt`] extractor and [`Authenticate::jwt`] find it,
/// and extract it with `State<JwtConfig>` to issue tokens.
///
/// Defaults: `exp` is required and validated (60 s leeway), `nbf` is
/// validated when present, issued tokens live for one hour.
///
/// ```
/// use std::time::Duration;
/// use zapline::auth::{Claims, JwtConfig};
///
/// let jwt = JwtConfig::hs256("an-example-secret-of-at-least-32-bytes")
///     .issuer("https://auth.example.com")
///     .audience("api")
///     .ttl(Duration::from_secs(15 * 60));
///
/// let token = jwt.encode(&Claims::new("user-1").roles(["admin"])).unwrap();
/// let claims: Claims = jwt.decode(&token).unwrap();
/// assert_eq!(claims.sub, "user-1");
/// ```
#[derive(Clone)]
pub struct JwtConfig {
    inner: Arc<Inner>,
}

#[derive(Clone)]
struct Inner {
    algorithm: Algorithm,
    encoding: Option<EncodingKey>,
    decoding: DecodingKey,
    validation: Validation,
    required: HashSet<String>,
    ttl: Duration,
    issuer: Option<String>,
    audience: Vec<String>,
    key_id: Option<String>,
}

impl fmt::Debug for JwtConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let i = &self.inner;
        f.debug_struct("JwtConfig")
            .field("algorithm", &i.algorithm)
            .field("can_sign", &i.encoding.is_some())
            .field("issuer", &i.issuer)
            .field("audience", &i.audience)
            .field("ttl", &i.ttl)
            .finish_non_exhaustive()
    }
}

impl JwtConfig {
    /// Full control: any algorithm and keys. `encoding` is needed only to
    /// issue tokens.
    pub fn new(algorithm: Algorithm, decoding: DecodingKey, encoding: Option<EncodingKey>) -> Self {
        // Pin the aws-lc backend unless the app installed one: if another
        // crate also enabled `rust_crypto`, jsonwebtoken would otherwise
        // panic on first use. (aws-lc, not RustCrypto: the `rsa` crate's
        // signing is open to the Marvin timing attack, RUSTSEC-2023-0071.)
        let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
        let mut validation = Validation::new(algorithm);
        validation.validate_nbf = true;
        let required = validation.required_spec_claims.clone();
        JwtConfig {
            inner: Arc::new(Inner {
                algorithm,
                encoding,
                decoding,
                validation,
                required,
                ttl: Duration::from_secs(3600),
                issuer: None,
                audience: Vec::new(),
                key_id: None,
            }),
        }
    }

    /// HMAC-SHA256 with a shared secret.
    ///
    /// # Panics
    /// If the secret is shorter than 32 bytes (RFC 7518 §3.2).
    pub fn hs256(secret: impl AsRef<[u8]>) -> Self {
        Self::hmac(Algorithm::HS256, secret.as_ref())
    }

    /// HMAC-SHA384 with a shared secret (at least 32 bytes).
    pub fn hs384(secret: impl AsRef<[u8]>) -> Self {
        Self::hmac(Algorithm::HS384, secret.as_ref())
    }

    /// HMAC-SHA512 with a shared secret (at least 32 bytes).
    pub fn hs512(secret: impl AsRef<[u8]>) -> Self {
        Self::hmac(Algorithm::HS512, secret.as_ref())
    }

    fn hmac(alg: Algorithm, secret: &[u8]) -> Self {
        assert!(
            secret.len() >= MIN_HMAC_SECRET,
            "JWT HMAC secret must be at least {MIN_HMAC_SECRET} bytes, got {}",
            secret.len()
        );
        Self::new(
            alg,
            DecodingKey::from_secret(secret),
            Some(EncodingKey::from_secret(secret)),
        )
    }

    /// RS256 from a PEM public key (`PUBLIC KEY` or `RSA PUBLIC KEY`) and,
    /// to issue tokens, a PEM private key.
    ///
    /// # Panics
    /// If a key cannot be parsed; see [`try_rs256_pem`](Self::try_rs256_pem).
    pub fn rs256_pem<P: AsRef<[u8]>>(public_pem: P, private_pem: Option<P>) -> Self {
        Self::try_rs256_pem(public_pem, private_pem)
            .unwrap_or_else(|e| panic!("invalid RS256 key: {}", e.message()))
    }

    /// Fallible [`rs256_pem`](Self::rs256_pem).
    pub fn try_rs256_pem<P: AsRef<[u8]>>(public_pem: P, private_pem: Option<P>) -> Result<Self> {
        let decoding = DecodingKey::from_rsa_pem(public_pem.as_ref()).map_err(key_error)?;
        let encoding = private_pem
            .map(|p| EncodingKey::from_rsa_pem(p.as_ref()).map_err(key_error))
            .transpose()?;
        Ok(Self::new(Algorithm::RS256, decoding, encoding))
    }

    /// ES256 (P-256) from a PEM public key and, to issue tokens, a PKCS#8 PEM
    /// private key.
    ///
    /// # Panics
    /// If a key cannot be parsed; see [`try_es256_pem`](Self::try_es256_pem).
    pub fn es256_pem<P: AsRef<[u8]>>(public_pem: P, private_pem: Option<P>) -> Self {
        Self::try_es256_pem(public_pem, private_pem)
            .unwrap_or_else(|e| panic!("invalid ES256 key: {}", e.message()))
    }

    /// Fallible [`es256_pem`](Self::es256_pem).
    pub fn try_es256_pem<P: AsRef<[u8]>>(public_pem: P, private_pem: Option<P>) -> Result<Self> {
        let decoding = DecodingKey::from_ec_pem(public_pem.as_ref()).map_err(key_error)?;
        let encoding = private_pem
            .map(|p| EncodingKey::from_ec_pem(p.as_ref()).map_err(key_error))
            .transpose()?;
        Ok(Self::new(Algorithm::ES256, decoding, encoding))
    }

    fn inner_mut(&mut self) -> &mut Inner {
        Arc::make_mut(&mut self.inner)
    }

    /// Recompute the required claims: the configured set plus `iss`/`aud`
    /// when an issuer/audience is expected (otherwise tokens could omit them).
    fn sync_required(&mut self) {
        let i = self.inner_mut();
        let mut req = i.required.clone();
        if i.issuer.is_some() {
            req.insert("iss".into());
        }
        if !i.audience.is_empty() {
            req.insert("aud".into());
        }
        i.validation.required_spec_claims = req;
    }

    /// Expected issuer: tokens must carry this `iss`; issued tokens get it.
    pub fn issuer(mut self, issuer: impl Into<String>) -> Self {
        let issuer = issuer.into();
        let i = self.inner_mut();
        i.validation.set_issuer(&[&issuer]);
        i.issuer = Some(issuer);
        self.sync_required();
        self
    }

    /// An accepted audience (call again to accept several). Tokens must carry
    /// a matching `aud`; issued tokens get the first one.
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        let i = self.inner_mut();
        i.audience.push(audience.into());
        let all = i.audience.clone();
        i.validation.set_audience(&all);
        self.sync_required();
        self
    }

    /// Clock-skew tolerance for `exp` and `nbf` (default 60 s).
    pub fn leeway(mut self, leeway: Duration) -> Self {
        self.inner_mut().validation.leeway = leeway.as_secs();
        self
    }

    /// Registered claims that must be present: any of `exp`, `nbf`, `iss`,
    /// `aud`, `sub` (default: `exp`). `iss`/`aud` are always required when an
    /// issuer/audience is configured; `exp` and `nbf` are validated whenever
    /// present.
    pub fn required_claims<I, S>(mut self, claims: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.inner_mut().required = claims.into_iter().map(Into::into).collect();
        self.sync_required();
        self
    }

    /// Lifetime of issued tokens when the claims carry no `exp` (default 1 h).
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.inner_mut().ttl = ttl;
        self
    }

    /// `kid` header for issued tokens (key rotation).
    pub fn key_id(mut self, kid: impl Into<String>) -> Self {
        self.inner_mut().key_id = Some(kid.into());
        self
    }

    /// Adjust the underlying [`Validation`] directly.
    pub fn validation(mut self, f: impl FnOnce(&mut Validation)) -> Self {
        f(&mut self.inner_mut().validation);
        self
    }

    /// The signing algorithm.
    pub fn algorithm(&self) -> Algorithm {
        self.inner.algorithm
    }

    /// Sign `claims`. Missing `iat`, `exp` (now + TTL), `iss` and `aud` are
    /// filled in from this config. `claims` must serialize to a JSON object.
    pub fn encode<C: Serialize + ?Sized>(&self, claims: &C) -> Result<String> {
        let i = &self.inner;
        let key = i.encoding.as_ref().ok_or_else(|| {
            Error::internal("this JwtConfig has no private key and cannot issue tokens")
        })?;
        let mut value = serde_json::to_value(claims)?;
        let obj = value
            .as_object_mut()
            .ok_or_else(|| Error::internal("JWT claims must serialize to a JSON object"))?;
        let now = unix_now();
        fill(obj, "iat", || now.into());
        fill(obj, "exp", || now.saturating_add(i.ttl.as_secs()).into());
        if let Some(iss) = &i.issuer {
            fill(obj, "iss", || iss.clone().into());
        }
        if let Some(aud) = i.audience.first() {
            fill(obj, "aud", || aud.clone().into());
        }
        let mut header = Header::new(i.algorithm);
        header.kid = i.key_id.clone();
        jsonwebtoken::encode(&header, &value, key)
            .map_err(|e| Error::internal("failed to sign JWT").with_source(e))
    }

    /// Verify `token` and deserialize its claims.
    ///
    /// Errors: `401 token_expired` for expired tokens, `401 invalid_token`
    /// for anything else wrong with the token (both with a `Bearer`
    /// challenge), `500` when the configured key is unusable.
    pub fn decode<C: DeserializeOwned>(&self, token: &str) -> Result<C> {
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            return Err(invalid_token());
        }
        jsonwebtoken::decode::<C>(token, &self.inner.decoding, &self.inner.validation)
            .map(|data| data.claims)
            .map_err(decode_error)
    }
}

fn fill(obj: &mut Map<String, Value>, key: &str, value: impl FnOnce() -> Value) {
    if obj.get(key).is_none_or(Value::is_null) {
        obj.insert(key.to_owned(), value());
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn key_error(e: jsonwebtoken::errors::Error) -> Error {
    Error::internal(format!("invalid JWT key: {e}")).with_source(e)
}

fn invalid_token() -> Error {
    unauthorized("invalid_token", "invalid token", CHALLENGE_INVALID)
}

fn decode_error(e: jsonwebtoken::errors::Error) -> Error {
    match e.kind() {
        ErrorKind::ExpiredSignature => {
            unauthorized("token_expired", "token expired", CHALLENGE_EXPIRED)
        }
        ErrorKind::InvalidEcdsaKey
        | ErrorKind::InvalidEddsaKey
        | ErrorKind::InvalidRsaKey(_)
        | ErrorKind::InvalidKeyFormat
        | ErrorKind::MissingAlgorithm
        | ErrorKind::Provider(_) => {
            Error::internal("JWT verification is misconfigured").with_source(e)
        }
        _ => invalid_token(),
    }
}

/// The registered JWT claims plus `roles`, `scopes` and any other claims
/// (in [`extra`](Self::extra)).
///
/// `aud` and `roles` accept a string or an array; `scopes` also reads the
/// OAuth `scope` (space-separated) and `scp` claims.
///
/// ```
/// use zapline::auth::Claims;
///
/// let claims = Claims::new("user-1")
///     .roles(["admin"])
///     .scopes(["users:read", "users:write"])
///     .claim("tenant", "acme");
/// assert!(claims.has_role("admin"));
/// assert_eq!(claims.get::<String>("tenant").as_deref(), Some("acme"));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: who the token is about (usually a user id).
    pub sub: String,
    /// Expiry (seconds since the Unix epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,
    /// Issued at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<u64>,
    /// Not before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbf: Option<u64>,
    /// Issuer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    /// Audience(s).
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "one_or_many")]
    pub aud: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "one_or_many::deserialize"
    )]
    /// Roles, checked by `.require_roles(..)` policies.
    pub roles: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        alias = "scope",
        alias = "scp",
        deserialize_with = "space_separated::deserialize"
    )]
    /// Scopes, checked by `.require_scopes(..)` policies.
    pub scopes: Vec<String>,
    /// Every other claim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Claims {
    /// Claims about subject `sub`.
    pub fn new(sub: impl Into<String>) -> Self {
        Claims {
            sub: sub.into(),
            ..Self::default()
        }
    }

    /// Add roles. (Read them back through the `roles` field.)
    pub fn roles<I, S>(mut self, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.roles.extend(roles.into_iter().map(Into::into));
        self
    }

    /// Add scopes. (Read them back through the `scopes` field.)
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes.extend(scopes.into_iter().map(Into::into));
        self
    }

    /// Set a custom claim. Values that fail to serialize are stored as `null`.
    pub fn claim(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        let value = serde_json::to_value(value).unwrap_or(Value::Null);
        self.extra.insert(key.into(), value);
        self
    }

    /// Expire `ttl` from now (overrides the config's TTL).
    pub fn expires_in(mut self, ttl: Duration) -> Self {
        self.exp = Some(unix_now().saturating_add(ttl.as_secs()));
        self
    }

    /// Expire at `unix_secs`.
    pub fn expires_at(mut self, unix_secs: u64) -> Self {
        self.exp = Some(unix_secs);
        self
    }

    /// Set the issuer (otherwise filled from the config when encoding).
    pub fn issuer(mut self, iss: impl Into<String>) -> Self {
        self.iss = Some(iss.into());
        self
    }

    /// Add an audience.
    pub fn audience(mut self, aud: impl Into<String>) -> Self {
        self.aud.push(aud.into());
        self
    }

    /// A custom claim from [`extra`](Self::extra), deserialized.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.extra.get(key).and_then(|v| T::deserialize(v).ok())
    }

    /// Whether `roles` contains `role`.
    pub fn has_role(&self, role: &str) -> bool {
        HasRoles::has_role(self, role)
    }

    /// Whether `scopes` contains `scope`.
    pub fn has_scope(&self, scope: &str) -> bool {
        HasRoles::has_scope(self, scope)
    }
}

impl HasRoles for Claims {
    fn roles(&self) -> &[String] {
        &self.roles
    }
    fn scopes(&self) -> &[String] {
        &self.scopes
    }
}

/// A string or an array of strings.
mod one_or_many {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        Ok(match Option::<OneOrMany>::deserialize(d)? {
            None => Vec::new(),
            Some(OneOrMany::One(s)) => vec![s],
            Some(OneOrMany::Many(v)) => v,
        })
    }

    pub fn serialize<S: Serializer>(v: &[String], s: S) -> Result<S::Ok, S::Error> {
        match v {
            [one] => one.serialize(s),
            many => many.serialize(s),
        }
    }
}

/// An array of strings or one space-separated string (OAuth `scope`).
mod space_separated {
    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scopes {
        Joined(String),
        List(Vec<String>),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        Ok(match Option::<Scopes>::deserialize(d)? {
            None => Vec::new(),
            Some(Scopes::Joined(s)) => s.split_whitespace().map(str::to_owned).collect(),
            Some(Scopes::List(v)) => v,
        })
    }
}

fn config(req: &Request) -> Result<&JwtConfig> {
    req.state::<JwtConfig>().ok_or_else(|| {
        Error::internal("JWT is not configured: register a JwtConfig with `App::jwt(..)`")
            .with_code("auth_misconfigured")
            .diagnosed("ZL0075", None)
    })
}

/// Extracts and validates the bearer JWT, yielding its claims.
///
/// Needs a [`JwtConfig`] registered with [`App::jwt`] (otherwise `500`).
/// Missing token: `401 missing_credentials`; expired: `401 token_expired`;
/// anything else wrong: `401 invalid_token`.
///
/// ```
/// use zapline::auth::{Claims, Jwt};
///
/// async fn me(Jwt(claims): Jwt) -> String {
///     claims.sub
/// }
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Jwt<C = Claims>(pub C);

impl<C> Jwt<C> {
    /// The claims.
    pub fn into_inner(self) -> C {
        self.0
    }
}

impl<C> Deref for Jwt<C> {
    type Target = C;
    fn deref(&self) -> &C {
        &self.0
    }
}

impl<C> DerefMut for Jwt<C> {
    fn deref_mut(&mut self) -> &mut C {
        &mut self.0
    }
}

impl<C: DeserializeOwned + Send + 'static> FromRequestParts for Jwt<C> {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        let cfg = config(req)?;
        let token = bearer(req)?.ok_or_else(|| missing_credentials(Some(CHALLENGE)))?;
        cfg.decode(token).map(Jwt)
    }
}

/// An [`Authenticator`] for bearer JWTs, producing the token's claims `C`.
/// Requests without a bearer token are anonymous; invalid tokens are
/// rejected (`401 invalid_token` / `token_expired`).
///
/// Usually used through [`Authenticate::jwt`]; for custom claims:
/// `Authenticate::new(JwtAuthenticator::<MyClaims>::new())`.
pub struct JwtAuthenticator<C = Claims> {
    config: Option<JwtConfig>,
    _claims: PhantomData<fn() -> C>,
}

impl<C> JwtAuthenticator<C> {
    /// Use the [`JwtConfig`] registered with [`App::jwt`].
    pub fn new() -> Self {
        JwtAuthenticator {
            config: None,
            _claims: PhantomData,
        }
    }

    /// Use this config instead of the app's.
    pub fn with_config(config: JwtConfig) -> Self {
        JwtAuthenticator {
            config: Some(config),
            _claims: PhantomData,
        }
    }
}

impl<C> Default for JwtAuthenticator<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> Authenticator for JwtAuthenticator<C>
where
    C: DeserializeOwned + Clone + Send + Sync + 'static,
{
    type User = C;

    async fn authenticate(&self, req: &Request) -> Result<Option<C>> {
        let Some(token) = bearer(req)? else {
            return Ok(None);
        };
        let cfg = match &self.config {
            Some(cfg) => cfg,
            None => config(req)?,
        };
        cfg.decode(token).map(Some)
    }

    fn challenge(&self) -> Option<HeaderValue> {
        Some(CHALLENGE)
    }
}

impl Authenticate<JwtAuthenticator<Claims>> {
    /// Bearer-JWT authentication with the app's [`JwtConfig`]: requires a
    /// valid token on every route it wraps (except `.allow_anonymous()`
    /// routes; call `.optional()` to allow anonymous requests) and enforces
    /// role/scope policies against the token's [`Claims`].
    ///
    /// ```no_run
    /// use zapline::auth::{Authenticate, Claims, CurrentUser, JwtConfig};
    /// use zapline::prelude::*;
    ///
    /// let api = Routes::new()
    ///     .get("/me", |CurrentUser(c): CurrentUser<Claims>| async move { c.sub })
    ///     .post("/admin", || async { "ok" }).require_roles(["admin"]);
    ///
    /// App::new()
    ///     .jwt(JwtConfig::hs256(std::env::var("JWT_SECRET").unwrap()))
    ///     .nest("/api", api.wrap(Authenticate::jwt()))
    ///     .run("0.0.0.0:8080")
    ///     .unwrap();
    /// ```
    pub fn jwt() -> Self {
        Authenticate::new(JwtAuthenticator::new())
            .required()
            .authorize()
    }

    /// Like [`jwt`](Self::jwt) with an explicit config.
    pub fn jwt_with(config: JwtConfig) -> Self {
        Authenticate::new(JwtAuthenticator::with_config(config))
            .required()
            .authorize()
    }
}

impl App {
    /// Register the [`JwtConfig`] used by [`Jwt`], [`Authenticate::jwt`] and
    /// `State<JwtConfig>`.
    pub fn jwt(self, config: JwtConfig) -> Self {
        self.state(config)
    }
}
