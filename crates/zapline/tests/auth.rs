//! Credential extractors, the `Authenticate` middleware, `CurrentUser`,
//! route policies and guards.
#![cfg(all(feature = "json", feature = "query"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;
use zapline::auth::{
    constant_time_eq, AllowAnonymous, ApiKey, ApiKeyConfig, Authenticate, Authenticator, BasicAuth,
    BasicAuthConfig, BearerToken, CurrentUser, Guard, HasRoles, RequireAuth, RequireRoles,
    RequireScopes, RoutePolicy,
};
use zapline::prelude::*;
use zapline::test::TestResponse;
use zapline::RouteInfo;

fn code(r: &TestResponse) -> String {
    r.json::<Value>()["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn basic(user: &str, pass: &str) -> String {
    use base64::prelude::{Engine as _, BASE64_STANDARD};
    format!("Basic {}", BASE64_STANDARD.encode(format!("{user}:{pass}")))
}

#[test]
fn constant_time_comparison() {
    assert!(constant_time_eq("abc", "abc"));
    assert!(constant_time_eq(b"", b""));
    assert!(!constant_time_eq("abc", "abd"));
    assert!(!constant_time_eq("abc", "abcd"));
    assert!(!constant_time_eq("", "a"));
}

// ---------------------------------------------------------------------------
// Bearer

#[tokio::test]
async fn bearer_token_extractor() {
    let c = App::new()
        .get("/", |t: BearerToken| async move { t.token().to_owned() })
        .get("/opt", |t: Option<BearerToken>| async move {
            t.map_or("anon".to_owned(), |t| t.as_str().to_owned())
        })
        .test();

    assert_eq!(
        c.get("/").bearer("abc.DEF-1_~+/=").send().await.text(),
        "abc.DEF-1_~+/="
    );
    // Scheme is case-insensitive; extra spaces are tolerated.
    let r = c
        .get("/")
        .header("authorization", "bEaReR   tok")
        .send()
        .await;
    assert_eq!(r.text(), "tok");

    let r = c.get("/").send().await;
    assert_eq!(r.status(), 401);
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    assert_eq!(code(&r), "missing_credentials");

    // Another scheme counts as missing bearer credentials.
    let r = c
        .get("/")
        .header("authorization", "Basic Zm9vOmJhcg==")
        .send()
        .await;
    assert_eq!(r.status(), 401);
    assert_eq!(code(&r), "missing_credentials");

    for bad in [
        "Bearer",
        "Bearer ",
        "Bearer a b",
        "Bearer a\"b",
        "Bearer ==",
    ] {
        let r = c.get("/").header("authorization", bad).send().await;
        assert_eq!(r.status(), 401, "{bad}");
        assert_eq!(code(&r), "invalid_credentials", "{bad}");
        assert!(r.header("www-authenticate").unwrap().starts_with("Bearer"));
    }

    // Two Authorization headers are ambiguous.
    let r = c
        .get("/")
        .header("authorization", "Bearer a")
        .header("authorization", "Bearer b")
        .send()
        .await;
    assert_eq!(code(&r), "invalid_credentials");

    // Oversized tokens are rejected.
    let huge = "a".repeat(20_000);
    assert_eq!(c.get("/").bearer(&huge).send().await.status(), 401);

    assert_eq!(c.get("/opt").send().await.text(), "anon");
    assert_eq!(c.get("/opt").bearer("t").send().await.text(), "t");
}

// ---------------------------------------------------------------------------
// Basic

#[tokio::test]
async fn basic_auth_extractor() {
    async fn h(auth: BasicAuth) -> Result<String> {
        if !auth.verify("ada", "pa:ss wörd") {
            return Err(auth.reject());
        }
        Ok(format!("{}|{}", auth.username, auth.password))
    }
    let c = App::new().get("/", h).test();

    let r = c
        .get("/")
        .header("authorization", basic("ada", "pa:ss wörd"))
        .send()
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.text(), "ada|pa:ss wörd");

    let r = c
        .get("/")
        .header("authorization", basic("ada", "nope"))
        .send()
        .await;
    assert_eq!(r.status(), 401);
    assert_eq!(code(&r), "invalid_credentials");

    let r = c.get("/").send().await;
    assert_eq!(r.status(), 401);
    assert_eq!(code(&r), "missing_credentials");
    assert_eq!(
        r.header("www-authenticate"),
        Some(r#"Basic realm="Restricted", charset="UTF-8""#)
    );

    for bad in [
        "Basic !!!notbase64",
        "Basic bm9jb2xvbg==", // "nocolon"
        "Basic //79",         // invalid UTF-8
        "Basic ",
    ] {
        let r = c.get("/").header("authorization", bad).send().await;
        assert_eq!(r.status(), 401, "{bad}");
        assert_eq!(code(&r), "invalid_credentials", "{bad}");
    }
    // Scheme is case-insensitive.
    let lower = basic("ada", "pa:ss wörd").replacen("Basic", "basic", 1);
    assert_eq!(
        c.get("/")
            .header("authorization", lower)
            .send()
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn basic_auth_realm_is_configurable() {
    let c = App::new()
        .state(BasicAuthConfig::new("ops area"))
        .get("/", |_: BasicAuth| async { "ok" })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(
        r.header("www-authenticate"),
        Some(r#"Basic realm="ops area", charset="UTF-8""#)
    );
}

#[test]
fn basic_debug_redacts_password() {
    let a = BasicAuth {
        username: "ada".into(),
        password: "secret".into(),
    };
    let s = format!("{a:?}");
    assert!(s.contains("ada") && !s.contains("secret"), "{s}");
    assert!(a.verify("ada", "secret"));
    assert!(!a.verify("ad", "secret"));
}

// ---------------------------------------------------------------------------
// API keys

#[tokio::test]
async fn api_key_from_default_header() {
    let c = App::new()
        .get("/", |k: ApiKey| async move {
            if k.verify("k-1") {
                Ok("ok")
            } else {
                Err(k.reject())
            }
        })
        .test();
    assert_eq!(
        c.get("/").header("x-api-key", "k-1").send().await.text(),
        "ok"
    );
    let r = c.get("/").header("x-api-key", "k-2").send().await;
    assert_eq!(
        (r.status().as_u16(), code(&r)),
        (401, "invalid_credentials".into())
    );
    let r = c.get("/").send().await;
    assert_eq!(
        (r.status().as_u16(), code(&r)),
        (401, "missing_credentials".into())
    );
    assert_eq!(
        r.header("www-authenticate"),
        Some(r#"ApiKey header="x-api-key""#)
    );
    // Query parameters are ignored unless configured.
    assert_eq!(c.get("/?api_key=k-1").send().await.status(), 401);
    // Empty or duplicated keys are invalid.
    let r = c.get("/").header("x-api-key", "  ").send().await;
    assert_eq!(code(&r), "invalid_credentials");
    let r = c
        .get("/")
        .header("x-api-key", "k-1")
        .header("x-api-key", "k-1")
        .send()
        .await;
    assert_eq!(code(&r), "invalid_credentials");
}

#[tokio::test]
async fn api_key_custom_header_and_query() {
    let c = App::new()
        .state(ApiKeyConfig::new().header("x-token").query_param("api_key"))
        .get("/", |k: ApiKey| async move { k.as_str().to_owned() })
        .test();
    assert_eq!(
        c.get("/").header("x-token", "abc").send().await.text(),
        "abc"
    );
    assert_eq!(c.get("/?x=1&api_key=a%20b+c").send().await.text(), "a b c");
    // The header wins over the query parameter.
    let r = c.get("/?api_key=q").header("x-token", "h").send().await;
    assert_eq!(r.text(), "h");
    // The default header is no longer read.
    let r = c.get("/").header("x-api-key", "abc").send().await;
    assert_eq!(r.status(), 401);
    assert_eq!(
        r.header("www-authenticate"),
        Some(r#"ApiKey header="x-token""#)
    );
}

#[tokio::test]
async fn api_key_verify_any_and_redacted_debug() {
    let c = App::new()
        .get("/", |k: ApiKey| async move {
            let any = k.verify_any(["x", "secret-key"]);
            let none = k.verify_any(["x", "y"]);
            format!("{any} {none} {k:?}")
        })
        .get("/bearer", |t: BearerToken| async move { format!("{t:?}") })
        .test();
    let r = c.get("/").header("x-api-key", "secret-key").send().await;
    assert_eq!(r.text(), "true false ApiKey(<redacted>)");
    let r = c.get("/bearer").bearer("secret").send().await;
    assert_eq!(r.text(), "BearerToken(<redacted>)");
}

// ---------------------------------------------------------------------------
// Authenticate + CurrentUser

#[derive(Clone, Debug)]
struct User {
    name: String,
    roles: Vec<String>,
    scopes: Vec<String>,
}

impl HasRoles for User {
    fn roles(&self) -> &[String] {
        &self.roles
    }
    fn scopes(&self) -> &[String] {
        &self.scopes
    }
}

/// `x-user: name;role,role;scope scope`; `x-user: bad` is an invalid credential.
struct HeaderAuth {
    calls: Arc<AtomicUsize>,
}

impl HeaderAuth {
    fn new() -> Self {
        HeaderAuth {
            calls: Arc::default(),
        }
    }
}

impl Authenticator for HeaderAuth {
    type User = User;

    async fn authenticate(&self, req: &Request) -> Result<Option<User>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let Some(v) = req.headers().get("x-user") else {
            return Ok(None);
        };
        let v = v.to_str().map_err(|_| Error::unauthorized())?;
        if v == "bad" {
            return Err(Error::unauthorized().with_code("invalid_credentials"));
        }
        let mut parts = v.split(';');
        let name = parts.next().unwrap_or_default().to_owned();
        let list = |s: Option<&str>, sep: char| -> Vec<String> {
            s.unwrap_or_default()
                .split(sep)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let roles = list(parts.next(), ',');
        let scopes = list(parts.next(), ' ');
        Ok(Some(User {
            name,
            roles,
            scopes,
        }))
    }

    fn challenge(&self) -> Option<HeaderValue> {
        Some(HeaderValue::from_static("X-User"))
    }
}

async fn whoami(CurrentUser(u): CurrentUser<User>) -> String {
    u.name
}

async fn maybe(u: Option<CurrentUser<User>>) -> String {
    u.map_or("anonymous".into(), |u| u.name.clone())
}

#[tokio::test]
async fn closure_authenticator_optional_and_required() {
    let auth = |req: &Request| {
        let name = req
            .headers()
            .get("x-name")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        async move { Ok(name) }
    };
    let optional = Routes::new()
        .get("/", |u: Option<CurrentUser<String>>| async move {
            u.map_or("anon".to_owned(), |u| u.0)
        })
        .wrap(Authenticate::new(auth));
    let required = Routes::new()
        .get("/", |CurrentUser(n): CurrentUser<String>| async move { n })
        .wrap(
            Authenticate::new(auth)
                .required()
                .challenge("Custom realm=\"x\""),
        );
    let c = App::new()
        .nest("/opt", optional)
        .nest("/req", required)
        .get("/plain", |u: Result<CurrentUser<String>>| async move {
            match u {
                Ok(_) => "user".to_owned(),
                Err(e) => format!("{} {}", e.status().as_u16(), e.code().unwrap_or("")),
            }
        })
        .test();

    assert_eq!(c.get("/opt").send().await.text(), "anon");
    assert_eq!(
        c.get("/opt").header("x-name", "ada").send().await.text(),
        "ada"
    );
    let r = c.get("/req").send().await;
    assert_eq!(r.status(), 401);
    assert_eq!(code(&r), "missing_credentials");
    assert_eq!(r.header("www-authenticate"), Some("Custom realm=\"x\""));
    assert_eq!(
        c.get("/req").header("x-name", "bob").send().await.text(),
        "bob"
    );
    // Without the middleware there is no user.
    assert_eq!(
        c.get("/plain").send().await.text(),
        "401 missing_credentials"
    );
}

#[tokio::test]
async fn invalid_credentials_are_rejected_even_when_optional() {
    let api = Routes::new()
        .get("/", maybe)
        .get("/login", maybe)
        .allow_anonymous()
        .wrap(Authenticate::new(HeaderAuth::new()));
    let c = App::new().nest("/api", api).test();

    assert_eq!(c.get("/api").send().await.text(), "anonymous");
    let r = c.get("/api").header("x-user", "bad").send().await;
    assert_eq!(r.status(), 401);
    assert_eq!(code(&r), "invalid_credentials");
    // `.allow_anonymous()` routes ignore broken credentials.
    let r = c.get("/api/login").header("x-user", "bad").send().await;
    assert_eq!(r.text(), "anonymous");
    let r = c.get("/api/login").header("x-user", "ada").send().await;
    assert_eq!(r.text(), "ada");
}

#[tokio::test]
async fn required_authentication_with_anonymous_exceptions() {
    let auth = HeaderAuth::new();
    let calls = auth.calls.clone();
    let api = Routes::new()
        .get("/me", whoami)
        .post("/login", maybe)
        .allow_anonymous()
        .wrap(Authenticate::new(auth).required());
    let c = App::new()
        .nest("/api", api)
        .get("/public", || async { "public" })
        .test();

    let r = c.get("/api/me").send().await;
    assert_eq!(r.status(), 401);
    assert_eq!(r.header("www-authenticate"), Some("X-User"));
    assert_eq!(
        c.get("/api/me").header("x-user", "ada").send().await.text(),
        "ada"
    );
    assert_eq!(c.post("/api/login").send().await.text(), "anonymous");
    // Routes outside the group never run the authenticator.
    let before = calls.load(Ordering::SeqCst);
    assert_eq!(c.get("/public").send().await.text(), "public");
    assert_eq!(calls.load(Ordering::SeqCst), before);
}

fn policy_app() -> TestClient {
    // /api            require_auth_all
    //   /me           (inherits auth)
    //   /admin/*      require_roles_all(["admin", "root"])
    //     /stats      (inherits roles)
    //     /audit      own roles ["auditor"] (overrides the group's)
    //     /write      + scopes ["data:write", "data:read"] (combined with group roles)
    //     /health     allow_anonymous
    let admin = Routes::new()
        .get("/stats", || async { "stats" })
        .get("/audit", || async { "audit" })
        .require_roles(["auditor"])
        .post("/write", || async { "written" })
        .require_scopes(["data:write"])
        .require_scopes(["data:read"])
        .get("/health", || async { "healthy" })
        .allow_anonymous()
        .require_roles_all(["admin", "root"]);
    let api = Routes::new()
        .get("/me", whoami)
        .nest("/admin", admin)
        .require_auth_all()
        .wrap(Authenticate::new(HeaderAuth::new()).authorize());
    App::new().nest("/api", api).get("/open", maybe).test()
}

use zapline::test::TestClient;

#[tokio::test]
async fn role_and_scope_policies_on_nested_groups() {
    let c = policy_app();
    let get = |path: &'static str, user: Option<&'static str>| {
        let c = c.clone();
        async move {
            let mut r = c.get(path);
            if let Some(u) = user {
                r = r.header("x-user", u);
            }
            let r = r.send().await;
            (r.status().as_u16(), code_or_text(&r))
        }
    };
    fn code_or_text(r: &TestResponse) -> String {
        if r.status().is_success() {
            r.text()
        } else {
            code(r)
        }
    }

    // Group-level auth.
    assert_eq!(
        get("/api/me", None).await,
        (401, "missing_credentials".into())
    );
    assert_eq!(get("/api/me", Some("ada")).await, (200, "ada".into()));
    // Group-level roles: any of admin/root.
    assert_eq!(
        get("/api/admin/stats", None).await,
        (401, "missing_credentials".into())
    );
    assert_eq!(
        get("/api/admin/stats", Some("ada;user")).await,
        (403, "forbidden".into())
    );
    assert_eq!(
        get("/api/admin/stats", Some("ada;root")).await,
        (200, "stats".into())
    );
    assert_eq!(
        get("/api/admin/stats", Some("ada;user,admin")).await,
        (200, "stats".into())
    );
    // Route-level roles replace the group's.
    assert_eq!(
        get("/api/admin/audit", Some("ada;admin")).await,
        (403, "forbidden".into())
    );
    assert_eq!(
        get("/api/admin/audit", Some("ada;auditor")).await,
        (200, "audit".into())
    );
    // Anonymous route inside a protected group.
    assert_eq!(
        get("/api/admin/health", None).await,
        (200, "healthy".into())
    );
    // Outside the protected group.
    assert_eq!(get("/open", None).await, (200, "anonymous".into()));
    assert_eq!(get("/open", Some("ada")).await, (200, "anonymous".into()));

    // Scopes are all required, on top of the group's roles.
    let post = |user: &'static str| {
        let c = c.clone();
        async move {
            let r = c
                .post("/api/admin/write")
                .header("x-user", user)
                .send()
                .await;
            (r.status().as_u16(), code_or_text(&r))
        }
    };
    assert_eq!(
        post("ada;admin;data:write").await,
        (403, "forbidden".into())
    );
    assert_eq!(
        post("ada;user;data:write data:read").await,
        (403, "forbidden".into())
    );
    assert_eq!(
        post("ada;admin;data:read data:write").await,
        (200, "written".into())
    );
}

#[tokio::test]
async fn policies_are_readable_route_metadata() {
    let c = policy_app();
    let find = |path: &str| -> &'static RouteInfo {
        c.routes()
            .iter()
            .copied()
            .find(|r| r.path() == path)
            .unwrap()
    };
    let me = RoutePolicy::of(find("/api/me"));
    assert!(me.requires_auth() && !me.has_permissions() && !me.is_anonymous());

    let stats = RoutePolicy::of(find("/api/admin/stats"));
    assert_eq!(stats.roles(), ["admin", "root"]);
    assert!(stats.scopes().is_empty());
    let write = find("/api/admin/write");
    assert_eq!(
        write.meta::<RequireScopes>().unwrap().scopes(),
        ["data:write", "data:read"]
    );
    assert_eq!(
        write.meta::<RequireRoles>().unwrap().roles(),
        ["admin", "root"]
    );
    assert!(write.meta::<RequireAuth>().is_some());
    let health = find("/api/admin/health");
    assert!(health.meta::<AllowAnonymous>().is_some());
    let hp = RoutePolicy::of(health);
    assert!(hp.is_anonymous() && !hp.requires_auth() && hp.roles().is_empty());

    let open = RoutePolicy::of(find("/open"));
    assert_eq!(open, RoutePolicy::default());

    let user = User {
        name: "x".into(),
        roles: vec!["root".into()],
        scopes: vec!["data:read".into(), "data:write".into()],
    };
    assert!(RoutePolicy::of(write).allows(&user));
    assert!(!RoutePolicy::of(find("/api/admin/audit")).allows(&user));
}

#[tokio::test]
async fn app_level_policy_methods() {
    let table = App::new()
        .get("/a", || async { "" })
        .require_auth()
        .get("/b", || async { "" })
        .require_roles(["x"])
        .require_roles(["y", "x"])
        .get("/c", || async { "" })
        .require_scopes(["s"])
        .get("/d", || async { "" })
        .allow_anonymous()
        .route_table();
    let p: Vec<RoutePolicy> = table.iter().map(|r| RoutePolicy::of(r)).collect();
    assert!(p[0].requires_auth());
    assert_eq!(p[1].roles(), ["x", "y"]);
    assert_eq!(p[2].scopes(), ["s"]);
    assert!(p[3].is_anonymous());
}

#[tokio::test]
async fn role_policy_without_role_checker_fails_closed() {
    let api = Routes::new()
        .get("/admin", || async { "secret" })
        .require_roles(["admin"])
        .get("/me", whoami)
        .require_auth()
        .wrap(Authenticate::new(HeaderAuth::new()));
    let c = App::new().nest("/api", api).test();
    let r = c
        .get("/api/admin")
        .header("x-user", "ada;admin")
        .send()
        .await;
    assert_eq!(r.status(), 500);
    assert_eq!(code(&r), "auth_misconfigured");
    // Plain authentication requirements still work without a role checker.
    assert_eq!(c.get("/api/me").send().await.status(), 401);
    assert_eq!(
        c.get("/api/me").header("x-user", "ada").send().await.text(),
        "ada"
    );
}

#[tokio::test]
async fn custom_guard_sees_user_and_route() {
    let guard = Guard::new(|u: &User, route: &RouteInfo| {
        // Only "ada" may use DELETE; everyone may read.
        route.method() != Some(&Method::DELETE) || u.name == "ada"
    });
    let api = Routes::new()
        .get("/doc", || async { "doc" })
        .delete("/doc", || async { "deleted" })
        .get("/status", || async { "up" })
        .allow_anonymous()
        .wrap(Authenticate::new(HeaderAuth::new()))
        .wrap(guard);
    let c = App::new().nest("/api", api).test();

    assert_eq!(
        c.get("/api/doc")
            .header("x-user", "bob")
            .send()
            .await
            .text(),
        "doc"
    );
    let r = c.delete("/api/doc").header("x-user", "bob").send().await;
    assert_eq!((r.status().as_u16(), code(&r)), (403, "forbidden".into()));
    let r = c.delete("/api/doc").header("x-user", "ada").send().await;
    assert_eq!(r.text(), "deleted");
    // A guard needs a user...
    let r = c.get("/api/doc").send().await;
    assert_eq!(
        (r.status().as_u16(), code(&r)),
        (401, "missing_credentials".into())
    );
    // ...except on anonymous routes.
    assert_eq!(c.get("/api/status").send().await.text(), "up");
}

#[tokio::test]
async fn guard_roles_and_scopes_are_additive() {
    let api = Routes::new()
        .get("/x", || async { "x" })
        .require_roles(["editor"])
        .wrap(Authenticate::new(HeaderAuth::new()).authorize())
        .wrap(Guard::<User>::roles(["staff"]))
        .wrap(Guard::<User>::scopes(["read"]));
    let c = App::new().nest("/api", api).test();
    let status = |u: &'static str| {
        let c = c.clone();
        async move {
            c.get("/api/x")
                .header("x-user", u)
                .send()
                .await
                .status()
                .as_u16()
        }
    };
    assert_eq!(status("a;editor;read").await, 403); // not staff
    assert_eq!(status("a;staff;read").await, 403); // not editor (route policy)
    assert_eq!(status("a;staff,editor").await, 403); // missing scope
    assert_eq!(status("a;staff,editor;read").await, 200);
}

#[tokio::test]
async fn global_authentication_with_policy_guard() {
    // Authenticate before routing (optional), enforce policies after routing.
    let c = App::new()
        .wrap(Authenticate::new(HeaderAuth::new()))
        .wrap_route(Guard::<User>::policies())
        .get("/public", maybe)
        .get("/me", whoami)
        .require_auth()
        .get("/admin", || async { "admin" })
        .require_roles(["admin"])
        .test();
    assert_eq!(c.get("/public").send().await.text(), "anonymous");
    assert_eq!(
        c.get("/public").header("x-user", "ada").send().await.text(),
        "ada"
    );
    assert_eq!(c.get("/me").send().await.status(), 401);
    assert_eq!(
        c.get("/me").header("x-user", "ada").send().await.text(),
        "ada"
    );
    assert_eq!(
        c.get("/admin")
            .header("x-user", "ada")
            .send()
            .await
            .status(),
        403
    );
    let r = c.get("/admin").header("x-user", "ada;admin").send().await;
    assert_eq!(r.text(), "admin");
    // Global middleware also sees unmatched requests.
    assert_eq!(c.get("/nope").send().await.status(), 404);
}

#[tokio::test]
async fn guard_before_routing_is_a_configuration_error() {
    let c = App::new()
        .wrap(Guard::<User>::policies())
        .get("/", || async { "x" })
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.status(), 500);
    assert_eq!(code(&r), "auth_misconfigured");
}

#[tokio::test]
async fn authenticator_errors_propagate() {
    let failing =
        |_: &Request| async { Err::<Option<String>, _>(Error::service_unavailable("idp down")) };
    let c = App::new()
        .get("/", || async { "x" })
        .wrap_route(Authenticate::new(failing))
        .test();
    let r = c.get("/").send().await;
    assert_eq!(r.status(), 503);
}

// ---------------------------------------------------------------------------
// Fail-closed policies: forgetting or misplacing auth middleware never makes
// a protected route public.

#[tokio::test]
async fn policies_fail_closed_without_auth_middleware() {
    let c = App::new()
        .get("/secret", || async { "secret" })
        .require_auth()
        .get("/admin", || async { "admin" })
        .require_roles(["admin"])
        .get("/public", || async { "public" })
        .test();

    let r = c.get("/secret").send().await;
    assert_eq!(r.status(), 401);
    assert_eq!(code(&r), "missing_credentials");
    assert_eq!(
        c.get("/admin")
            .header("x-user", "ann;admin")
            .send()
            .await
            .status(),
        401
    );
    assert_eq!(c.get("/public").send().await.text(), "public");
}

#[tokio::test]
async fn global_authenticate_satisfies_auth_but_not_unchecked_roles() {
    // Global middleware runs before routing, so it cannot check route roles.
    let c = App::new()
        .wrap(Authenticate::new(HeaderAuth::new()).authorize())
        .get("/me", whoami)
        .require_auth()
        .get("/admin", || async { "admin" })
        .require_roles(["admin"])
        .test();

    assert_eq!(
        c.get("/me").header("x-user", "ann").send().await.text(),
        "ann"
    );
    assert_eq!(c.get("/me").send().await.status(), 401);
    let r = c.get("/admin").header("x-user", "ann;admin").send().await;
    assert_eq!(r.status(), 500);
    assert_eq!(code(&r), "auth_misconfigured");

    // The same setup with route middleware enforces roles properly.
    let c = App::new()
        .wrap_route(Authenticate::new(HeaderAuth::new()).authorize())
        .get("/admin", || async { "admin" })
        .require_roles(["admin"])
        .test();
    assert_eq!(
        c.get("/admin")
            .header("x-user", "ann;admin")
            .send()
            .await
            .text(),
        "admin"
    );
    assert_eq!(
        c.get("/admin")
            .header("x-user", "bob;user")
            .send()
            .await
            .status(),
        403
    );
}

#[tokio::test]
async fn group_policy_with_auth_in_inner_group_is_enforced_in_order() {
    // Policy declared on the outer group, Authenticate on the inner group:
    // the enforcement layer must still run after (inside) authentication.
    let inner = Routes::new()
        .get("/x", || async { "x" })
        .wrap(Authenticate::new(HeaderAuth::new()).authorize());
    let outer = Routes::new()
        .nest("/inner", inner)
        .require_roles_all(["admin"]);
    let c = App::new().nest("/outer", outer).test();

    assert_eq!(
        c.get("/outer/inner/x")
            .header("x-user", "a;admin")
            .send()
            .await
            .text(),
        "x"
    );
    assert_eq!(
        c.get("/outer/inner/x")
            .header("x-user", "a;user")
            .send()
            .await
            .status(),
        403
    );
    assert_eq!(c.get("/outer/inner/x").send().await.status(), 401);
}

#[tokio::test]
async fn guard_policies_marks_routes_verified() {
    let c = App::new()
        .wrap(Authenticate::new(HeaderAuth::new()).authorize())
        .wrap_route(Guard::<User>::policies())
        .get("/admin", || async { "admin" })
        .require_roles(["admin"])
        .test();
    assert_eq!(
        c.get("/admin")
            .header("x-user", "a;admin")
            .send()
            .await
            .text(),
        "admin"
    );
    assert_eq!(
        c.get("/admin")
            .header("x-user", "a;user")
            .send()
            .await
            .status(),
        403
    );
}

#[tokio::test]
async fn every_401_carries_a_challenge() {
    // No Authenticate at all: the fail-closed policy layer and CurrentUser
    // still answer with a challenge (RFC 9110 requires one on 401).
    let c = App::new()
        .get("/policy", || async { "x" })
        .require_auth()
        .get("/me", whoami)
        .test();
    assert_eq!(
        c.get("/policy").send().await.header("www-authenticate"),
        Some("Bearer")
    );
    assert_eq!(
        c.get("/me").send().await.header("www-authenticate"),
        Some("Bearer")
    );

    // A global Authenticate's challenge is reused by the guard.
    let c = App::new()
        .wrap(Authenticate::new(HeaderAuth::new()))
        .wrap_route(Guard::<User>::policies())
        .get("/admin", || async { "admin" })
        .require_auth()
        .test();
    let r = c.get("/admin").send().await;
    assert_eq!(r.status(), 401);
    assert_eq!(r.header("www-authenticate"), Some("X-User"));
}
