//! JSON Web Tokens (feature `jwt`), signed and verified with `jsonwebtoken`
//! (aws-lc backend).
//!
//! A [`JwtConfig`] holds a key ring: the signing key plus any number of
//! verification keys, picked by the token's `kid` header. Keys can be added,
//! rotated and retired while the app runs ([`JwtConfig::add_key`],
//! [`JwtConfig::rotate`], [`JwtConfig::retire`]), published as a JWK Set
//! ([`App::serve_jwks`]) and, with the `jwks` feature, fetched from an
//! identity provider's JWKS endpoint ([`Jwks`](super::Jwks)).

use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::header::CACHE_CONTROL;
use http::HeaderValue;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, PublicKeyUse};
use jsonwebtoken::Header;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use jsonwebtoken::jwk::{Jwk, JwkSet};
pub use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Validation};

use super::authenticate::{Authenticate, Authenticator};
use super::bearer::bearer;
use super::guard::HasRoles;
use super::{missing_credentials, unauthorized};
use crate::extract::FromRequestParts;
use crate::response::{IntoResponse, Response};
use crate::{App, Error, Json, Request, Result, Routes};

/// Longest token accepted, in bytes.
const MAX_TOKEN_LEN: usize = 16 * 1024;
/// Minimum HMAC secret length (RFC 7518 §3.2: at least the hash size).
const MIN_HMAC_SECRET: usize = 32;

const CHALLENGE: HeaderValue = HeaderValue::from_static("Bearer");
const CHALLENGE_INVALID: HeaderValue = HeaderValue::from_static("Bearer error=\"invalid_token\"");
const CHALLENGE_EXPIRED: HeaderValue =
    HeaderValue::from_static("Bearer error=\"invalid_token\", error_description=\"token expired\"");

/// The asymmetric algorithms a JWK may be used with.
pub(crate) const ASYMMETRIC: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// A verification key in a key ring (local, or fetched from a JWKS).
pub(crate) struct VerifyKey {
    pub(crate) kid: Option<String>,
    pub(crate) alg: Algorithm,
    pub(crate) key: DecodingKey,
}

#[derive(Clone)]
struct Signer {
    kid: Option<String>,
    alg: Algorithm,
    key: EncodingKey,
}

/// The keys of a config: who signs, and every key that verifies (the
/// signing key's own verification key first).
#[derive(Clone, Default)]
struct KeyRing {
    signer: Option<Signer>,
    keys: Vec<Arc<VerifyKey>>,
}

impl KeyRing {
    /// Insert `key` (replacing a key with the same `kid`) at the front.
    fn insert_front(&mut self, key: VerifyKey) {
        if let Some(kid) = &key.kid {
            self.keys.retain(|k| k.kid.as_ref() != Some(kid));
        }
        self.keys.insert(0, Arc::new(key));
    }
}

/// The key ring shared by a config and its clones, swapped as a whole so a
/// decode never sees a half-applied rotation.
#[derive(Default)]
struct KeyCell(RwLock<Arc<KeyRing>>);

impl KeyCell {
    fn new(ring: KeyRing) -> Arc<Self> {
        Arc::new(KeyCell(RwLock::new(Arc::new(ring))))
    }

    fn load(&self) -> Arc<KeyRing> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn update<R>(&self, f: impl FnOnce(&mut KeyRing) -> R) -> R {
        let mut guard = self.0.write().unwrap_or_else(|e| e.into_inner());
        let mut ring = KeyRing::clone(&guard);
        let out = f(&mut ring);
        *guard = Arc::new(ring);
        out
    }
}

/// One key for a [`JwtConfig`] key ring: an algorithm, a `kid`, the key that
/// verifies and, to sign, the private key.
///
/// ```
/// use zapline::auth::{Claims, JwtConfig, JwtKey};
///
/// let jwt = JwtConfig::hs256("the-first-secret-of-at-least-32-bytes").key_id("2026-09");
/// let old = jwt.encode(&Claims::new("user-1")).unwrap();
///
/// // Sign with a new key from now on; tokens of the old one still verify.
/// jwt.rotate(JwtKey::hs256("2026-10", "the-second-secret-of-at-least-32-bytes").unwrap())
///     .unwrap();
/// assert_eq!(jwt.decode::<Claims>(&old).unwrap().sub, "user-1");
/// ```
#[derive(Clone)]
pub struct JwtKey {
    kid: Option<String>,
    alg: Algorithm,
    decoding: DecodingKey,
    encoding: Option<EncodingKey>,
}

