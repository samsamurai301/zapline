//! Remote JWK Sets against an in-process JWKS server on a real socket:
//! RS256/ES256/EdDSA keys, refresh on unknown `kid` (rate limited), stale
//! keys on errors, timeouts, OpenID discovery, algorithm confusion, and a
//! rotation published by one zapline app and followed by another.
#![cfg(feature = "jwks")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD as B64};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use zapline::auth::{Claims, Jwks, Jwt, JwtConfig, JwtKey};
use zapline::prelude::*;

// Test-only RSA keys (2048 bit), the same as in tests/jwt.rs.
const RSA_PRIVATE: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEogIBAAKCAQEAt/uTPjc2vvXbWEgAE4T+ZNhU/cy2MPJFZXdFuNRGcu/e4G8F
cEULiWvFcGHETgkdKJDM0MYV89F24sdpIICAyRvLgqnmKfycmyXZ+e0qWzbd9zVd
U+Yuu4s7ONCzMpRZ0CgzKOXy75US0OsYjA7V3m9DufLP/6OxvpWck+zDrVS9dQHG
AqEFnxVa922nBlpb7U2HWxhh79T1sFLRvkJyFHk7IQQQe1mGq42VnJmfKuJmK6O7
0uSNGUvh3i5uyIiRhUXFm5+Q0Dp6UrYYvbrwmH+/FaYUGMKimelHwrL96cZ7qQJZ
Gfbn8dHfKceaPX1l1uL8k8BQj5DhJc2uw3s04QIDAQABAoIBAAQrFV4gSIOkp+0k
56hWWwa6rMvgCqcRPIFDMoRj5GfpbbzbLXeQ5bIEJXT/9T9o9XNcOuFMzswdv5PK
I/3+61Jv6F+nE1ys9hKNyPwTyeHgwsA9dUY+JAQbvQiMfrYNDMiqhuSZhE3pRepC
nH72PXvMoyKTXbygLABYlpn2kBH2z65+Rur7qLpEnIFylRj+vTvrZKyI38ZijIie
jWlFZ+fifHPgTIWLPuAdQMEDa9HiS8jp0alIDlK/OhHO/M6itbkjuvUG3AkqNRtc
RWu+ZK/i/OmlfiaGh7BhDrg3LVuKzbkyoALhKjwGIzr1s/AKHRizw+cU9BfOJK4+
KGzNdAECgYEA5Fw63+hRrTqgAcK2mE4eOyPb85DrqtsIm3TCjqCx3Lbe5cV5lq3+
PLwpMp2tWFYFu+ltSFTuzlDR1/+0WBS5MBxhw/b5xyzUoBqe1A79djU+qNszwTWa
5cy7w4wJuMpyY8+soyhROHWxLG6ma/819ird4k8f1cxgaWjrPtVXFLMCgYEAzkBN
I2cbZSnh+UPE01ggBQSlVkjxNqhCeE6eHnTNnzZLPaPTPBrp+aG2/uGDcYDF5QQB
UU1jVKUJe+W+MH3TCvPzTNKt4VuzZEsp5Gd2Ozxp6h5yFTqfxP1TMGhnNKdM+ngr
2NtgHTX+nq4A0arAwfHwuPMZllsUYkq9bO/i4hsCgYAQVqz7sjKN02D5X0YTNZui
HZmyHo9OBSfjf4IgFE1FfmwKQovFhSNokwnbjj6F0HnOjQ/9md4QMuYgrzHnD2yE
jbNNAxGx93orDlyIzZ3C5GJcUHquezhe8rwIbK+RYiK3E3fFoK/W+0y24h/UIElr
GHwRohmkAWBwikoyj68QUwKBgH1r0ry9nYYmMvlugYl3pHkIuzY+QxjWtx0LydGB
e+zeimCgd/qr7y5TbcGPB2yFP1Il3ls/cTDu71Z9fetlCGo3sPM9hmltZyiNIJt5
YwdglJO/Eebg8HlNB0J/qor1VbVWUGud89OvmveJnovhhzgpVRZTFnM9Tab8Bjbx
pYmTAoGAE5LzdKVALaMmFKYSV6hpVdTstNWyujk3JcS5mqTROTTYQl9Zkg7Xtg6J
qnwbkk6DplQoYuwZtEf+7qZvKgXuiF4tOXwEYI/g0Gz/iauOv41fpLeSHzewa+nS
lQGXqyfnrq9w775rlTxG2ovz72LsAl8hpEKE8eT62jlq8vKy28E=
-----END RSA PRIVATE KEY-----";
const RSA_PUBLIC: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAt/uTPjc2vvXbWEgAE4T+
ZNhU/cy2MPJFZXdFuNRGcu/e4G8FcEULiWvFcGHETgkdKJDM0MYV89F24sdpIICA
yRvLgqnmKfycmyXZ+e0qWzbd9zVdU+Yuu4s7ONCzMpRZ0CgzKOXy75US0OsYjA7V
3m9DufLP/6OxvpWck+zDrVS9dQHGAqEFnxVa922nBlpb7U2HWxhh79T1sFLRvkJy
FHk7IQQQe1mGq42VnJmfKuJmK6O70uSNGUvh3i5uyIiRhUXFm5+Q0Dp6UrYYvbrw
mH+/FaYUGMKimelHwrL96cZ7qQJZGfbn8dHfKceaPX1l1uL8k8BQj5DhJc2uw3s0
4QIDAQAB
-----END PUBLIC KEY-----";

