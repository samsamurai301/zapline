//! Tests for the extension points plugins build on: lifecycle hooks, build
//! hooks, route metadata, route middleware, error handler, connection info.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use zapline::prelude::*;
use zapline::{ErrorReport, RouteInfo};

#[derive(Clone)]
struct Db(&'static str);

#[tokio::test]
async fn startup_hooks_provide_state_and_shutdown_runs_in_reverse() {
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (o1, o2) = (order.clone(), order.clone());
    let c = App::new()
        .state(41u32)
        .on_startup(|res: Resources| async move {
            let base = res.require::<u32>()?;
            res.insert(base + 1);
            res.insert(Db("pool"));
            Ok(())
        })
        .on_shutdown(move |_| async move { o1.lock().unwrap().push("first") })
        .on_shutdown(move |_| async move { o2.lock().unwrap().push("second") })
        .get(
            "/",
            |State(n): State<u32>, State(db): State<Db>| async move { format!("{n} {}", db.0) },
        )
        .test();

    assert_eq!(c.get("/").send().await.text(), "42 pool");
    c.shutdown().await;
    assert_eq!(*order.lock().unwrap(), vec!["second", "first"]);
}

#[tokio::test]
#[should_panic(expected = "startup failed")]
async fn failing_startup_hook_aborts() {
    let c = App::new()
        .on_startup(|_| async { Err(Error::internal("db unreachable")) })
        .test();
    c.get("/").send().await;
}

#[derive(Clone, Debug, PartialEq)]
struct Tag(&'static str);

#[tokio::test]
async fn route_meta_build_hooks_and_route_middleware() {
    let seen = Arc::new(AtomicUsize::new(0));
    let seen2 = seen.clone();

    let users = Routes::new()
        .get("/{id}", |MatchedPath(p): MatchedPath| async move { p })
        .meta(Tag("special"))
        .get("/", || async { "list" })
        .meta_all(Tag("users"));

    let c = App::new()
        .nest("/users", users)
        .wrap_route(|req: Request, next: Next| async move {
            // Route middleware sees the matched route and its metadata.
            let tag = req
                .route()
                .and_then(|r| r.meta::<Tag>())
                .map(|t| t.0)
                .unwrap_or("-");
            let mut res = next.run(req).await;
            res.headers_mut()
                .insert("x-tag", HeaderValue::from_static(tag));
            res
        })
        .on_build(move |table: &[&'static RouteInfo]| {
            seen2.store(table.len(), Ordering::SeqCst);
            let summary: Vec<String> = table
                .iter()
                .map(|r| format!("{} {}", r.method().unwrap(), r.path()))
                .collect();
            Routes::new().get("/_routes", move || {
                let s = summary.join(",");
                async move { s }
            })
        })
        .test();

    assert_eq!(seen.load(Ordering::SeqCst), 2);
    let r = c.get("/users/7").send().await;
    assert_eq!(r.text(), "/users/{id}");
    assert_eq!(r.header("x-tag"), Some("special"));
    assert_eq!(c.get("/users").send().await.header("x-tag"), Some("users"));
    assert_eq!(
        c.get("/_routes").send().await.text(),
        "GET /users/{id},GET /users"
    );
    assert_eq!(c.routes().len(), 3);
}

#[tokio::test]
async fn plugins_are_functions() {
    fn ping(app: App) -> App {
        app.get("/ping", || async { "pong" })
    }
    let c = App::new().plugin(ping).test();
    assert_eq!(c.get("/ping").send().await.text(), "pong");
}

#[tokio::test]
async fn error_codes_details_headers_and_custom_handler() {
    async fn fail() -> Result<()> {
        Err(Error::unprocessable("bad input")
            .with_code("invalid_input")
            .with_details(serde_json::json!({"field": "name"}))
            .with_header(http::header::RETRY_AFTER, HeaderValue::from_static("5")))
    }
    let plain = App::new().get("/", fail).test();
    let r = plain.get("/").send().await;
    assert_eq!(r.status(), 422);
    assert_eq!(r.header("retry-after"), Some("5"));
    assert_eq!(
        r.json::<serde_json::Value>(),
        serde_json::json!({"error": "bad input", "code": "invalid_input", "details": {"field": "name"}})
    );

    let custom = App::new()
        .get("/", fail)
        .error_handler(|e: &ErrorReport| {
            (
                e.status,
                format!(
                    "problem: {} ({})",
                    e.message,
                    e.code.as_deref().unwrap_or("")
                ),
            )
                .into_response()
        })
        .test();
    let r = custom.get("/").send().await;
    assert_eq!(r.status(), 422);
    assert_eq!(r.text(), "problem: bad input (invalid_input)");
    assert_eq!(
        custom.get("/missing").send().await.text(),
        "problem: not found ()"
    );
}

#[tokio::test]
async fn conn_info_extractor() {
    let c = App::new()
        .get("/", |conn: ConnInfo| async move {
            conn.remote_addr.unwrap().to_string()
        })
        .test();
    let r = c
        .get("/")
        .remote_addr("10.0.0.9:5555".parse().unwrap())
        .send()
        .await;
    assert_eq!(r.text(), "10.0.0.9:5555");
}

#[tokio::test]
async fn real_server_reports_peer_address() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        App::new()
            .get("/", |c: ConnInfo| async move {
                c.remote_addr.unwrap().ip().to_string()
            })
            .serve(listener),
    );
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.ends_with("127.0.0.1"), "{out}");
}

