# Upstream issues

Bugs in dependencies that zapline works around, written up so they can be
filed (search the tracker for an existing report first) and re-checked on
each upgrade. Each one has a canary test in `crates/zapline/tests/` that
fails once the upstream fix lands.

## hyper: automatic error responses are dropped with `pipeline_flush`

**Status:** present in hyper 1.12.0. Workaround: `ServerConfig::pipeline_flush = false`.
Canary: `hyper_drops_automatic_error_responses_with_pipeline_flush` in
`crates/zapline/tests/server.rs`.

**Why zapline can't fix it on its side:** the lost bytes live in hyper's
private write buffer and are dropped with the connection. zapline could
write its own `400` to the socket after hyper reports a parse error, but on a
pipelined connection that response would arrive in place of the earlier
responses hyper also dropped, and the client would match it to the wrong
request. Knowing whether anything else was buffered would take bookkeeping
on every request, or a wrapper around every connection's IO type. Both cost
the hot path (the IO wrapper blocks LTO from inlining hyper's dispatcher;
see `server::conn`).

The draft below is ready to file at <https://github.com/hyperium/hyper/issues>.

---

**Title:** `http1::Builder::pipeline_flush(true)` drops the automatic 400/414/431 response (and buffered pipelined responses) on a request-head parse error

**Version:** hyper 1.12.0, `server` + `http1` features, Linux, Tokio 1.

**Description**

With `pipeline_flush(true)`, a request head that fails to parse (invalid
header line, invalid method/URI/version, head larger than
`max_header_size`/`max_buf_size`, URI too long) closes the connection without
sending the automatic error response. With `pipeline_flush(false)` the
same requests get `400`, `431` or `414` as documented.

Responses to earlier requests on the same connection that were still
buffered are lost as well: a client that pipelines a valid request and a
malformed one in one write receives nothing at all, not even the `200` for
the first request.

**Reproduction**

```rust
// [dependencies] hyper = { version = "1.12", features = ["server", "http1"] }
// hyper-util = { version = "0.1", features = ["tokio"] }
// tokio = { version = "1", features = ["full"] }
// http-body-util = "0.1"
use std::convert::Infallible;
use http_body_util::Full;
use hyper::{body::Bytes, server::conn::http1, service::service_fn, Response};
use hyper_util::rt::TokioIo;

#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            let svc = service_fn(|_req| async {
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from("ok"))))
            });
            let result = http1::Builder::new()
                .pipeline_flush(true) // false: the 400 arrives
                .serve_connection(TokioIo::new(stream), svc)
                .await;
            eprintln!("connection ended: {result:?}");
        });
    }
}
```

```sh
printf 'GET / HTTP/1.1\r\nHost: x\r\nno colon here\r\n\r\n' | nc 127.0.0.1 3000
# expected: HTTP/1.1 400 Bad Request ...   actual: nothing, connection closed

printf 'GET / HTTP/1.1\r\nHost: x\r\n\r\nGET / HTTP/1.1\r\nno colon\r\n\r\n' | nc 127.0.0.1 3000
# expected: HTTP/1.1 200 OK ... ok, then HTTP/1.1 400   actual: nothing
```

The server logs `connection ended: Err(hyper::Error(Parse(Header(Token))))`
in both cases.

**Cause**

1. `Conn::on_read_head_error` (`src/proto/h1/conn.rs`, line 339) calls
   `on_parse_error` (line 836), which queues the error head with
   `write_head` and stores the error in `state.error`. The malformed bytes
   stay in the read buffer (`consume_leading_lines` only strips leading
   CRLFs).
2. `Buffered::poll_flush` (`src/proto/h1/io.rs`, line 278) returns
   `Ready(Ok(()))` without writing while `flush_pipeline && !read_buf.is_empty()`,
   on the assumption that more pipelined requests are about to be parsed and
   their responses batched.
3. The read side is already closed, so nothing is parsed or flushed again.
   `Dispatcher::poll_inner` (`src/proto/h1/dispatch.rs`, line 143) sees
   `is_done()`, `take_error()` returns the parse error, and the connection,
   with its unflushed write buffer, is dropped.

**Suggested fix**

Drop the unparseable input once the automatic response is queued: the
connection closes after it and never reads that buffer again, and an empty
read buffer lets `poll_flush` write. Error path only, so pipelined
throughput is unaffected.

```diff
--- a/src/proto/h1/io.rs
+++ b/src/proto/h1/io.rs
@@ -115,6 +115,11 @@
         self.read_buf.as_ref()
     }
 
+    /// Drop unread input: the connection will not parse it.
+    pub(crate) fn clear_read_buf(&mut self) {
+        self.read_buf.clear();
+    }
+
     #[cfg(test)]
     #[cfg(feature = "nightly")]
     pub(super) fn read_buf_mut(&mut self) -> &mut BytesMut {
--- a/src/proto/h1/conn.rs
+++ b/src/proto/h1/conn.rs
@@ -843,6 +843,7 @@
                 // assert in `write_head`...
                 self.state.cached_headers.take();
                 self.write_head(msg, None);
+                self.io.clear_read_buf();
                 self.state.error = Some(err);
                 return Ok(());
             }
```

Verified against 1.12.0 with zapline's test suite through
`[patch.crates-io]`: with `pipeline_flush(true)` a malformed head gets
`400 Bad Request`, an oversized one `431 Request Header Fields Too Large`,
and the pipelined case gets the `200` followed by the `400`.

An alternative is to skip the pipeline shortcut in `Buffered::poll_flush`
once the read side is closed (`Conn::poll_flush` knows `state.is_read_closed()`).

A regression test: serve with `pipeline_flush(true)`, write
`"GET / HTTP/1.1\r\nHost: x\r\n\r\nGET / HTTP/1.1\r\nbad\r\n\r\n"` and expect
a `200` followed by a `400` before EOF.