impl fmt::Debug for JwtKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JwtKey")
            .field("kid", &self.kid)
            .field("algorithm", &self.alg)
            .field("can_sign", &self.encoding.is_some())
            .finish_non_exhaustive()
    }
}

impl JwtKey {
    /// Any algorithm and keys. `encoding` is needed only to sign.
    ///
    /// ```
    /// use zapline::auth::{Algorithm, DecodingKey, JwtKey};
    ///
    /// let key = JwtKey::new("k1", Algorithm::HS256, DecodingKey::from_secret(&[7; 32]), None);
    /// assert_eq!(key.kid(), Some("k1"));
    /// ```
    pub fn new(
        kid: impl Into<String>,
        algorithm: Algorithm,
        decoding: DecodingKey,
        encoding: Option<EncodingKey>,
    ) -> Self {
        JwtKey {
            kid: Some(kid.into()),
            alg: algorithm,
            decoding,
            encoding,
        }
    }

    /// HMAC-SHA256 with a shared secret of at least 32 bytes.
    ///
    /// ```
    /// let key = zapline::auth::JwtKey::hs256("k1", "a-secret-of-at-least-thirty-two-bytes");
    /// assert!(key.is_ok());
    /// assert!(zapline::auth::JwtKey::hs256("k1", "short").is_err());
    /// ```
    pub fn hs256(kid: impl Into<String>, secret: impl AsRef<[u8]>) -> Result<Self> {
        let secret = secret.as_ref();
        if secret.len() < MIN_HMAC_SECRET {
            return Err(Error::internal(format!(
                "JWT HMAC secret must be at least {MIN_HMAC_SECRET} bytes, got {}",
                secret.len()
            )));
        }
        Ok(Self::new(
            kid,
            Algorithm::HS256,
            DecodingKey::from_secret(secret),
            Some(EncodingKey::from_secret(secret)),
        ))
    }

    /// RS256 from a PEM public key and, to sign, a PEM private key.
    ///
    /// ```no_run
    /// let pem = std::fs::read("keys/2026-10.pub.pem").unwrap();
    /// let key = zapline::auth::JwtKey::rs256_pem("2026-10", &pem, None).unwrap();
    /// ```
    pub fn rs256_pem<P: AsRef<[u8]>>(
        kid: impl Into<String>,
        public_pem: P,
        private_pem: Option<P>,
    ) -> Result<Self> {
        let decoding = DecodingKey::from_rsa_pem(public_pem.as_ref()).map_err(key_error)?;
        let encoding = private_pem
            .map(|p| EncodingKey::from_rsa_pem(p.as_ref()).map_err(key_error))
            .transpose()?;
        Ok(Self::new(kid, Algorithm::RS256, decoding, encoding))
    }

    /// ES256 (P-256) from a PEM public key and, to sign, a PKCS#8 PEM
    /// private key.
    ///
    /// ```no_run
    /// let public = std::fs::read("keys/ec.pub.pem").unwrap();
    /// let private = std::fs::read("keys/ec.pem").unwrap();
    /// let key = zapline::auth::JwtKey::es256_pem("ec-1", &public, Some(&private)).unwrap();
    /// ```
    pub fn es256_pem<P: AsRef<[u8]>>(
        kid: impl Into<String>,
        public_pem: P,
        private_pem: Option<P>,
    ) -> Result<Self> {
        let decoding = DecodingKey::from_ec_pem(public_pem.as_ref()).map_err(key_error)?;
        let encoding = private_pem
            .map(|p| EncodingKey::from_ec_pem(p.as_ref()).map_err(key_error))
            .transpose()?;
        Ok(Self::new(kid, Algorithm::ES256, decoding, encoding))
    }