const SECRET: &str = "a-shared-secret-that-is-at-least-32-bytes";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn rsa(kid: &str) -> JwtKey {
    JwtKey::rs256_pem(kid, RSA_PUBLIC, Some(RSA_PRIVATE)).unwrap()
}

fn ec(kid: &str) -> JwtKey {
    let kp = rcgen::KeyPair::generate().unwrap();
    JwtKey::es256_pem(
        kid,
        kp.public_key_pem().as_str(),
        Some(kp.serialize_pem().as_str()),
    )
    .unwrap()
}

fn ed(kid: &str) -> JwtKey {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    JwtKey::ed25519_pem(
        kid,
        kp.public_key_pem().as_str(),
        Some(kp.serialize_pem().as_str()),
    )
    .unwrap()
}

/// A token for `sub` signed by `key`.
fn sign(key: &JwtKey, sub: &str) -> String {
    JwtConfig::from_key(key.clone())
        .encode(&Claims::new(sub))
        .unwrap()
}

/// The public JWK Set of `keys`.
fn set_of(keys: &[&JwtKey]) -> Value {
    let ring = JwtConfig::from_key(keys[0].clone());
    for k in &keys[1..] {
        ring.add_key((*k).clone());
    }
    serde_json::to_value(ring.public_jwks()).unwrap()
}

fn err_code(e: &Error) -> (u16, &str) {
    (e.status().as_u16(), e.code().unwrap_or(""))
}

/// The fake identity provider: serves `body` at `/jwks` with `status`
/// after `delay_ms`, counting fetches.
#[derive(Clone)]
struct Idp {
    body: Arc<Mutex<Value>>,
    status: Arc<AtomicU16>,
    delay_ms: Arc<AtomicU64>,
    hits: Arc<AtomicUsize>,
    addr: SocketAddr,
}

