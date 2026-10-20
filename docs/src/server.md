# Server: TLS, HTTP/2, graceful shutdown

The server chapter will describe the thread-per-core HTTP server and its `ServerConfig` tuning knobs (workers, runtime mode, header read timeout, connection limits), serving with TLS through rustls (`App::tls`), HTTP/2 negotiation, listening on Unix domain sockets, and how signals trigger a graceful shutdown that drains connections, runs shutdown hooks and stops background tasks within their grace periods.

<!-- TODO: completed during integration -->