    /// EdDSA (Ed25519) from a PEM public key and, to sign, a PKCS#8 PEM
    /// private key.
    ///
    /// ```no_run
    /// let public = std::fs::read("keys/ed.pub.pem").unwrap();
    /// let key = zapline::auth::JwtKey::ed25519_pem("ed-1", &public, None).unwrap();
    /// ```
    pub fn ed25519_pem<P: AsRef<[u8]>>(
        kid: impl Into<String>,
        public_pem: P,
        private_pem: Option<P>,
    ) -> Result<Self> {
        let decoding = DecodingKey::from_ed_pem(public_pem.as_ref()).map_err(key_error)?;
        let encoding = private_pem
            .map(|p| EncodingKey::from_ed_pem(p.as_ref()).map_err(key_error))
            .transpose()?;
        Ok(Self::new(kid, Algorithm::EdDSA, decoding, encoding))
    }

    /// A verification key from a public JWK (RSA, EC P-256/P-384 or
    /// Ed25519). The algorithm is the JWK's `alg`, or the one its key type
    /// implies (RSA: RS256). Symmetric (`oct`) and encryption keys are
    /// refused.
    ///
    /// ```
    /// use zapline::auth::{Algorithm, Jwk, JwtKey};
    ///
    /// let jwk: Jwk = serde_json::from_value(serde_json::json!({
    ///     "kty": "OKP", "crv": "Ed25519", "kid": "ed-1",
    ///     "x": "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
    /// })).unwrap();
    /// let key = JwtKey::from_jwk(&jwk).unwrap();
    /// assert_eq!(key.algorithm(), Algorithm::EdDSA);
    /// ```
    pub fn from_jwk(jwk: &Jwk) -> Result<Self> {
        let (alg, decoding) = jwk_key(jwk).map_err(|why| {
            Error::internal(format!("unusable JWK {:?}: {why}", jwk.common.key_id))
        })?;
        Ok(JwtKey {
            kid: jwk.common.key_id.clone(),
            alg,
            decoding,
            encoding: None,
        })
    }

    /// The key id (`kid`).
    pub fn kid(&self) -> Option<&str> {
        self.kid.as_deref()
    }

    /// The algorithm this key signs and verifies with.
    pub fn algorithm(&self) -> Algorithm {
        self.alg
    }

    fn verify_key(&self) -> VerifyKey {
        VerifyKey {
            kid: self.kid.clone(),
            alg: self.alg,
            key: self.decoding.clone(),
        }
    }
}

/// The algorithm and verification key of a public JWK, or why it can't be
/// used. The algorithm must fit the key type (no `alg: HS256` on an RSA key).
pub(crate) fn jwk_key(jwk: &Jwk) -> std::result::Result<(Algorithm, DecodingKey), &'static str> {
    if matches!(
        jwk.common.public_key_use,
        Some(PublicKeyUse::Encryption | PublicKeyUse::Other(_))
    ) {
        return Err("not a signature key");
    }
    let declared = match jwk.common.key_algorithm {
        None => None,
        Some(a) => Some(Algorithm::try_from(a).map_err(|_| "unsupported algorithm")?),
    };
    let alg = match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => match declared {
            None => Algorithm::RS256,
            Some(
                a @ (Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512),
            ) => a,
            Some(_) => return Err("algorithm does not fit an RSA key"),
        },
        AlgorithmParameters::EllipticCurve(p) => {
            let implied = match p.curve {
                EllipticCurve::P256 => Algorithm::ES256,
                EllipticCurve::P384 => Algorithm::ES384,
                _ => return Err("unsupported curve"),
            };
            if declared.is_some_and(|a| a != implied) {
                return Err("algorithm does not fit the curve");
            }
            implied
        }
        AlgorithmParameters::OctetKeyPair(p) => {
            if p.curve != EllipticCurve::Ed25519 {
                return Err("unsupported curve");
            }
            if declared.is_some_and(|a| a != Algorithm::EdDSA) {
                return Err("algorithm does not fit an Ed25519 key");
            }
            Algorithm::EdDSA
        }
        AlgorithmParameters::OctetKey(_) => return Err("symmetric keys are not accepted"),
        _ => return Err("unsupported key type"),
    };
    let key = DecodingKey::from_jwk(jwk).map_err(|_| "malformed key")?;
    Ok((alg, key))
}