#[tokio::test]
async fn endpoint_layers_run_innermost_across_nested_groups() {
    use std::sync::Mutex;
    use zapline::middleware::Middleware;
    use zapline::{BoxFuture, EndpointLayers};

    type Log = Arc<Mutex<Vec<&'static str>>>;

    #[derive(Clone)]
    struct Mark(&'static str, Log);
    impl Middleware for Mark {
        fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
            self.1.lock().unwrap().push(self.0);
            next.run(req)
        }
    }

    let log: Log = Arc::default();
    let mut layers = EndpointLayers::default();
    layers.add(Mark("innermost", log.clone()));

    let inner = Routes::new()
        .get("/x", || async { "x" })
        .wrap(Mark("inner-group", log.clone()));
    let mut outer = Routes::new()
        .nest("/inner", inner)
        .wrap(Mark("outer-group", log.clone()));
    outer.defaults_mut().insert(layers);
    let c = App::new()
        .nest("/outer", outer)
        .wrap_route(Mark("route", log.clone()))
        .test();

    assert_eq!(c.get("/outer/inner/x").send().await.text(), "x");
    // The same layer type registered twice is kept once.
    assert_eq!(
        *log.lock().unwrap(),
        vec!["route", "outer-group", "inner-group", "innermost"]
    );
}

#[test]
fn meta_hooks_compose_across_nested_groups() {
    /// Accumulated by hooks, inner groups first.
    #[derive(Clone, Debug, Default, PartialEq)]
    struct Trail(Vec<String>);

    let runs = Arc::new(AtomicUsize::new(0));
    let push = |label: &'static str| {
        let runs = runs.clone();
        move |meta: &mut zapline::http::Extensions| {
            runs.fetch_add(1, Ordering::SeqCst);
            // Hooks run after defaults are merged: they see `meta_all` values.
            let tag = meta.get::<Tag>().map_or("-", |t| t.0);
            meta.get_or_insert_default::<Trail>()
                .0
                .push(format!("{label}:{tag}"));
        }
    };

    let inner = Routes::new()
        .map_meta_all(push("inner"))
        // Routes registered after the hook are covered too.
        .get("/x", || async { "x" })
        .meta_all(Tag("inner"));
    let outer = Routes::new()
        .nest("/in", inner)
        .get("/y", || async { "y" })
        .map_meta_all(push("outer"))
        .meta_all(Tag("outer"));
    let table = App::new().nest("/out", outer).route_table();

    let route = |path: &str| *table.iter().find(|r| r.path() == path).unwrap();
    let x = route("/out/in/x");
    assert_eq!(
        x.meta::<Trail>().unwrap().0,
        ["inner:inner", "outer:inner"],
        "outer hooks run after inner ones"
    );
    // `meta_all` keeps its replace semantics: the innermost value wins.
    assert_eq!(x.meta::<Tag>(), Some(&Tag("inner")));
    let y = route("/out/y");
    assert_eq!(y.meta::<Trail>().unwrap().0, ["outer:outer"]);
    assert_eq!(y.meta::<Tag>(), Some(&Tag("outer")));
    // Once per route per hook, at build time.
    assert_eq!(runs.load(Ordering::SeqCst), 3);
}

#[test]
fn meta_hooks_apply_to_routes_from_build_hooks() {
    #[derive(Clone, Debug, PartialEq)]
    struct Seen;

    let table = App::new()
        .get("/a", || async { "a" })
        .on_build(|_: &[&'static RouteInfo]| {
            Routes::new().get("/b", || async { "b" }).map_meta_all(|m| {
                m.insert(Seen);
            })
        })
        .route_table();
    assert!(table[0].meta::<Seen>().is_none());
    assert_eq!(table[1].path(), "/b");
    assert_eq!(table[1].meta::<Seen>(), Some(&Seen));
}

#[tokio::test]
async fn error_keeps_repeated_headers() {
    let c = App::new()
        .get("/", || async {
            Err::<(), _>(
                Error::unauthorized()
                    .with_header(
                        http::header::WWW_AUTHENTICATE,
                        HeaderValue::from_static("Bearer"),
                    )
                    .with_header(
                        http::header::WWW_AUTHENTICATE,
                        HeaderValue::from_static("Basic realm=\"x\""),
                    )
                    .with_header(http::header::RETRY_AFTER, HeaderValue::from_static("1")),
            )
        })
        .test();
    let r = c.get("/").send().await;
    let values: Vec<_> = r.headers().get_all("www-authenticate").iter().collect();
    assert_eq!(values, ["Bearer", "Basic realm=\"x\""]);
    assert_eq!(r.header("retry-after"), Some("1"));
}

#[tokio::test]
async fn error_handler_keeps_protocol_headers() {
    let c = App::new()
        .get("/x", || async { "x" })
        .get("/auth", || async {
            Err::<(), _>(
                Error::unauthorized()
                    .with_header(
                        http::header::WWW_AUTHENTICATE,
                        HeaderValue::from_static("Basic realm=\"x\""),
                    )
                    .with_header(http::header::SET_COOKIE, HeaderValue::from_static("a=1"))
                    .with_header(http::header::SET_COOKIE, HeaderValue::from_static("b=2")),
            )
        })
        .error_handler(|e: &ErrorReport| (e.status, "custom").into_response())
        .test();
    let r = c.get("/auth").send().await;
    assert_eq!(r.text(), "custom");
    assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));
    assert_eq!(r.header("www-authenticate"), Some("Basic realm=\"x\""));
    assert_eq!(r.headers().get_all("set-cookie").iter().count(), 2);
    let r = c.post("/x").send().await;
    assert_eq!(r.status(), 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD"));
}
