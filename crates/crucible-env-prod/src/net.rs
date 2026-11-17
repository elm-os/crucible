//! TCP and DNS on tokio, TLS on rustls with the ring provider.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use crucible_env::{Clock, ClockExt, Instant, Listener, Net, NetError, Stream, TlsRoots};
use rustls::ClientConfig;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::{TcpListener, TcpStream};

/// The production [`Net`]. Accepted and connected sockets have `TCP_NODELAY` set.
#[derive(Debug)]
pub struct TokioNet {
    clock: Arc<dyn Clock>,
    /// The client configuration for [`TlsRoots::WebPki`], built on first use.
    webpki: OnceLock<Arc<ClientConfig>>,
}

impl TokioNet {
    /// Creates the network; `clock` measures connect deadlines.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            webpki: OnceLock::new(),
        }
    }

    fn client_config(&self, roots: &TlsRoots) -> Result<Arc<ClientConfig>, NetError> {
        match roots {
            TlsRoots::WebPki => {
                if let Some(config) = self.webpki.get() {
                    return Ok(Arc::clone(config));
                }
                let config = build_client_config(roots)?;
                Ok(Arc::clone(self.webpki.get_or_init(|| config)))
            }
            TlsRoots::Certificates(_) => build_client_config(roots),
        }
    }
}

fn tls_error(e: impl std::fmt::Display) -> NetError {
    NetError::Tls(e.to_string())
}

fn build_client_config(roots: &TlsRoots) -> Result<Arc<ClientConfig>, NetError> {
    let mut store = rustls::RootCertStore::empty();
    match roots {
        TlsRoots::WebPki => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        TlsRoots::Certificates(certificates) => {
            for der in certificates {
                store
                    .add(CertificateDer::from(der.to_vec()))
                    .map_err(tls_error)?;
            }
        }
    }
    // The provider is passed explicitly; nothing here depends on a process-wide default.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .with_root_certificates(store)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct TokioListener(TcpListener);

fn tcp_stream(stream: TcpStream) -> Stream {
    // Best effort: a socket that refuses the option still works.
    let _nodelay = stream.set_nodelay(true);
    Stream::new(stream)
}

#[async_trait]
impl Listener for TokioListener {
    async fn accept(&mut self) -> Result<(Stream, SocketAddr), NetError> {
        let (stream, peer) = self.0.accept().await?;
        Ok((tcp_stream(stream), peer))
    }

    fn local_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(self.0.local_addr()?)
    }
}

#[async_trait]
impl Net for TokioNet {
    async fn listen(&self, addr: SocketAddr) -> Result<Box<dyn Listener>, NetError> {
        Ok(Box::new(TokioListener(TcpListener::bind(addr).await?)))
    }

    async fn connect(&self, addr: SocketAddr, deadline: Instant) -> Result<Stream, NetError> {
        // A deadline that has already passed fails without touching the network.
        if self.clock.now() >= deadline {
            return Err(NetError::DeadlineElapsed);
        }
        match self
            .clock
            .timeout_at(deadline, TcpStream::connect(addr))
            .await
        {
            Ok(stream) => Ok(tcp_stream(stream?)),
            Err(_elapsed) => Err(NetError::DeadlineElapsed),
        }
    }

    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        let mut addresses = Vec::new();
        for addr in tokio::net::lookup_host((host, 0)).await? {
            if !addresses.contains(&addr.ip()) {
                addresses.push(addr.ip());
            }
        }
        Ok(addresses)
    }

    async fn tls_connect(
        &self,
        stream: Stream,
        server_name: &str,
        roots: &TlsRoots,
    ) -> Result<Stream, NetError> {
        let config = self.client_config(roots)?;
        let name = ServerName::try_from(server_name.to_owned())
            .map_err(|_| NetError::InvalidServerName)?;
        let tls = tokio_rustls::TlsConnector::from(config)
            .connect(name, stream)
            .await
            .map_err(|e| {
                // rustls reports protocol and certificate failures as `InvalidData`.
                if e.kind() == std::io::ErrorKind::InvalidData {
                    tls_error(e)
                } else {
                    NetError::Io(e)
                }
            })?;
        Ok(Stream::new(tls))
    }
}
