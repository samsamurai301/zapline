//! JWT: config, encode/decode, validation failures, RS256/ES256, the `Jwt`
//! extractor and `Authenticate::jwt`.
#![cfg(feature = "jwt")]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD as B64};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zapline::auth::{Authenticate, Claims, CurrentUser, Jwt, JwtAuthenticator, JwtConfig, JwtKey};
use zapline::prelude::*;
use zapline::test::TestResponse;

const SECRET: &str = "test-secret-that-is-at-least-32-bytes-long";

// Test-only RSA keys (2048 bit), generated for this test suite.
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
const OTHER_RSA_PUBLIC: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA2ogU8zxfmQhy22oEDXin
A0Jxk3Yv3PI9w5IcHG4jEv96Rjd2Jj+nQDFeR+YkyNEsmE7qih049249ieFanjTP
Ev5rQJml1ZL+efV31lI7+j1Sodo17vePjb7B7u7mFwJHMBP5IkNp+dIMBLk9mklB
az/La7wOb/t7IkfNWRtYLdu5s2n1wSfRnJSoiimHzAxOdGLpCJ1KtvXngZCSO4bQ
VkcMrE5ULiWObKVT3ZHDchNsTnxbhQTJiRYM3RunWjtHkJqL1xxPS3vODC28BlZ1
53Md0gDDG7wWaBUbvRlVPJTnNqPUIMFb7eBEubactVhnG8i/192k02GthvXqhhJH
KQIDAQAB
-----END PUBLIC KEY-----";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn hs() -> JwtConfig {
    JwtConfig::hs256(SECRET)
}

