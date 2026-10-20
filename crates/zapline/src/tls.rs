//! TLS termination with [rustls](https://docs.rs/rustls).
//!
//! ```no_run
//! use zapline::prelude::*;
//! use zapline::tls::TlsConfig;
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .get("/", || async { "hello over TLS" })
//!         .tls(TlsConfig::from_pem_files("cert.pem", "key.pem")?)
//!         .run("0.0.0.0:8443")
//! }
//! ```
//!
//! The server advertises ALPN `h2` (when HTTP/2 is enabled) and `http/1.1`,
//! performs each handshake inside the connection's task with a timeout
//! ([`ServerConfig::tls_handshake_timeout`]), and marks requests with
//! [`ConnInfo::secure`]. For client certificates (mTLS), custom cipher
//! suites or certificate resolvers (SNI), build a [`rustls::ServerConfig`]
//! yourself and use [`TlsConfig::from_rustls`].
//!
//! [`ServerConfig::tls_handshake_timeout`]: crate::ServerConfig::tls_handshake_timeout
//! [`ConnInfo::secure`]: crate::ConnInfo::secure

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;

pub use tokio_rustls::rustls;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::App;

/// Certificates and settings for serving HTTPS. Cheap to clone.
#[derive(Clone)]
pub struct TlsConfig {
    config: Arc<rustls::ServerConfig>,
    // Created here so apps that never build a `TlsConfig` do not contain
    // the TLS connection code (see `server::conn`).
    entry: crate::server::TlsEntry,
}

impl TlsConfig {
    /// Load a PEM certificate chain (leaf first) and a PEM private key
    /// (PKCS#8, PKCS#1 or SEC1) from files.
    pub fn from_pem_files(
        cert_path: impl AsRef<Path>,
        key_path: impl AsRef<Path>,
    ) -> io::Result<Self> {
        let read = |path: &Path, what: &str| {
            std::fs::read(path).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("cannot read TLS {what} {}: {e}", path.display()),
                )
            })
        };
        let cert = read(cert_path.as_ref(), "certificate")?;
        let key = read(key_path.as_ref(), "private key")?;
        Self::from_pem(cert, key)
    }

    /// Use an in-memory PEM certificate chain (leaf first) and PEM private
    /// key.
    pub fn from_pem(cert_pem: impl AsRef<[u8]>, key_pem: impl AsRef<[u8]>) -> io::Result<Self> {
        let certs = CertificateDer::pem_slice_iter(cert_pem.as_ref())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| invalid(format!("invalid TLS certificate PEM: {e}")))?;
        if certs.is_empty() {
            return Err(invalid(
                "no certificate found in TLS certificate PEM".into(),
            ));
        }
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_ref())
            .map_err(|e| invalid(format!("invalid TLS private key PEM: {e}")))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| invalid(format!("TLS configuration: {e}")))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| invalid(format!("TLS certificate/key: {e}")))?;
        Ok(Self::from_rustls(Arc::new(config)))
    }

    /// Use a fully custom rustls configuration (client auth, SNI resolvers,
    /// protocol versions...). If its `alpn_protocols` is empty, the server
    /// fills in `h2` (when HTTP/2 is enabled) and `http/1.1`.
    pub fn from_rustls(config: Arc<rustls::ServerConfig>) -> Self {
        TlsConfig {
            config,
            entry: crate::server::TlsEntry::new(),
        }
    }

    /// The underlying rustls configuration.
    pub fn rustls_config(&self) -> &Arc<rustls::ServerConfig> {
        &self.config
    }

    pub(crate) fn entry(&self) -> crate::server::TlsEntry {
        self.entry
    }

    /// The acceptor the server uses, with ALPN filled in.
    pub(crate) fn acceptor(&self, http2: bool) -> tokio_rustls::TlsAcceptor {
        if !self.config.alpn_protocols.is_empty() {
            return self.config.clone().into();
        }
        let mut config = (*self.config).clone();
        config.alpn_protocols = if http2 {
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        } else {
            vec![b"http/1.1".to_vec()]
        };
        Arc::new(config).into()
    }
}

impl fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let alpn: Vec<_> = self
            .config
            .alpn_protocols
            .iter()
            .map(|p| String::from_utf8_lossy(p))
            .collect();
        f.debug_struct("TlsConfig")
            .field("alpn_protocols", &alpn)
            .finish_non_exhaustive()
    }
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

impl App {
    /// Serve HTTPS: terminate TLS on every connection.
    ///
    /// ```no_run
    /// use zapline::prelude::*;
    /// use zapline::tls::TlsConfig;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// App::new()
    ///     .get("/", || async { "secure" })
    ///     .tls(TlsConfig::from_pem_files("cert.pem", "key.pem")?)
    ///     .run("0.0.0.0:8443")
    /// # }
    /// ```
    pub fn tls(mut self, tls: TlsConfig) -> Self {
        self.config.tls = Some(tls);
        self
    }
}