/// How tokens are signed and validated. Cheap to clone; register it with
/// [`App::jwt`] so the [`Jwt`] extractor and [`Authenticate::jwt`] find it,
/// and extract it with `State<JwtConfig>` to issue tokens.
///
/// Defaults: `exp` is required and validated (60 s leeway), `nbf` is
/// validated when present, issued tokens live for one hour.
///
/// A config holds a key ring: the signing key and every key that verifies,
/// chosen by the token's `kid` header. Clones share the ring, so
/// [`rotate`](Self::rotate) on the `State<JwtConfig>` a handler extracted
/// takes effect for the whole app.
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
    keys: Arc<KeyCell>,
    validation: Validation,
    /// `validation` pinned to each algorithm (index: `ALL`), built on first
    /// use and reset by every builder method. jsonwebtoken requires one
    /// algorithm family per `Validation`.
    pinned: OnceLock<Arc<[Validation]>>,
    required: HashSet<String>,
    ttl: Duration,
    issuer: Option<String>,
    audience: Vec<String>,
    #[cfg(feature = "jwks")]
    remote: Option<super::jwks::Jwks>,
}

/// Every signature algorithm, in the order of `Inner::pinned`.
const ALL: [Algorithm; 12] = [
    Algorithm::HS256,
    Algorithm::HS384,
    Algorithm::HS512,
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

impl fmt::Debug for JwtConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let i = &self.inner;
        let ring = i.keys.load();
        let kids: Vec<_> = ring.keys.iter().map(|k| k.kid.as_deref()).collect();
        let mut d = f.debug_struct("JwtConfig");
        d.field("algorithm", &self.algorithm())
            .field("can_sign", &ring.signer.is_some())
            .field("key_ids", &kids)
            .field("issuer", &i.issuer)
            .field("audience", &i.audience)
            .field("ttl", &i.ttl);
        #[cfg(feature = "jwks")]
        d.field("jwks", &i.remote);
        d.finish_non_exhaustive()
    }
}

impl JwtConfig {
    /// Full control: any algorithm and keys. `encoding` is needed only to
    /// issue tokens.
    ///
    /// ```
    /// use zapline::auth::{Algorithm, DecodingKey, JwtConfig};
    ///
    /// let verify_only = JwtConfig::new(Algorithm::HS256, DecodingKey::from_secret(&[7; 32]), None);
    /// assert!(verify_only.encode(&zapline::auth::Claims::new("x")).is_err());
    /// ```
    pub fn new(algorithm: Algorithm, decoding: DecodingKey, encoding: Option<EncodingKey>) -> Self {
        let ring = KeyRing {
            signer: encoding.map(|key| Signer {
                kid: None,
                alg: algorithm,
                key,
            }),
            keys: vec![Arc::new(VerifyKey {
                kid: None,
                alg: algorithm,
                key: decoding,
            })],
        };
        Self::with_ring(algorithm, ring)
    }