fn code(r: &TestResponse) -> String {
    r.json::<Value>()["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn err_code(e: &Error) -> (u16, &str) {
    (e.status().as_u16(), e.code().unwrap_or(""))
}

/// Split a JWT and decode its JSON parts.
fn parts(token: &str) -> (Value, Value) {
    let mut it = token.split('.');
    let h = B64.decode(it.next().unwrap()).unwrap();
    let p = B64.decode(it.next().unwrap()).unwrap();
    (
        serde_json::from_slice(&h).unwrap(),
        serde_json::from_slice(&p).unwrap(),
    )
}

/// A token with an arbitrary payload, signed with HS256 and `key`, bypassing
/// `JwtConfig` (no claims are filled in, no secret length check).
fn forge_hs256(payload: &Value, key: &[u8]) -> String {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    encode(
        &Header::new(Algorithm::HS256),
        payload,
        &EncodingKey::from_secret(key),
    )
    .unwrap()
}

#[test]
fn hs256_round_trip_fills_registered_claims() {
    let cfg = hs()
        .issuer("https://auth.test")
        .audience("api")
        .ttl(Duration::from_secs(600))
        .key_id("k1");
    let claims = Claims::new("user-1")
        .roles(["admin", "user"])
        .scopes(["a:read"])
        .claim("tenant", "acme")
        .claim("n", 7);
    let token = cfg.encode(&claims).unwrap();

    let (header, payload) = parts(&token);
    assert_eq!(header["alg"], "HS256");
    assert_eq!(header["kid"], "k1");
    assert_eq!(payload["iss"], "https://auth.test");
    assert_eq!(payload["aud"], "api");
    let exp = payload["exp"].as_u64().unwrap();
    let iat = payload["iat"].as_u64().unwrap();
    assert!(iat.abs_diff(now()) <= 2);
    assert_eq!(exp - iat, 600);

    let back: Claims = cfg.decode(&token).unwrap();
    assert_eq!(back.sub, "user-1");
    assert_eq!(back.roles, ["admin", "user"]);
    assert_eq!(back.scopes, ["a:read"]);
    assert_eq!(back.aud, ["api"]);
    assert_eq!(back.iss.as_deref(), Some("https://auth.test"));
    assert_eq!(back.get::<String>("tenant").as_deref(), Some("acme"));
    assert_eq!(back.get::<u32>("n"), Some(7));
    assert!(back.has_role("admin") && !back.has_role("root"));
    assert!(back.has_scope("a:read"));
}

#[test]
fn explicit_claims_are_kept() {
    let cfg = hs().issuer("me");
    let token = cfg
        .encode(&Claims::new("u").expires_at(now() + 5).issuer("me"))
        .unwrap();
    let (_, p) = parts(&token);
    assert_eq!(p["exp"].as_u64().unwrap(), now() + 5);
    // A custom serializable type works too.
    #[derive(Serialize)]
    struct Mine {
        sub: &'static str,
        org: u32,
    }
    let token = cfg.encode(&Mine { sub: "x", org: 3 }).unwrap();
    let back: Value = cfg.decode(&token).unwrap();
    assert_eq!(back["org"], 3);
    assert_eq!(back["iss"], "me");
    // Claims must be a JSON object.
    let e = cfg.encode(&42).unwrap_err();
    assert_eq!(e.status(), 500);
}

#[test]
fn expired_tokens() {
    let cfg = hs();
    let token = cfg
        .encode(&Claims::new("u").expires_at(now() - 3600))
        .unwrap();
    let e = cfg.decode::<Claims>(&token).unwrap_err();
    assert_eq!(err_code(&e), (401, "token_expired"));
    let r = e.into_response();
    let challenge = r.headers()["www-authenticate"].to_str().unwrap();
    assert!(
        challenge.starts_with("Bearer error=\"invalid_token\""),
        "{challenge}"
    );

    // Within the default 60 s leeway...
    let recent = cfg
        .encode(&Claims::new("u").expires_at(now() - 20))
        .unwrap();
    assert!(cfg.decode::<Claims>(&recent).is_ok());
    // ...but not with zero leeway.
    let strict = hs().leeway(Duration::ZERO);
    let e = strict.decode::<Claims>(&recent).unwrap_err();
    assert_eq!(err_code(&e), (401, "token_expired"));
}

#[test]
fn issuer_and_audience_are_enforced() {
    let issuer = hs().issuer("good");
    let wrong = hs().issuer("evil").encode(&Claims::new("u")).unwrap();
    let e = issuer.decode::<Claims>(&wrong).unwrap_err();
    assert_eq!(err_code(&e), (401, "invalid_token"));
    // An expected issuer makes `iss` mandatory.
    let none = hs().encode(&Claims::new("u")).unwrap();
    assert_eq!(
        err_code(&issuer.decode::<Claims>(&none).unwrap_err()),
        (401, "invalid_token")
    );
    assert!(issuer
        .decode::<Claims>(&issuer.encode(&Claims::new("u")).unwrap())
        .is_ok());

    let aud = hs().audience("api").audience("admin-api");
    let other = hs().audience("billing").encode(&Claims::new("u")).unwrap();
    assert_eq!(
        err_code(&aud.decode::<Claims>(&other).unwrap_err()),
        (401, "invalid_token")
    );
    assert_eq!(
        err_code(&aud.decode::<Claims>(&none).unwrap_err()),
        (401, "invalid_token")
    );
    let second = hs()
        .audience("admin-api")
        .encode(&Claims::new("u"))
        .unwrap();
    assert!(aud.decode::<Claims>(&second).is_ok());
    // A token with an audience is rejected by a config that expects none.
    assert!(hs().decode::<Claims>(&other).is_err());
    // Arrays of audiences are accepted when one matches.
    let multi = hs()
        .encode(&Claims::new("u").audience("x").audience("api"))
        .unwrap();
    let back: Claims = aud.decode(&multi).unwrap();
    assert_eq!(back.aud, ["x", "api"]);
}

#[test]
fn bad_signatures_and_tampering() {
    let cfg = hs();
    let token = cfg.encode(&Claims::new("u").roles(["user"])).unwrap();

    let other = JwtConfig::hs256("another-secret-that-is-32-bytes-long!!");
    assert_eq!(
        err_code(&other.decode::<Claims>(&token).unwrap_err()),
        (401, "invalid_token")
    );

    // Privilege escalation attempt: swap the payload, keep the signature.
    let (_, mut p) = parts(&token);
    p["roles"] = json!(["admin"]);
    let mut it = token.split('.');
    let forged = format!(
        "{}.{}.{}",
        it.next().unwrap(),
        B64.encode(serde_json::to_vec(&p).unwrap()),
        it.nth(1).unwrap()
    );
    assert_eq!(
        err_code(&cfg.decode::<Claims>(&forged).unwrap_err()),
        (401, "invalid_token")
    );

    for junk in ["", "abc", "a.b.c", "a.b", "....", &"x".repeat(20_000)] {
        assert_eq!(
            err_code(&cfg.decode::<Claims>(junk).unwrap_err()),
            (401, "invalid_token"),
            "{junk}"
        );
    }
}

#[test]
fn algorithm_confusion_and_none_are_rejected() {
    let rs = JwtConfig::rs256_pem(RSA_PUBLIC, None);
    let payload = json!({"sub": "attacker", "exp": now() + 600});
    // HS256 token "signed" with the RSA public key as the HMAC secret.
    let confused = forge_hs256(&payload, RSA_PUBLIC.as_bytes());
    assert_eq!(
        err_code(&rs.decode::<Claims>(&confused).unwrap_err()),
        (401, "invalid_token")
    );

    let none = format!(
        "{}.{}.",
        B64.encode(br#"{"alg":"none","typ":"JWT"}"#),
        B64.encode(serde_json::to_vec(&payload).unwrap())
    );
    assert_eq!(
        err_code(&rs.decode::<Claims>(&none).unwrap_err()),
        (401, "invalid_token")
    );
    assert_eq!(
        err_code(&hs().decode::<Claims>(&none).unwrap_err()),
        (401, "invalid_token")
    );
}

#[test]
fn nbf_and_required_claims() {
    let cfg = hs().leeway(Duration::ZERO);
    let future = cfg
        .encode(&Claims {
            nbf: Some(now() + 3600),
            ..Claims::new("u")
        })
        .unwrap();
    assert_eq!(
        err_code(&cfg.decode::<Claims>(&future).unwrap_err()),
        (401, "invalid_token")
    );

    let strict = hs().required_claims(["exp", "nbf"]);
    let no_nbf = strict.encode(&Claims::new("u")).unwrap();
    assert!(strict.decode::<Claims>(&no_nbf).is_err());
    let with_nbf = strict
        .encode(&Claims {
            nbf: Some(now() - 10),
            ..Claims::new("u")
        })
        .unwrap();
    assert!(strict.decode::<Claims>(&with_nbf).is_ok());

    // `exp` is required by default; the requirement can be lifted.
    let no_exp = forge_hs256(&json!({"sub": "u"}), SECRET.as_bytes());
    assert_eq!(
        err_code(&hs().decode::<Claims>(&no_exp).unwrap_err()),
        (401, "invalid_token")
    );
    let lax = hs().required_claims(Vec::<String>::new());
    assert_eq!(lax.decode::<Claims>(&no_exp).unwrap().sub, "u");
    // A present `exp` is still validated.
    let old = forge_hs256(&json!({"sub": "u", "exp": now() - 3600}), SECRET.as_bytes());
    assert_eq!(
        err_code(&lax.decode::<Claims>(&old).unwrap_err()),
        (401, "token_expired")
    );
}

#[test]
fn rs256_with_pem_keys() {
    let signer = JwtConfig::rs256_pem(RSA_PUBLIC, Some(RSA_PRIVATE)).issuer("idp");
    let token = signer.encode(&Claims::new("u").roles(["admin"])).unwrap();
    assert_eq!(parts(&token).0["alg"], "RS256");

    // Verification needs only the public key.
    let verifier = JwtConfig::rs256_pem(RSA_PUBLIC, None).issuer("idp");
    let back: Claims = verifier.decode(&token).unwrap();
    assert_eq!(back.roles, ["admin"]);
    let e = verifier.encode(&Claims::new("u")).unwrap_err();
    assert_eq!(e.status(), 500);

    let wrong_key = JwtConfig::rs256_pem(OTHER_RSA_PUBLIC, None).issuer("idp");
    assert_eq!(
        err_code(&wrong_key.decode::<Claims>(&token).unwrap_err()),
        (401, "invalid_token")
    );

    // An HS256 config never accepts RS256 tokens.
    assert!(hs().issuer("idp").decode::<Claims>(&token).is_err());

    assert!(JwtConfig::try_rs256_pem("not a pem", None).is_err());
    assert!(JwtConfig::try_rs256_pem(RSA_PUBLIC, Some("garbage")).is_err());
}

#[test]
fn es256_with_generated_keys() {
    let kp = rcgen::KeyPair::generate().unwrap();
    let (private, public) = (kp.serialize_pem(), kp.public_key_pem());
    let signer = JwtConfig::es256_pem(public.as_str(), Some(private.as_str()));
    let token = signer.encode(&Claims::new("ec-user")).unwrap();
    assert_eq!(parts(&token).0["alg"], "ES256");
    let verifier = JwtConfig::es256_pem(public.as_str(), None);
    assert_eq!(verifier.decode::<Claims>(&token).unwrap().sub, "ec-user");

    let other = rcgen::KeyPair::generate().unwrap().public_key_pem();
    let e = JwtConfig::es256_pem(other.as_str(), None)
        .decode::<Claims>(&token)
        .unwrap_err();
    assert_eq!(err_code(&e), (401, "invalid_token"));
    assert!(JwtConfig::try_es256_pem(RSA_PUBLIC, None).is_err());
}

#[test]
#[should_panic(expected = "at least 32 bytes")]
fn short_hmac_secrets_are_refused() {
    let _ = JwtConfig::hs256("secret");
}

#[test]
fn claims_serde_accepts_common_shapes() {
    let c: Claims = serde_json::from_value(json!({
        "sub": "u", "aud": "one", "roles": "admin", "scope": "read write", "custom": {"a": 1}
    }))
    .unwrap();
    assert_eq!(c.aud, ["one"]);
    assert_eq!(c.roles, ["admin"]);
    assert_eq!(c.scopes, ["read", "write"]);
    assert_eq!(c.extra["custom"], json!({"a": 1}));

    let c: Claims = serde_json::from_value(json!({"sub": "u", "scp": ["a", "b"]})).unwrap();
    assert_eq!(c.scopes, ["a", "b"]);

    // Empty fields are omitted; a single audience is a string.
    let v = serde_json::to_value(Claims::new("u").audience("x")).unwrap();
    assert_eq!(v, json!({"sub": "u", "aud": "x"}));
    // `sub` is required.
    assert!(serde_json::from_value::<Claims>(json!({"exp": 1})).is_err());
}

// ---------------------------------------------------------------------------
// HTTP

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrgClaims {
    sub: String,
    org: String,
}

fn app() -> App {
    let api = Routes::new()
        .get(
            "/me",
            |CurrentUser(c): CurrentUser<Claims>| async move { c.sub },
        )
        .get("/admin", || async { "admin area" })
        .require_roles(["admin"])
        .post("/token", |State(jwt): State<JwtConfig>| async move {
            jwt.encode(&Claims::new("new-user").roles(["user"]))
        })
        .allow_anonymous();
    App::new()
        .jwt(hs().issuer("test"))
        .nest("/api", api.wrap(Authenticate::jwt()))
        .get("/claims", |Jwt(c): Jwt| async move { c.sub })
        .get("/org", |Jwt(c): Jwt<OrgClaims>| async move { c.org })
        .get("/maybe", |c: Option<Jwt>| async move {
            c.map_or("anonymous".to_owned(), |c| c.sub.clone())
        })
}

fn token(roles: &[&str]) -> String {
    hs().issuer("test")
        .encode(&Claims::new("ada").roles(roles.iter().copied()))
        .unwrap()
}

#[tokio::test]
async fn jwt_extractor() {
    let c = app().test();
    assert_eq!(
        c.get("/claims").bearer(&token(&[])).send().await.text(),
        "ada"
    );

    let r = c.get("/claims").send().await;
    assert_eq!(
        (r.status().as_u16(), code(&r)),
        (401, "missing_credentials".into())
    );
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));

    let r = c.get("/claims").bearer("not-a-jwt").send().await;
    assert_eq!(
        (r.status().as_u16(), code(&r)),
        (401, "invalid_token".into())
    );
    assert_eq!(
        r.header("www-authenticate"),
        Some("Bearer error=\"invalid_token\"")
    );

    let expired = hs()
        .issuer("test")
        .encode(&Claims::new("ada").expires_at(now() - 3600))
        .unwrap();
    let r = c.get("/claims").bearer(&expired).send().await;
    assert_eq!(
        (r.status().as_u16(), code(&r)),
        (401, "token_expired".into())
    );

    // Custom claim types.
    let org = hs()
        .issuer("test")
        .encode(&json!({"sub": "ada", "org": "acme"}))
        .unwrap();
    assert_eq!(c.get("/org").bearer(&org).send().await.text(), "acme");
    // Valid token, wrong shape for the claims type.
    let r = c.get("/org").bearer(&token(&[])).send().await;
    assert_eq!(code(&r), "invalid_token");

    assert_eq!(c.get("/maybe").send().await.text(), "anonymous");
    assert_eq!(
        c.get("/maybe").bearer(&token(&[])).send().await.text(),
        "ada"
    );
}