impl Idp {
    async fn start(body: Value) -> Idp {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let idp = Idp {
            body: Arc::new(Mutex::new(body)),
            status: Arc::new(AtomicU16::new(200)),
            delay_ms: Arc::new(AtomicU64::new(0)),
            hits: Arc::new(AtomicUsize::new(0)),
            addr: listener.local_addr().unwrap(),
        };
        let issuer = idp.issuer();
        let app = App::new()
            .state(idp.clone())
            .get("/jwks", |State(idp): State<Idp>| async move {
                idp.hits.fetch_add(1, Ordering::SeqCst);
                let delay = idp.delay_ms.load(Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let status = StatusCode::from_u16(idp.status.load(Ordering::SeqCst)).unwrap();
                let body = idp.body.lock().unwrap().clone();
                (status, Json(body)).into_response()
            })
            .get(
                "/.well-known/openid-configuration",
                move |State(idp): State<Idp>| {
                    let issuer = issuer.clone();
                    async move {
                        Json(json!({
                            "issuer": issuer,
                            "jwks_uri": format!("{}/jwks", idp.issuer()),
                        }))
                    }
                },
            );
        tokio::spawn(app.serve(listener));
        idp
    }

    fn url(&self) -> String {
        format!("{}/jwks", self.issuer())
    }

    fn issuer(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn serve(&self, body: Value) {
        *self.body.lock().unwrap() = body;
    }

    fn fail_with(&self, status: u16) {
        self.status.store(status, Ordering::SeqCst);
    }
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn verifies_rs256_es256_and_eddsa_keys_from_a_remote_set() {
    let (r, e, d) = (rsa("rsa-1"), ec("ec-1"), ed("ed-1"));
    let idp = Idp::start(set_of(&[&r, &e, &d])).await;
    let jwks = Jwks::new(idp.url());
    let jwt = JwtConfig::from_jwks(jwks.clone());

    // Nothing cached yet: the synchronous decode can't wait for the fetch.
    let token = sign(&r, "ada");
    assert!(jwt.decode::<Claims>(&token).is_err());

    for (key, sub) in [(&r, "ada"), (&e, "grace"), (&d, "linus")] {
        let claims: Claims = jwt.verify(&sign(key, sub)).await.unwrap();
        assert_eq!(claims.sub, sub);
    }
    // One fetch served them all (the decode above may have started it).
    assert_eq!(idp.hits(), 1);
    // Now cached, so the synchronous decode works too.
    assert_eq!(jwt.decode::<Claims>(&token).unwrap().sub, "ada");

    let mut kids = jwks.key_ids();
    kids.sort();
    assert_eq!(kids, ["ec-1", "ed-1", "rsa-1"]);
    assert!(jwt.encode(&Claims::new("x")).is_err(), "verify-only");
}

#[tokio::test]
async fn an_unknown_kid_refreshes_at_most_once_per_interval() {
    let k1 = rsa("k1");
    let idp = Idp::start(set_of(&[&k1])).await;
    let interval = Duration::from_millis(300);
    let jwks = Jwks::new(idp.url()).min_refresh_interval(interval);
    let jwt = JwtConfig::from_jwks(jwks.clone());

    jwt.verify::<Claims>(&sign(&k1, "a")).await.unwrap();
    assert_eq!(idp.hits(), 1);

    // The provider adds k2; the first token naming it triggers a refresh.
    let k2 = ec("k2");
    idp.serve(set_of(&[&k2, &k1]));
    tokio::time::sleep(interval).await;
    let claims: Claims = jwt.verify(&sign(&k2, "b")).await.unwrap();
    assert_eq!(claims.sub, "b");
    assert_eq!(idp.hits(), 2);

    // Made-up kids can't make it fetch again within the interval.
    for i in 0..25 {
        let forged = sign(&rsa(&format!("bogus-{i}")), "mallory");
        let e = jwt.verify::<Claims>(&forged).await.unwrap_err();
        assert_eq!(err_code(&e), (401, "invalid_token"));
    }
    assert_eq!(idp.hits(), 2);
    // Known keys keep verifying from the cache meanwhile.
    jwt.verify::<Claims>(&sign(&k1, "a")).await.unwrap();
    assert_eq!(idp.hits(), 2);

    // Once the interval has passed, one more refresh is allowed.
    tokio::time::sleep(interval).await;
    let forged = sign(&rsa("bogus-late"), "mallory");
    assert!(jwt.verify::<Claims>(&forged).await.is_err());
    assert_eq!(idp.hits(), 3);
    let mut kids = jwks.key_ids();
    kids.sort();
    assert_eq!(kids, ["k1", "k2"]);
}

#[tokio::test]
async fn concurrent_unknown_kids_share_one_fetch() {
    let k1 = rsa("k1");
    let idp = Idp::start(set_of(&[&k1])).await;
    idp.delay_ms.store(100, Ordering::SeqCst);
    let jwt = JwtConfig::from_jwks(Jwks::new(idp.url()));
    let token = sign(&k1, "a");
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let (jwt, token) = (jwt.clone(), token.clone());
            tokio::spawn(async move { jwt.verify::<Claims>(&token).await })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap().unwrap().sub, "a");
    }
    assert_eq!(idp.hits(), 1);
}

#[tokio::test]
async fn stale_keys_keep_serving_while_the_endpoint_fails() {
    let k1 = rsa("k1");
    let idp = Idp::start(set_of(&[&k1])).await;
    let jwks = Jwks::new(idp.url())
        .ttl(Duration::from_millis(100))
        .min_refresh_interval(Duration::from_millis(50));
    let jwt = JwtConfig::from_jwks(jwks.clone());
    let token = sign(&k1, "a");
    jwt.verify::<Claims>(&token).await.unwrap();
    assert_eq!(idp.hits(), 1);

    idp.fail_with(500);
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Stale: answered from the cache, refreshed in the background.
    assert_eq!(jwt.verify::<Claims>(&token).await.unwrap().sub, "a");
    wait_for("background refresh", || idp.hits() == 2).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(jwt.verify::<Claims>(&token).await.unwrap().sub, "a");
    assert_eq!(jwks.key_ids(), ["k1"]);

    // A token for a key the cache can't have: 503, not a misleading 401.
    tokio::time::sleep(Duration::from_millis(60)).await;
    let k2 = ec("k2");
    let e = jwt.verify::<Claims>(&sign(&k2, "b")).await.unwrap_err();
    assert_eq!(err_code(&e), (503, "jwks_unavailable"));

    // The provider recovers (with a new key): the next refresh picks it up.
    idp.serve(set_of(&[&k2, &k1]));
    idp.fail_with(200);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        jwt.verify::<Claims>(&sign(&k2, "b")).await.unwrap().sub,
        "b"
    );

