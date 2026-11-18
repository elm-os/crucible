//! TCP, DNS and TLS (contract 2.5). The outbound HTTP client and `tls_accept` join in a later wave.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::clock::Instant;

/// A network failure.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// The deadline passed before the operation finished.
    #[error("deadline elapsed")]
    DeadlineElapsed,
    /// The TLS server name is not a valid DNS name or IP address.
    #[error("invalid server name")]
    InvalidServerName,
    /// The TLS configuration or handshake failed.
    #[error("tls: {0}")]
    Tls(String),
    /// A socket or resolver error.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The trust anchors for [`Net::tls_connect`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TlsRoots {
    /// The bundled Mozilla root program (the `webpki-roots` crate in production).
    WebPki,
    /// Exactly these DER-encoded CA certificates.
    Certificates(Vec<Bytes>),
}

/// The byte-stream capabilities every [`Stream`] backend provides.
pub trait StreamIo: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug> StreamIo for T {}

/// A connected byte stream (TCP, or TLS over one). It implements tokio's `AsyncRead` and `AsyncWrite`, so
/// hyper, axum and the database clients' connectors run over it unchanged.
#[derive(Debug)]
pub struct Stream(Box<dyn StreamIo>);

impl Stream {
    /// Wraps a backend's stream. Called by `Net` implementations.
    pub fn new(io: impl StreamIo + 'static) -> Self {
        Self(Box::new(io))
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

/// A bound TCP listener.
#[async_trait]
pub trait Listener: fmt::Debug + Send {
    /// Waits for the next connection and returns it with the peer's address. Cancel-safe.
    async fn accept(&mut self) -> Result<(Stream, SocketAddr), NetError>;

    /// The address the listener is bound to.
    fn local_addr(&self) -> Result<SocketAddr, NetError>;
}

/// The network. Every outbound connection in product code goes through here.
#[async_trait]
pub trait Net: fmt::Debug + Send + Sync {
    /// Binds a TCP listener.
    async fn listen(&self, addr: SocketAddr) -> Result<Box<dyn Listener>, NetError>;

    /// Connects to a literal socket address; never resolves names. Fails with
    /// [`NetError::DeadlineElapsed`] when `deadline` (on the `Env` clock) passes first, and without
    /// connecting when it has already passed.
    async fn connect(&self, addr: SocketAddr, deadline: Instant) -> Result<Stream, NetError>;

    /// Resolves a host name to its addresses, in resolver order without duplicates.
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError>;

    /// Runs a TLS client handshake over `stream`, verifying the peer against `roots` and `server_name`.
    /// Dropping the future drops the stream.
    async fn tls_connect(
        &self,
        stream: Stream,
        server_name: &str,
        roots: &TlsRoots,
    ) -> Result<Stream, NetError>;
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// An in-memory stream end: reads drain `input`, writes collect in `output`.
    #[derive(Debug)]
    struct Scripted {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl AsyncRead for Scripted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.input).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Scripted {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.output).poll_write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn stream_forwards_reads_and_writes_to_its_backend() {
        fn assert_usable<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(_: &T) {}
        let mut stream = Stream::new(Scripted {
            input: io::Cursor::new(b"hello".to_vec()),
            output: Vec::new(),
        });
        assert_usable(&stream);
        block_on(async {
            let mut read = Vec::new();
            stream.read_to_end(&mut read).await.unwrap();
            assert_eq!(read, b"hello");
            stream.write_all(b"world").await.unwrap();
            stream.flush().await.unwrap();
            stream.shutdown().await.unwrap();
        });
    }
}