#[tokio::test]
async fn jwt_without_config_is_a_server_error() {
    let c = App::new()
        .get("/", |Jwt(c): Jwt| async move { c.sub })
        .nest(
            "/api",
            Routes::new()
                .get("/", || async { "x" })
                .wrap(Authenticate::jwt()),
        )
        .test();
    for path in ["/", "/api"] {
        let r = c.get(path).bearer(&token(&[])).send().await;
        assert_eq!(
            (r.status().as_u16(), code(&r)),
            (500, "auth_misconfigured".into()),
            "{path}"
        );
    }
}

#[tokio::test]
async fn authenticate_jwt_one_liner() {
    let c = app().test();
    let user = token(&["user"]);
    let admin = token(&["admin"]);

    let r = c.get("/api/me").send().await;
    assert_eq!(
        (r.status().as_u16(), code(&r)),
        (401, "missing_credentials".into())
    );
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    assert_eq!(c.get("/api/me").bearer(&user).send().await.text(), "ada");

    let r = c.get("/api/admin").bearer(&user).send().await;
    assert_eq!((r.status().as_u16(), code(&r)), (403, "forbidden".into()));
    assert_eq!(
        c.get("/api/admin").bearer(&admin).send().await.text(),
        "admin area"
    );

    // Expired and forged tokens.
    let expired = hs()
        .issuer("test")
        .encode(&Claims::new("ada").expires_at(now() - 3600))
        .unwrap();
    let r = c.get("/api/me").bearer(&expired).send().await;
    assert_eq!(code(&r), "token_expired");
    let foreign = JwtConfig::hs256("some-other-secret-with-32-bytes-or-more")
        .issuer("test")
        .encode(&Claims::new("ada").roles(["admin"]))
        .unwrap();
    let r = c.get("/api/admin").bearer(&foreign).send().await;
    assert_eq!(
        (r.status().as_u16(), code(&r)),
        (401, "invalid_token".into())
    );

    // Anonymous route issuing a token through `State<JwtConfig>`.
    let r = c.post("/api/token").send().await;
    assert_eq!(r.status(), 200);
    let issued = r.text();
    assert_eq!(
        c.get("/api/me").bearer(&issued).send().await.text(),
        "new-user"
    );
}