    fn with_ring(algorithm: Algorithm, ring: KeyRing) -> Self {
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
                keys: KeyCell::new(ring),
                validation,
                pinned: OnceLock::new(),
                required,
                ttl: Duration::from_secs(3600),
                issuer: None,
                audience: Vec::new(),
                #[cfg(feature = "jwks")]
                remote: None,
            }),
        }
    }

    /// A config whose first key is `key`: it signs if it has a private part,
    /// and verifies tokens carrying its `kid`.
    ///
    /// ```
    /// use zapline::auth::{Algorithm, JwtConfig, JwtKey};
    ///
    /// let jwt = JwtConfig::from_key(JwtKey::hs256("k1", "a-secret-of-at-least-thirty-two-bytes").unwrap());
    /// assert_eq!(jwt.algorithm(), Algorithm::HS256);
    /// ```
    pub fn from_key(key: JwtKey) -> Self {
        let ring = KeyRing {
            signer: key.encoding.clone().map(|k| Signer {
                kid: key.kid.clone(),
                alg: key.alg,
                key: k,
            }),
            keys: vec![Arc::new(key.verify_key())],
        };
        Self::with_ring(key.alg, ring)
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

    /// Verify tokens with the keys of a remote JWK Set (an identity
    /// provider's `jwks_uri`), fetched and cached by [`Jwks`](super::Jwks).
    /// The config can't issue tokens. Feature `jwks`.
    ///
    /// ```
    /// use zapline::auth::{Jwks, JwtConfig};
    ///
    /// let jwt = JwtConfig::from_jwks(Jwks::new("https://idp.example.com/.well-known/jwks.json"))
    ///     .issuer("https://idp.example.com")
    ///     .audience("api");
    /// ```
    #[cfg(feature = "jwks")]
    pub fn from_jwks(jwks: super::jwks::Jwks) -> Self {
        Self::with_ring(Algorithm::RS256, KeyRing::default()).jwks(jwks)
    }

    /// Also verify tokens with the keys of a remote JWK Set, for `kid`s the
    /// local key ring doesn't have. Feature `jwks`.
    ///
    /// ```
    /// use zapline::auth::{Jwks, JwtConfig};
    ///
    /// // Our own HS256 tokens, plus RS256 tokens from the company IdP.
    /// let jwt = JwtConfig::hs256("an-example-secret-of-at-least-32-bytes")
    ///     .jwks(Jwks::new("https://idp.example.com/.well-known/jwks.json"));
    /// ```
    #[cfg(feature = "jwks")]
    pub fn jwks(mut self, jwks: super::jwks::Jwks) -> Self {
        self.inner_mut().remote = Some(jwks);
        self
    }

    /// The remote JWK Set configured with [`jwks`](Self::jwks), if any.
    #[cfg(feature = "jwks")]
    pub(crate) fn remote(&self) -> Option<&super::jwks::Jwks> {
        self.inner.remote.as_ref()
    }

    fn inner_mut(&mut self) -> &mut Inner {
        let inner = Arc::make_mut(&mut self.inner);
        inner.pinned = OnceLock::new();
        inner
    }

    /// The validation for a token of algorithm `alg`.
    fn pinned(&self, alg: Algorithm) -> Option<&Validation> {
        let all = self.inner.pinned.get_or_init(|| {
            ALL.iter()
                .map(|alg| {
                    let mut v = self.inner.validation.clone();
                    v.algorithms = vec![*alg];
                    v
                })
                .collect()
        });
        ALL.iter().position(|a| *a == alg).map(|i| &all[i])
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

    /// The `kid` of the config's own key: issued tokens carry it in their
    /// header, and tokens carrying it are verified with that key. Name keys
    /// before rotating them (see [`rotate`](Self::rotate)).
    pub fn key_id(mut self, kid: impl Into<String>) -> Self {
        let kid = kid.into();
        // Copy-on-write: configs this one was cloned from keep their ring.
        let mut ring = KeyRing::clone(&self.inner.keys.load());
        if let Some(signer) = &mut ring.signer {
            signer.kid = Some(kid.clone());
        }
        if let Some(first) = ring.keys.first_mut() {
            *first = Arc::new(VerifyKey {
                kid: Some(kid),
                alg: first.alg,
                key: first.key.clone(),
            });
        }
        self.inner_mut().keys = KeyCell::new(ring);
        self
    }

    /// Adjust the underlying [`Validation`] directly.
    pub fn validation(mut self, f: impl FnOnce(&mut Validation)) -> Self {
        f(&mut self.inner_mut().validation);
        self
    }

    /// The signing algorithm (of the current signing key, after a
    /// [`rotate`](Self::rotate)).
    pub fn algorithm(&self) -> Algorithm {
        self.inner
            .keys
            .load()
            .signer
            .as_ref()
            .map_or(self.inner.algorithm, |s| s.alg)
    }

    /// The `kid` of every local verification key, the signing key's first.
    ///
    /// ```
    /// let jwt = zapline::auth::JwtConfig::hs256("an-example-secret-of-at-least-32-bytes").key_id("k1");
    /// assert_eq!(jwt.key_ids(), ["k1"]);
    /// ```
    pub fn key_ids(&self) -> Vec<String> {
        self.inner
            .keys
            .load()
            .keys
            .iter()
            .filter_map(|k| k.kid.clone())
            .collect()
    }

    /// Add a verification key (replacing a key with the same `kid`): tokens
    /// carrying its `kid` verify with it from now on. Use it to keep an old
    /// key during a rotation, or to publish the next key in the JWK Set
    /// before signing with it. Affects every clone of this config.
    ///
    /// ```
    /// use zapline::auth::{JwtConfig, JwtKey};
    ///
    /// let jwt = JwtConfig::hs256("the-current-secret-of-at-least-32-bytes").key_id("new");
    /// jwt.add_key(JwtKey::hs256("old", "the-previous-secret-of-at-least-32-bytes").unwrap());
    /// assert_eq!(jwt.key_ids(), ["new", "old"]);
    /// ```
    pub fn add_key(&self, key: JwtKey) {
        let key = key.verify_key();
        self.inner.keys.update(|ring| {
            let at = usize::from(ring.signer.is_some());
            if let Some(kid) = &key.kid {
                ring.keys.retain(|k| k.kid.as_ref() != Some(kid));
            }
            let at = at.min(ring.keys.len());
            ring.keys.insert(at, Arc::new(key));
        });
    }

    /// Sign with `key` from now on. The previous signing key stays in the
    /// ring, so tokens it issued keep verifying until they expire or you
    /// [`retire`](Self::retire) it. Affects every clone of this config.
    ///
    /// Errors if `key` has no private part or no `kid` (tokens must name
    /// the key that signed them).
    ///
    /// ```
    /// use zapline::auth::{Claims, JwtConfig, JwtKey};
    ///
    /// let jwt = JwtConfig::hs256("the-first-secret-of-at-least-32-bytes").key_id("k1");
    /// jwt.rotate(JwtKey::hs256("k2", "the-second-secret-of-at-least-32-bytes").unwrap()).unwrap();
    /// assert_eq!(jwt.key_ids(), ["k2", "k1"]);
    /// ```
    pub fn rotate(&self, key: JwtKey) -> Result<()> {
        let Some(encoding) = key.encoding.clone() else {
            return Err(Error::internal(
                "JwtConfig::rotate needs a key with a private part",
            ));
        };
        if key.kid.is_none() {
            return Err(Error::internal("JwtConfig::rotate needs a key with a kid"));
        }
        self.inner.keys.update(|ring| {
            ring.signer = Some(Signer {
                kid: key.kid.clone(),
                alg: key.alg,
                key: encoding,
            });
            ring.insert_front(key.verify_key());
        });
        Ok(())
    }

    /// Stop accepting tokens signed with key `kid`. Returns whether a key
    /// was removed; the current signing key can't be retired (rotate first).
    ///
    /// ```
    /// use zapline::auth::{JwtConfig, JwtKey};
    ///
    /// let jwt = JwtConfig::hs256("the-first-secret-of-at-least-32-bytes").key_id("k1");
    /// jwt.rotate(JwtKey::hs256("k2", "the-second-secret-of-at-least-32-bytes").unwrap()).unwrap();
    /// assert!(!jwt.retire("k2"));
    /// assert!(jwt.retire("k1"));
    /// assert_eq!(jwt.key_ids(), ["k2"]);
    /// ```
    pub fn retire(&self, kid: &str) -> bool {
        self.inner.keys.update(|ring| {
            if ring
                .signer
                .as_ref()
                .is_some_and(|s| s.kid.as_deref() == Some(kid))
            {
                return false;
            }
            let before = ring.keys.len();
            ring.keys.retain(|k| k.kid.as_deref() != Some(kid));
            ring.keys.len() != before
        })
    }

    /// The public half of every local asymmetric key (RSA, EC, Ed25519) as a
    /// JWK Set, for other services to verify this app's tokens. HMAC keys
    /// are never included. [`App::serve_jwks`] serves it.
    ///
    /// ```
    /// let jwt = zapline::auth::JwtConfig::hs256("an-example-secret-of-at-least-32-bytes");
    /// assert!(jwt.public_jwks().keys.is_empty());
    /// ```
    pub fn public_jwks(&self) -> JwkSet {
        let ring = self.inner.keys.load();
        let keys = ring
            .keys
            .iter()
            .filter(|k| ASYMMETRIC.contains(&k.alg))
            .filter_map(|k| {
                let mut jwk = Jwk::from_decoding_key(&k.key, Some(k.alg)).ok()?;
                jwk.common.key_id = k.kid.clone();
                jwk.common.public_key_use = Some(PublicKeyUse::Signature);
                Some(jwk)
            })
            .collect();
        JwkSet { keys }
    }

    /// Sign `claims`. Missing `iat`, `exp` (now + TTL), `iss` and `aud` are
    /// filled in from this config. `claims` must serialize to a JSON object.
    pub fn encode<C: Serialize + ?Sized>(&self, claims: &C) -> Result<String> {
        let i = &self.inner;
        let ring = i.keys.load();
        let signer = ring.signer.as_ref().ok_or_else(|| {
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
        let mut header = Header::new(signer.alg);
        header.kid = signer.kid.clone();
        jsonwebtoken::encode(&header, &value, &signer.key)
            .map_err(|e| Error::internal("failed to sign JWT").with_source(e))
    }

    /// Verify `token` and deserialize its claims, with the keys at hand: the
    /// local ring and, with a remote [`Jwks`](super::Jwks), the keys already
    /// fetched (a missing key is fetched in the background, never waited
    /// for). Request handling uses [`verify`](Self::verify), which waits for
    /// the fetch.
    ///
    /// Errors: `401 token_expired` for expired tokens, `401 invalid_token`
    /// for anything else wrong with the token (both with a `Bearer`
    /// challenge), `503 jwks_unavailable` when the key may exist but the
    /// JWKS endpoint failed, `500` when the configured key is unusable.
    pub fn decode<C: DeserializeOwned>(&self, token: &str) -> Result<C> {
        let ring = match self.prepare(token)? {
            Prepared::Single(key) => return self.decode_with(token, &key, None),
            Prepared::Ring(ring) => ring,
        };
        let header = jsonwebtoken::decode_header(token).map_err(|_| invalid_token())?;
        match self.try_local(token, &header, &ring) {
            Some(done) => done,
            None => {
                #[cfg(feature = "jwks")]
                if let Some(remote) = &self.inner.remote {
                    return self.try_remote(token, &header, remote.cached(header.kid.as_deref()));
                }
                Err(invalid_token())
            }
        }
    }

    /// Like [`decode`](Self::decode), but when the token names a key that
    /// isn't known yet, waits (up to the [`Jwks`](super::Jwks) timeout) for
    /// the remote JWK Set to be fetched. This is what the [`Jwt`] extractor
    /// and [`Authenticate::jwt`] use.
    ///
    /// ```
    /// use zapline::auth::{Claims, JwtConfig};
    ///
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// let jwt = JwtConfig::hs256("an-example-secret-of-at-least-32-bytes");
    /// let token = jwt.encode(&Claims::new("user-1")).unwrap();
    /// let claims: Claims = jwt.verify(&token).await.unwrap();
    /// assert_eq!(claims.sub, "user-1");
    /// # });
    /// ```
    pub async fn verify<C: DeserializeOwned>(&self, token: &str) -> Result<C> {
        #[cfg(feature = "jwks")]
        if let Some(remote) = &self.inner.remote {
            let ring = match self.prepare(token)? {
                Prepared::Single(key) => return self.decode_with(token, &key, None),
                Prepared::Ring(ring) => ring,
            };
            let header = jsonwebtoken::decode_header(token).map_err(|_| invalid_token())?;
            if let Some(done) = self.try_local(token, &header, &ring) {
                return done;
            }
            let found = remote.key(header.kid.as_deref()).await;
            return self.try_remote(token, &header, found);
        }
        self.decode(token)
    }

    /// Length checks, then the key ring; a ring of one unnamed key (the
    /// classic single-key config) is verified without reading the header.
    fn prepare(&self, token: &str) -> Result<Prepared> {
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            return Err(invalid_token());
        }
        let ring = self.inner.keys.load();
        #[cfg(feature = "jwks")]
        let remote = self.inner.remote.is_some();
        #[cfg(not(feature = "jwks"))]
        let remote = false;
        if let [only] = ring.keys.as_slice() {
            if only.kid.is_none() && !remote && self.inner.validation.algorithms.contains(&only.alg)
            {
                return Ok(Prepared::Single(only.clone()));
            }
        }
        Ok(Prepared::Ring(ring))
    }

    /// Verify with the local key the token names, or the unnamed keys of its
    /// algorithm; `None` when no local key applies.
    fn try_local<C: DeserializeOwned>(
        &self,
        token: &str,
        header: &Header,
        ring: &KeyRing,
    ) -> Option<Result<C>> {
        let named = match header.kid.as_deref() {
            Some(kid) => ring.keys.iter().find(|k| k.kid.as_deref() == Some(kid)),
            // No `kid`: a lone key is unambiguous.
            None => match ring.keys.as_slice() {
                [only] if only.alg == header.alg => Some(only),
                _ => None,
            },
        };
        if let Some(key) = named {
            return Some(self.decode_with(token, key, Some(header)));
        }
        // Unnamed keys (configs from before key ids) accept any `kid`.
        let mut tried = None;
        for key in ring
            .keys
            .iter()
            .filter(|k| k.kid.is_none() && k.alg == header.alg)
        {
            match self.decode_with(token, key, Some(header)) {
                Err(e) if e.code() == Some("invalid_token") => tried = Some(Err(e)),
                done => return Some(done),
            }
        }
        tried
    }

    #[cfg(feature = "jwks")]
    fn try_remote<C: DeserializeOwned>(
        &self,
        token: &str,
        header: &Header,
        found: Result<Option<Arc<VerifyKey>>>,
    ) -> Result<C> {
        match found? {
            Some(key) => self.decode_with(token, &key, Some(header)),
            None => Err(invalid_token()),
        }
    }

    /// Verify with one key. With a parsed `header`, its `alg` must be the
    /// key's: a token can't pick how its key is used (no RS/HS confusion).
    fn decode_with<C: DeserializeOwned>(
        &self,
        token: &str,
        key: &VerifyKey,
        header: Option<&Header>,
    ) -> Result<C> {
        let validation = match header {
            None => &self.inner.validation,
            Some(h) if h.alg != key.alg => return Err(invalid_token()),
            Some(_) => self.pinned(key.alg).ok_or_else(invalid_token)?,
        };
        jsonwebtoken::decode::<C>(token, &key.key, validation)
            .map(|data| data.claims)
            .map_err(decode_error)
    }
}

enum Prepared {
    Single(Arc<VerifyKey>),
    Ring(Arc<KeyRing>),
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
        cfg.verify(token).await.map(Jwt)
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
        cfg.verify(token).await.map(Some)
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
    /// `State<JwtConfig>`. With a remote [`Jwks`](super::Jwks), its keys are
    /// fetched at startup (best effort: a failure is logged and retried on
    /// the first request).
    ///
    /// ```
    /// let app = zapline::App::new()
    ///     .jwt(zapline::auth::JwtConfig::hs256("an-example-secret-of-at-least-32-bytes"));
    /// ```
    pub fn jwt(self, config: JwtConfig) -> Self {
        #[cfg(feature = "jwks")]
        if let Some(remote) = config.remote().cloned() {
            return self.state(config).on_startup(move |_| async move {
                remote.warm().await;
                Ok(())
            });
        }
        self.state(config)
    }

    /// Serve the public keys of the registered [`JwtConfig`] as a JWK Set
    /// (RFC 7517) at `path`, conventionally `/.well-known/jwks.json`, so
    /// other services can verify this app's tokens (with [`Jwks`](super::Jwks)
    /// in another zapline app). The set is read on every request, so
    /// rotations show up at once; responses may be cached for five minutes.
    ///
    /// ```no_run
    /// use zapline::auth::{JwtConfig, JwtKey};
    ///
    /// let public = std::fs::read("keys/2026-10.pub.pem").unwrap();
    /// let private = std::fs::read("keys/2026-10.pem").unwrap();
    /// let key = JwtKey::es256_pem("2026-10", &public, Some(&private)).unwrap();
    /// let app = zapline::App::new()
    ///     .jwt(JwtConfig::from_key(key))
    ///     .serve_jwks("/.well-known/jwks.json");
    /// ```
    pub fn serve_jwks(self, path: &str) -> Self {
        self.mount(crate::dev::internal(Routes::new().get(path, jwks_handler)))
    }
}

async fn jwks_handler(req: Request) -> Response {
    let cfg = match config(&req) {
        Ok(cfg) => cfg,
        Err(e) => return e.into_response(),
    };
    let mut res = Json(cfg.public_jwks()).into_response();
    res.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    res
}