    // An empty or malformed set is a failure too: the cached keys stay.
    for bad in [json!({"keys": []}), json!({"nope": 1}), json!("x")] {
        idp.serve(bad);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(jwks.refresh().await.is_err());
        assert_eq!(jwt.verify::<Claims>(&token).await.unwrap().sub, "a");
    }
}

#[tokio::test]
async fn a_slow_endpoint_never_holds_a_request_past_the_timeout() {
    let k1 = rsa("k1");
    let idp = Idp::start(set_of(&[&k1])).await;
    idp.delay_ms.store(3_000, Ordering::SeqCst);
    let jwt = JwtConfig::from_jwks(
        Jwks::new(idp.url())
            .timeout(Duration::from_millis(200))
            .ttl(Duration::from_millis(100))
            .min_refresh_interval(Duration::ZERO),
    );
    let token = sign(&k1, "a");

    let start = Instant::now();
    let e = jwt.verify::<Claims>(&token).await.unwrap_err();
    assert_eq!(err_code(&e), (503, "jwks_unavailable"));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );

    // Fast again: keys arrive. Then slow again: stale keys answer without
    // waiting for the (background) refresh, which would take 200 ms each.
    idp.delay_ms.store(0, Ordering::SeqCst);
    jwt.verify::<Claims>(&token).await.unwrap();
    idp.delay_ms.store(3_000, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let start = Instant::now();
    for _ in 0..5 {
        jwt.verify::<Claims>(&token).await.unwrap();
    }
    assert!(
        start.elapsed() < Duration::from_millis(600),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn algorithm_confusion_and_unusable_keys_are_rejected() {
    let r = rsa("rsa");
    let mut set = set_of(&[&r]);
    let rsa_jwk = set["keys"][0].clone();
    let keys = set["keys"].as_array_mut().unwrap();
    // A symmetric key: never accepted from a remote set.
    keys.push(json!({"kty": "oct", "kid": "sym", "alg": "HS256", "k": B64.encode(SECRET)}));
    // An RSA key that claims to be for HMAC, and an encryption key.
    let mut hs_rsa = rsa_jwk.clone();
    hs_rsa["kid"] = "rsa-hs".into();
    hs_rsa["alg"] = "HS256".into();
    keys.push(hs_rsa);
    let mut enc = rsa_jwk.clone();
    enc["kid"] = "rsa-enc".into();
    enc["use"] = "enc".into();
    keys.push(enc);
    // Unknown key types don't spoil the set.
    keys.push(json!({"kty": "XYZ", "kid": "odd"}));

    let idp = Idp::start(set).await;
    let jwks = Jwks::new(idp.url());
    let jwt = JwtConfig::from_jwks(jwks.clone());
    jwt.verify::<Claims>(&sign(&r, "ok")).await.unwrap();
    assert_eq!(jwks.key_ids(), ["rsa"]);

    let payload = json!({"sub": "mallory", "exp": now() + 600});
    let forge = |alg: Algorithm, kid: &str, key: &EncodingKey| {
        let mut h = Header::new(alg);
        h.kid = Some(kid.into());
        encode(&h, &payload, key).unwrap()
    };
    let rsa_pem_as_secret = EncodingKey::from_secret(RSA_PUBLIC.as_bytes());
    let n_as_secret = EncodingKey::from_secret(rsa_jwk["n"].as_str().unwrap().as_bytes());
    let secret = EncodingKey::from_secret(SECRET.as_bytes());
    let attacks = [
        // HS256 "signed" with the RSA public key, naming the RSA key.
        forge(Algorithm::HS256, "rsa", &rsa_pem_as_secret),
        forge(Algorithm::HS256, "rsa", &n_as_secret),
        forge(Algorithm::HS256, "rsa-hs", &rsa_pem_as_secret),
        // HS256 with the symmetric key the set (wrongly) published.
        forge(Algorithm::HS256, "sym", &secret),
        // A real signature from another algorithm's key, naming the RSA key.
        {
            let other = ec("rsa");
            sign(&other, "mallory")
        },
        // PS256 with the right key but the wrong algorithm for it.
        forge(
            Algorithm::PS256,
            "rsa",
            &EncodingKey::from_rsa_pem(RSA_PRIVATE.as_bytes()).unwrap(),
        ),
        format!(
            "{}.{}.",
            B64.encode(br#"{"alg":"none","kid":"rsa"}"#),
            B64.encode(payload.to_string())
        ),
    ];
    for (i, token) in attacks.iter().enumerate() {
        let e = jwt.verify::<Claims>(token).await.unwrap_err();
        assert_eq!(err_code(&e), (401, "invalid_token"), "attack {i}");
    }

    // An allowlist narrows what the set may be used for.
    let es_only = JwtConfig::from_jwks(Jwks::new(idp.url()).algorithms([Algorithm::ES256]));
    let e = es_only.verify::<Claims>(&sign(&r, "x")).await.unwrap_err();
    assert_eq!(e.status().as_u16(), 503, "no usable keys: {e:?}");
}

#[tokio::test]
async fn discovery_and_tokens_without_kid() {
    let r = rsa("only");
    let idp = Idp::start(set_of(&[&r])).await;
    let jwt = JwtConfig::from_jwks(Jwks::discover(idp.issuer())).issuer(idp.issuer());

    // A kid-less token is fine when the set has a single key.
    let signer = JwtConfig::rs256_pem(RSA_PUBLIC, Some(RSA_PRIVATE)).issuer(idp.issuer());
    let token = signer.encode(&Claims::new("ada")).unwrap();
    assert_eq!(jwt.verify::<Claims>(&token).await.unwrap().sub, "ada");
    assert_eq!(idp.hits(), 1);

    // ...but ambiguous with two.
    idp.serve(set_of(&[&r, &ec("two")]));
    let jwks = Jwks::discover(idp.issuer());
    let two = JwtConfig::from_jwks(jwks.clone()).issuer(idp.issuer());
    assert_eq!(jwks.refresh().await.unwrap(), 2);
    let e = two.verify::<Claims>(&token).await.unwrap_err();
    assert_eq!(err_code(&e), (401, "invalid_token"));

    // The discovery document must name the configured issuer.
    let wrong = Jwks::discover(format!("http://{}/", idp.addr));
    assert!(wrong.refresh().await.is_err());
}

#[tokio::test]
async fn local_keys_and_a_remote_set_together() {
    let r = rsa("idp-key");
    let idp = Idp::start(set_of(&[&r])).await;
    // Own HS256 tokens, plus the IdP's RS256 ones.
    let jwt = JwtConfig::hs256(SECRET)
        .key_id("ours")
        .jwks(Jwks::new(idp.url()));
    let ours = jwt.encode(&Claims::new("local")).unwrap();
    assert_eq!(jwt.verify::<Claims>(&ours).await.unwrap().sub, "local");
    assert_eq!(idp.hits(), 0, "local kids never hit the network");
    assert_eq!(
        jwt.verify::<Claims>(&sign(&r, "remote")).await.unwrap().sub,
        "remote"
    );
    assert_eq!(idp.hits(), 1);

    // An HS256 token naming the remote RSA key, signed with our secret.
    let mut h = Header::new(Algorithm::HS256);
    h.kid = Some("idp-key".into());
    let confused = encode(
        &h,
        &json!({"sub": "x", "exp": now() + 60}),
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap();
    let e = jwt.verify::<Claims>(&confused).await.unwrap_err();
    assert_eq!(err_code(&e), (401, "invalid_token"));
}

#[tokio::test]
async fn extractors_use_the_remote_set_and_startup_warms_it() {
    let r = rsa("k1");
    let idp = Idp::start(set_of(&[&r])).await;
    let client = App::new()
        .jwt(JwtConfig::from_jwks(Jwks::new(idp.url())).audience("api"))
        .get("/me", |Jwt(c): Jwt| async move { c.sub })
        .test();
    client.start().await;
    assert_eq!(idp.hits(), 1, "fetched at startup");

    let token = JwtConfig::from_key(r.clone())
        .audience("api")
        .encode(&Claims::new("ada"))
        .unwrap();
    let res = client.get("/me").bearer(&token).send().await;
    res.assert_status(200);
    assert_eq!(res.text(), "ada");

    let wrong_aud = sign(&r, "ada");
    client
        .get("/me")
        .bearer(&wrong_aud)
        .send()
        .await
        .assert_status(401);
    assert_eq!(idp.hits(), 1);
}

#[tokio::test]
async fn one_app_rotates_and_another_follows_its_jwks() {
    // The issuing service publishes its keys and rotates on demand.
    let issuer_cfg = JwtConfig::from_key(ec("2026-09"));
    let issuer = App::new()
        .jwt(issuer_cfg.clone())
        .serve_jwks("/.well-known/jwks.json")
        .post("/token", |State(jwt): State<JwtConfig>| async move {
            jwt.encode(&Claims::new("ada"))
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(issuer.serve(listener));

    // The verifying service follows it.
    let verifier = App::new()
        .jwt(JwtConfig::from_jwks(
            Jwks::new(format!("http://{addr}/.well-known/jwks.json"))
                .min_refresh_interval(Duration::ZERO),
        ))
        .get("/me", |Jwt(c): Jwt| async move { c.sub })
        .test();

    let old = issuer_cfg.encode(&Claims::new("ada")).unwrap();
    verifier
        .get("/me")
        .bearer(&old)
        .send()
        .await
        .assert_status(200);

    issuer_cfg.rotate(ed("2026-10")).unwrap();
    let new = issuer_cfg.encode(&Claims::new("ada")).unwrap();
    assert_eq!(
        jsonwebtoken::decode_header(&new).unwrap().alg,
        Algorithm::EdDSA
    );
    for t in [&new, &old] {
        let res = verifier.get("/me").bearer(t).send().await;
        res.assert_status(200);
        assert_eq!(res.text(), "ada");
    }

    // Retired upstream: once refreshed, the old key is gone downstream too.
    assert!(issuer_cfg.retire("2026-09"));
    let probe = sign(&rsa("probe"), "x"); // an unknown kid forces a refresh
    verifier
        .get("/me")
        .bearer(&probe)
        .send()
        .await
        .assert_status(401);
    verifier
        .get("/me")
        .bearer(&old)
        .send()
        .await
        .assert_status(401);
}