#[tokio::test]
async fn optional_jwt_authentication_and_custom_claims() {
    #[derive(Clone, Deserialize)]
    struct Mine {
        sub: String,
    }
    let api = Routes::new()
        .get("/", |u: Option<CurrentUser<Mine>>| async move {
            u.map_or("anon".to_owned(), |u| u.0.sub)
        })
        .wrap(Authenticate::new(JwtAuthenticator::<Mine>::with_config(
            hs(),
        )));
    let open = Routes::new()
        .get("/", |u: Option<CurrentUser<Claims>>| async move {
            u.map_or("anon".to_owned(), |u| u.0.sub)
        })
        .wrap(Authenticate::jwt_with(hs()).optional());
    let c = App::new().nest("/custom", api).nest("/open", open).test();
    let t = hs().encode(&Claims::new("ada")).unwrap();
    assert_eq!(c.get("/custom").send().await.text(), "anon");
    assert_eq!(c.get("/custom").bearer(&t).send().await.text(), "ada");
    assert_eq!(c.get("/open").send().await.text(), "anon");
    assert_eq!(c.get("/open").bearer(&t).send().await.text(), "ada");
    assert_eq!(c.get("/open").bearer("junk").send().await.status(), 401);
}

// ---------------------------------------------------------------------------
// Key rings and rotation

