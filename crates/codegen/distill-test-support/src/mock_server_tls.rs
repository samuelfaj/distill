// Modified for Distill by Samuel Fajreldines, 2026.
//! HTTPS transport for the mock inference server: a throwaway CA and a loopback leaf, served with
//! no plaintext listener, so a request the mock logs implies a completed TLS handshake.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tempfile::TempDir;
use tokio::sync::oneshot;

/// The CA a client must trust to reach a [`serve_tls`] server.
pub(crate) struct ThrowawayCa {
    /// Kept alive with the server: dropping the TempDir deletes the PEM the child reads.
    _dir: TempDir,
    pem_path: PathBuf,
}

impl ThrowawayCa {
    pub(crate) fn pem_path(&self) -> &Path {
        &self.pem_path
    }
}

/// Serve `app` over HTTPS on `127.0.0.1:0` until `shutdown_rx` fires or its sender drops.
pub(crate) async fn serve_tls(
    app: Router,
    shutdown_rx: oneshot::Receiver<()>,
) -> anyhow::Result<(SocketAddr, ThrowawayCa)> {
    let (config, ca_pem) = generate_tls_material()?;
    let dir = TempDir::new().context("create CA dir")?;
    let pem_path = dir.path().join("ca.pem");
    tokio::fs::write(&pem_path, ca_pem)
        .await
        .context("write CA pem")?;

    // The std listener is re-registered with tokio, which requires non-blocking mode.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("bind mock TLS server")?;
    listener.set_nonblocking(true).context("nonblocking")?;
    let addr = listener.local_addr().context("local_addr")?;
    let tls = RustlsConfig::from_config(Arc::new(config));
    let handle = axum_server::Handle::new();
    let server = axum_server::from_tcp_rustls(listener, tls)
        .context("tls server")?
        .handle(handle.clone());
    tokio::spawn(async move {
        tokio::select! {
            result = server.serve(app.into_make_service()) => result.expect("mock TLS serve"),
            _ = shutdown_rx => handle.shutdown(),
        }
    });
    Ok((
        addr,
        ThrowawayCa {
            _dir: dir,
            pem_path,
        },
    ))
}

/// A throwaway CA and a leaf for `127.0.0.1` and `localhost`.
/// Returns the server config and the CA PEM a client must trust.
fn generate_tls_material() -> anyhow::Result<(rustls::ServerConfig, String)> {
    let ca_key = KeyPair::generate().context("ca key")?;
    let mut ca_params = CertificateParams::new(vec![]).context("ca params")?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).context("ca cert")?;

    let leaf_key = KeyPair::generate().context("leaf key")?;
    let leaf_cert = CertificateParams::new(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])
        .context("leaf params")?
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .context("leaf cert")?;

    // Both rustls providers are linked, so pick explicitly. Mirror distill-extra-ca:
    // aws-lc-rs except Windows ARM64, where jitterentropy overflows the stack (GB-5593).
    // A native harness must not crash in the mock before the child under test is spawned.
    let provider = if cfg!(all(windows, target_arch = "aarch64")) {
        rustls::crypto::ring::default_provider()
    } else {
        rustls::crypto::aws_lc_rs::default_provider()
    };
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .context("protocol versions")?
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf_cert.der().clone(), ca_cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        )
        .context("server config")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((config, ca_cert.pem()))
}
