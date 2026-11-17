//! The production network on loopback, including a TLS handshake against a local rustls server.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_methods)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{Clock, Instant, Net, NetError, Stream, TlsRoots};
use crucible_env_prod::{SystemClock, TokioNet};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

fn net() -> (Arc<SystemClock>, TokioNet) {
    let clock = Arc::new(SystemClock::new());
    (clock.clone(), TokioNet::new(clock))
}

fn soon(clock: &SystemClock) -> Instant {
    clock.now().saturating_add(Duration::from_secs(10))
}

#[tokio::test]
async fn loopback_connect_accept_and_echo() {
    let (clock, net) = net();
    let mut listener = net.listen(LOOPBACK).await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 0);

    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await.unwrap();
        let mut buf = [0; 4];
        stream.read_exact(&mut buf).await.unwrap();
        stream.write_all(&buf).await.unwrap();
        stream.write_all(b"!").await.unwrap();
        stream.shutdown().await.unwrap();
        peer
    });

    let mut client = net.connect(addr, soon(&clock)).await.unwrap();
    client.write_all(b"ping").await.unwrap();
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).await.unwrap();
    assert_eq!(reply, b"ping!");
    assert!(server.await.unwrap().ip().is_loopback());
}

#[tokio::test]
async fn connect_fails_when_nothing_listens_or_the_deadline_has_passed() {
    let (clock, net) = net();
    let listener = net.listen(LOOPBACK).await.unwrap();
    let addr = listener.local_addr().unwrap();

    // A deadline that has already passed fails even though the listener is ready.
    let late = net.connect(addr, clock.now()).await;
    assert!(matches!(late, Err(NetError::DeadlineElapsed)), "{late:?}");

    drop(listener);
    let refused = net.connect(addr, soon(&clock)).await;
    assert!(matches!(refused, Err(NetError::Io(_))), "{refused:?}");
}

#[tokio::test]
async fn resolve_returns_addresses_without_duplicates() {
    let (_clock, net) = net();
    assert_eq!(
        net.resolve("127.0.0.1").await.unwrap(),
        [IpAddr::V4(Ipv4Addr::LOCALHOST)]
    );
    let localhost = net.resolve("localhost").await.unwrap();
    assert!(
        !localhost.is_empty() && localhost.iter().all(IpAddr::is_loopback),
        "{localhost:?}"
    );
    let mut unique = localhost.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), localhost.len());
    assert!(net.resolve("no-such-host.invalid").await.is_err());
}

/// A local TLS server with a fresh self-signed certificate for `localhost`. Returns its address and the
/// certificate; each accepted connection is answered with `hello` after the handshake.
async fn tls_server(net: &TokioNet) -> (SocketAddr, Bytes) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let certificate: CertificateDer<'static> = cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let mut listener = net.listen(LOOPBACK).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _peer) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                // Handshakes that the client rejects end here with an error.
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let mut buf = [0; 5];
                    if tls.read_exact(&mut buf).await.is_ok() {
                        let _sent = tls.write_all(&buf.to_ascii_uppercase()).await;
                        let _closed = tls.shutdown().await;
                    }
                }
            });
        }
    });
    (addr, Bytes::copy_from_slice(&certificate))
}

async fn dial(net: &TokioNet, clock: &SystemClock, addr: SocketAddr) -> Stream {
    net.connect(addr, soon(clock)).await.unwrap()
}

#[tokio::test]
async fn tls_handshake_with_given_roots_carries_data() {
    let (clock, net) = net();
    let (addr, certificate) = tls_server(&net).await;
    let roots = TlsRoots::Certificates(vec![certificate]);

    let tcp = dial(&net, &clock, addr).await;
    let mut tls = net.tls_connect(tcp, "localhost", &roots).await.unwrap();
    tls.write_all(b"hello").await.unwrap();
    let mut reply = [0; 5];
    tls.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"HELLO");
}

#[tokio::test]
async fn tls_rejects_untrusted_roots_wrong_names_and_bad_input() {
    let (clock, net) = net();
    let (addr, certificate) = tls_server(&net).await;
    let roots = TlsRoots::Certificates(vec![certificate]);

    // The public root program does not contain the test certificate.
    let untrusted = net
        .tls_connect(
            dial(&net, &clock, addr).await,
            "localhost",
            &TlsRoots::WebPki,
        )
        .await;
    assert!(matches!(untrusted, Err(NetError::Tls(_))), "{untrusted:?}");

    // Trusted certificate, but not for this name.
    let wrong_name = net
        .tls_connect(dial(&net, &clock, addr).await, "example.com", &roots)
        .await;
    assert!(
        matches!(wrong_name, Err(NetError::Tls(_))),
        "{wrong_name:?}"
    );

    let bad_name = net
        .tls_connect(dial(&net, &clock, addr).await, "not a host name", &roots)
        .await;
    assert!(
        matches!(bad_name, Err(NetError::InvalidServerName)),
        "{bad_name:?}"
    );

    let bad_roots = TlsRoots::Certificates(vec![Bytes::from_static(b"not a certificate")]);
    let bad_root = net
        .tls_connect(dial(&net, &clock, addr).await, "localhost", &bad_roots)
        .await;
    assert!(matches!(bad_root, Err(NetError::Tls(_))), "{bad_root:?}");
}