const OTHER_SECRET: &str = "another-test-secret-that-is-at-least-32-bytes";

/// A token signed with HS256 and `key`, with header `kid`.
fn forge_hs256_kid(payload: &Value, key: &[u8], kid: Option<&str>) -> String {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    let mut header = Header::new(Algorithm::HS256);
    header.kid = kid.map(str::to_owned);
    encode(&header, payload, &EncodingKey::from_secret(key)).unwrap()
}

fn ec_key(kid: &str) -> JwtKey {
    let kp = rcgen::KeyPair::generate().unwrap();
    let (private, public) = (kp.serialize_pem(), kp.public_key_pem());
    JwtKey::es256_pem(kid, public.as_str(), Some(private.as_str())).unwrap()
}

#[test]
fn rotation_keeps_old_tokens_until_the_key_is_retired() {
    let jwt = hs().key_id("2026-09");
    let old = jwt.encode(&Claims::new("old")).unwrap();

    jwt.rotate(JwtKey::hs256("2026-10", OTHER_SECRET).unwrap())
        .unwrap();
    let new = jwt.encode(&Claims::new("new")).unwrap();
    assert_eq!(parts(&new).0["kid"], "2026-10");
    assert_eq!(jwt.key_ids(), ["2026-10", "2026-09"]);

    assert_eq!(jwt.decode::<Claims>(&old).unwrap().sub, "old");
    assert_eq!(jwt.decode::<Claims>(&new).unwrap().sub, "new");

    // The signing key can't be retired; the old one can.
    assert!(!jwt.retire("2026-10"));
    assert!(jwt.retire("2026-09"));
    assert!(!jwt.retire("2026-09"));
    assert_eq!(
        err_code(&jwt.decode::<Claims>(&old).unwrap_err()),
        (401, "invalid_token")
    );
    assert_eq!(jwt.decode::<Claims>(&new).unwrap().sub, "new");
}

#[test]
fn rotation_across_algorithms() {
    let jwt = hs().key_id("hs");
    let hs_token = jwt.encode(&Claims::new("a")).unwrap();
    jwt.rotate(ec_key("ec")).unwrap();
    assert_eq!(jwt.algorithm(), jsonwebtoken::Algorithm::ES256);
    let ec_token = jwt.encode(&Claims::new("b")).unwrap();
    assert_eq!(parts(&ec_token).0["alg"], "ES256");
    assert_eq!(jwt.decode::<Claims>(&hs_token).unwrap().sub, "a");
    assert_eq!(jwt.decode::<Claims>(&ec_token).unwrap().sub, "b");

    // A rotation to EdDSA, too.
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let ed = JwtKey::ed25519_pem(
        "ed",
        kp.public_key_pem().as_str(),
        Some(kp.serialize_pem().as_str()),
    )
    .unwrap();
    jwt.rotate(ed).unwrap();
    let ed_token = jwt.encode(&Claims::new("c")).unwrap();
    assert_eq!(parts(&ed_token).0["alg"], "EdDSA");
    for (t, sub) in [(&hs_token, "a"), (&ec_token, "b"), (&ed_token, "c")] {
        assert_eq!(jwt.decode::<Claims>(t).unwrap().sub, sub);
    }
}

#[test]
fn rotate_needs_a_named_signing_key() {
    let jwt = hs();
    let verify_only = JwtKey::new(
        "pub",
        jsonwebtoken::Algorithm::HS256,
        jsonwebtoken::DecodingKey::from_secret(OTHER_SECRET.as_bytes()),
        None,
    );
    assert!(jwt.rotate(verify_only).is_err());
    assert!(JwtKey::hs256("short", "too-short").is_err());
}

#[test]
fn keys_are_chosen_by_kid() {
    let jwt = hs().key_id("a");
    jwt.add_key(JwtKey::hs256("b", OTHER_SECRET).unwrap());
    let payload = json!({"sub": "x", "exp": now() + 600});

    let by_b = forge_hs256_kid(&payload, OTHER_SECRET.as_bytes(), Some("b"));
    assert_eq!(jwt.decode::<Claims>(&by_b).unwrap().sub, "x");
    // The right secret under the wrong kid fails: the kid picks the key.
    let mislabeled = forge_hs256_kid(&payload, SECRET.as_bytes(), Some("b"));
    assert_eq!(
        err_code(&jwt.decode::<Claims>(&mislabeled).unwrap_err()),
        (401, "invalid_token")
    );
    for kid in [Some("unknown"), None] {
        let t = forge_hs256_kid(&payload, SECRET.as_bytes(), kid);
        assert_eq!(
            err_code(&jwt.decode::<Claims>(&t).unwrap_err()),
            (401, "invalid_token"),
            "{kid:?}"
        );
    }
    // add_key keeps the signing key first.
    assert_eq!(jwt.key_ids(), ["a", "b"]);
}

#[test]
fn an_unnamed_key_accepts_any_kid() {
    // Configs without key ids keep working whatever `kid` a token carries.
    let payload = json!({"sub": "x", "exp": now() + 600});
    for kid in [None, Some("anything")] {
        let t = forge_hs256_kid(&payload, SECRET.as_bytes(), kid);
        assert_eq!(hs().decode::<Claims>(&t).unwrap().sub, "x");
    }
    // ...also next to named keys, when the algorithm fits.
    let jwt = hs();
    jwt.add_key(JwtKey::hs256("b", OTHER_SECRET).unwrap());
    let t = forge_hs256_kid(&payload, SECRET.as_bytes(), Some("legacy"));
    assert_eq!(jwt.decode::<Claims>(&t).unwrap().sub, "x");
}

#[test]
fn ring_algorithm_confusion_is_rejected() {
    // An RS256 key named "rsa": an HS256 token naming it, "signed" with the
    // public key as an HMAC secret, must fail.
    let jwt = hs().key_id("hs");
    jwt.add_key(JwtKey::rs256_pem("rsa", RSA_PUBLIC, None).unwrap());
    let payload = json!({"sub": "attacker", "exp": now() + 600});
    let confused = forge_hs256_kid(&payload, RSA_PUBLIC.as_bytes(), Some("rsa"));
    assert_eq!(
        err_code(&jwt.decode::<Claims>(&confused).unwrap_err()),
        (401, "invalid_token")
    );
    let none = format!(
        "{}.{}.",
        B64.encode(br#"{"alg":"none","kid":"rsa"}"#),
        B64.encode(serde_json::to_vec(&payload).unwrap())
    );
    assert_eq!(
        err_code(&jwt.decode::<Claims>(&none).unwrap_err()),
        (401, "invalid_token")
    );
    // The real RS256 key verifies its own tokens.
    let signer = JwtConfig::rs256_pem(RSA_PUBLIC, Some(RSA_PRIVATE)).key_id("rsa");
    let t = signer.encode(&Claims::new("ok")).unwrap();
    assert_eq!(jwt.decode::<Claims>(&t).unwrap().sub, "ok");
}

#[test]
fn key_id_does_not_leak_into_the_config_it_was_cloned_from() {
    let base = hs();
    let a = base.clone().key_id("a");
    assert!(base.key_ids().is_empty());
    assert_eq!(a.key_ids(), ["a"]);
    // Runtime changes are shared by clones.
    let shared = a.clone();
    a.add_key(JwtKey::hs256("b", OTHER_SECRET).unwrap());
    assert_eq!(shared.key_ids(), ["a", "b"]);
}

#[test]
fn public_jwks_has_only_public_asymmetric_keys() {
    let jwt = hs().key_id("hs");
    assert!(jwt.public_jwks().keys.is_empty());
    jwt.rotate(ec_key("ec")).unwrap();
    jwt.add_key(JwtKey::rs256_pem("rsa", RSA_PUBLIC, None).unwrap());
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    jwt.add_key(JwtKey::ed25519_pem("ed", kp.public_key_pem().as_str(), None).unwrap());

    let set = serde_json::to_value(jwt.public_jwks()).unwrap();
    let keys = set["keys"].as_array().unwrap();
    let mut summary: Vec<_> = keys
        .iter()
        .map(|k| {
            (
                k["kid"].as_str().unwrap(),
                k["kty"].as_str().unwrap(),
                k["alg"].as_str().unwrap(),
            )
        })
        .collect();
    summary.sort();
    assert_eq!(
        summary,
        [
            ("ec", "EC", "ES256"),
            ("ed", "OKP", "EdDSA"),
            ("rsa", "RSA", "RS256"),
        ]
    );
    for k in keys {
        assert_eq!(k["use"], "sig");
        assert!(k.get("d").is_none() && k.get("k").is_none(), "{k}");
    }

    // Each published JWK verifies tokens of its key.
    let signer = JwtConfig::rs256_pem(RSA_PUBLIC, Some(RSA_PRIVATE)).key_id("rsa");
    let t = signer.encode(&Claims::new("pub")).unwrap();
    let jwk = jwt.public_jwks().find("rsa").unwrap().clone();
    let verifier = JwtConfig::from_key(JwtKey::from_jwk(&jwk).unwrap());
    assert_eq!(verifier.decode::<Claims>(&t).unwrap().sub, "pub");
}

#[test]
fn from_jwk_refuses_symmetric_and_mismatched_keys() {
    use zapline::auth::Jwk;
    let jwk = |v: Value| serde_json::from_value::<Jwk>(v).unwrap();
    let oct = jwk(json!({"kty": "oct", "kid": "s", "k": B64.encode(SECRET)}));
    assert!(JwtKey::from_jwk(&oct).is_err());
    let rsa = serde_json::to_value(
        JwtConfig::rs256_pem(RSA_PUBLIC, None)
            .key_id("r")
            .public_jwks()
            .keys[0]
            .clone(),
    )
    .unwrap();
    let mut hs_rsa = rsa.clone();
    hs_rsa["alg"] = "HS256".into();
    assert!(JwtKey::from_jwk(&jwk(hs_rsa)).is_err());
    let mut ps = rsa.clone();
    ps["alg"] = "PS256".into();
    assert_eq!(
        JwtKey::from_jwk(&jwk(ps)).unwrap().algorithm(),
        jsonwebtoken::Algorithm::PS256
    );
    let mut enc = rsa;
    enc["use"] = "enc".into();
    assert!(JwtKey::from_jwk(&jwk(enc)).is_err());
}

#[tokio::test]
async fn rotation_through_state_and_the_jwks_route() {
    let app = App::new()
        .jwt(hs().key_id("k1"))
        .serve_jwks("/.well-known/jwks.json")
        .post("/rotate", |State(jwt): State<JwtConfig>| async move {
            jwt.rotate(ec_key("k2"))?;
            Ok::<_, Error>("rotated")
        })
        .post("/token", |State(jwt): State<JwtConfig>| async move {
            jwt.encode(&Claims::new("ada"))
        })
        .get("/me", |Jwt(c): Jwt| async move { c.sub });
    let client = app.test();

    let res = client.get("/.well-known/jwks.json").send().await;
    res.assert_status(200);
    assert_eq!(res.header("cache-control"), Some("public, max-age=300"));
    assert_eq!(res.json::<Value>(), json!({"keys": []}));

    let old = client.post("/token").send().await.text();
    client.post("/rotate").send().await.assert_status(200);
    let new = client.post("/token").send().await.text();
    assert_eq!(parts(&new).0["kid"], "k2");

    for t in [&old, &new] {
        let res = client.get("/me").bearer(t).send().await;
        res.assert_status(200);
        assert_eq!(res.text(), "ada");
    }
    let keys = client
        .get("/.well-known/jwks.json")
        .send()
        .await
        .json::<Value>();
    assert_eq!(keys["keys"][0]["kid"], "k2");
    assert_eq!(keys["keys"].as_array().unwrap().len(), 1);
}
