// Modified for Distill by Samuel Fajreldines, 2026.
//! The OTLP transports `distill_test_support::MockOtelServer` does not serve: gRPC, gRPC over TLS
//! and mutual TLS, and HTTP over mutual TLS. Each runs on its own thread and runtime, so a sync
//! test can block on `flush` and `shutdown`, and records into the `OtelRecorder` it started with.
#![allow(
    dead_code,
    reason = "each test binary includes this module and calls a different subset"
)]

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Once;

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::MetricsService;
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use prost::Message as _;
use distill_test_support::{MockOtelServer, OtelRecorder, OtelSignal};

/// Install a process-wide test tracing subscriber.
/// Construction and export failures then show up under `--test_output=errors` without production `eprintln!` side effects.
pub fn init_test_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_test_writer()
            .try_init();
    });
}

pub fn block_on<T>(wait: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("wait runtime")
        .block_on(wait)
}

#[derive(Clone)]
struct GrpcCollector {
    recorder: OtelRecorder,
}

#[async_trait::async_trait]
impl LogsService for GrpcCollector {
    async fn export(
        &self,
        request: tonic::Request<ExportLogsServiceRequest>,
    ) -> Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status> {
        self.recorder
            .record_protobuf(OtelSignal::Logs, &request.into_inner().encode_to_vec());
        Ok(tonic::Response::new(ExportLogsServiceResponse::default()))
    }
}

#[async_trait::async_trait]
impl MetricsService for GrpcCollector {
    async fn export(
        &self,
        request: tonic::Request<ExportMetricsServiceRequest>,
    ) -> Result<tonic::Response<ExportMetricsServiceResponse>, tonic::Status> {
        self.recorder
            .record_protobuf(OtelSignal::Metrics, &request.into_inner().encode_to_vec());
        Ok(tonic::Response::new(ExportMetricsServiceResponse::default()))
    }
}

pub fn start_grpc_collector(recorder: OtelRecorder) -> String {
    use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsServiceServer;
    use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::MetricsServiceServer;

    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("collector runtime");
        rt.block_on(async move {
            let incoming = tonic::transport::server::TcpIncoming::bind(
                "127.0.0.1:0".parse().expect("collector bind addr"),
            )
            .expect("bind gRPC collector");
            addr_tx
                .send(incoming.local_addr().expect("collector addr"))
                .expect("send addr");
            let service = GrpcCollector { recorder };
            tonic::transport::Server::builder()
                .add_service(LogsServiceServer::new(service.clone()))
                .add_service(MetricsServiceServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .expect("collector serve");
        });
    });
    let addr = addr_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("collector must start");
    format!("http://{addr}")
}

pub struct TestTlsMaterial {
    pub ca_cert_pem: String,
    pub server_cert_pem: String,
    pub server_key_pem: String,
    pub client_cert_pem: String,
    pub client_key_pem: String,
}

pub fn generate_tls_material() -> TestTlsMaterial {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

    let ca_key = KeyPair::generate().expect("generate CA key");
    let mut ca_params = CertificateParams::new(Vec::new()).expect("CA params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign CA");

    let server_key = KeyPair::generate().expect("generate server key");
    let server_params =
        CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .expect("server params");
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .expect("sign server cert");

    let client_key = KeyPair::generate().expect("generate client key");
    let client_params =
        CertificateParams::new(vec!["grok-client".to_owned()]).expect("client params");
    let client_cert = client_params
        .signed_by(&client_key, &ca_cert, &ca_key)
        .expect("sign client cert");

    TestTlsMaterial {
        ca_cert_pem: ca_cert.pem(),
        server_cert_pem: server_cert.pem(),
        server_key_pem: server_key.serialize_pem(),
        client_cert_pem: client_cert.pem(),
        client_key_pem: client_key.serialize_pem(),
    }
}

pub fn start_grpc_tls_collector(
    recorder: OtelRecorder,
    server_cert_pem: String,
    server_key_pem: String,
) -> String {
    start_grpc_tls_collector_inner(recorder, server_cert_pem, server_key_pem, None)
}

pub fn start_grpc_mtls_collector(
    recorder: OtelRecorder,
    server_cert_pem: String,
    server_key_pem: String,
    client_ca_pem: String,
) -> String {
    start_grpc_tls_collector_inner(
        recorder,
        server_cert_pem,
        server_key_pem,
        Some(client_ca_pem),
    )
}

fn start_grpc_tls_collector_inner(
    recorder: OtelRecorder,
    server_cert_pem: String,
    server_key_pem: String,
    client_ca_pem: Option<String>,
) -> String {
    use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsServiceServer;
    use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::MetricsServiceServer;

    // The test binary links both ring and aws-lc-rs, so rustls cannot pick a process default on its own; the server-side acceptor needs one pinned
    // (The production client is unaffected: tonic passes a provider explicitly.)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("collector runtime");
        rt.block_on(async move {
            let incoming = tonic::transport::server::TcpIncoming::bind(
                "127.0.0.1:0".parse().expect("collector bind addr"),
            )
            .expect("bind gRPC TLS collector");
            addr_tx
                .send(incoming.local_addr().expect("collector addr"))
                .expect("send addr");
            let identity = tonic::transport::Identity::from_pem(server_cert_pem, server_key_pem);
            let mut tls = tonic::transport::ServerTlsConfig::new().identity(identity);
            if let Some(ca_pem) = client_ca_pem {
                tls = tls.client_ca_root(tonic::transport::Certificate::from_pem(ca_pem));
            }
            let service = GrpcCollector { recorder };
            tonic::transport::Server::builder()
                .tls_config(tls)
                .expect("collector TLS config")
                .add_service(LogsServiceServer::new(service.clone()))
                .add_service(MetricsServiceServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .expect("collector serve");
        });
    });
    let addr = addr_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("collector must start");
    format!("https://localhost:{}", addr.port())
}

pub fn start_http_mtls_collector(
    recorder: OtelRecorder,
    server_cert_pem: String,
    server_key_pem: String,
    client_ca_pem: String,
) -> String {
    use axum::{Router, body::Bytes, extract::State, routing::post};
    use axum_server::tls_rustls::RustlsConfig;
    use rustls::RootCertStore;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    use rustls::server::WebPkiClientVerifier;
    use std::sync::Arc;

    // Same process-level CryptoProvider pin as gRPC mTLS tests: the binary links both ring and aws-lc-rs, so rustls will not auto-pick
    // Prefer aws-lc to match the workspace `rustls` feature set and tonic path
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("collector runtime");
        rt.block_on(async move {
            async fn sink(
                State((recorder, signal)): State<(OtelRecorder, OtelSignal)>,
                body: Bytes,
            ) -> &'static str {
                recorder.record_protobuf(signal, &body);
                ""
            }
            let app = OtelSignal::ALL
                .into_iter()
                .fold(Router::new(), |router, signal| {
                    router.route(
                        MockOtelServer::path(signal),
                        post(sink).with_state((recorder.clone(), signal)),
                    )
                });

            let certs: Vec<CertificateDer<'static>> =
                CertificateDer::pem_slice_iter(server_cert_pem.as_bytes())
                    .collect::<Result<_, _>>()
                    .expect("parse server cert pem");
            let key = PrivateKeyDer::from_pem_slice(server_key_pem.as_bytes())
                .expect("parse server key pem");

            let mut roots = RootCertStore::empty();
            for ca in CertificateDer::pem_slice_iter(client_ca_pem.as_bytes()) {
                roots
                    .add(ca.expect("parse client CA"))
                    .expect("add client CA");
            }
            let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .expect("client cert verifier");
            let mut server_config = rustls::ServerConfig::builder()
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, key)
                .expect("server TLS config");
            server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind HTTP mTLS");
            listener.set_nonblocking(true).expect("nonblocking");
            addr_tx
                .send(listener.local_addr().expect("local addr"))
                .expect("send addr");
            let config = RustlsConfig::from_config(Arc::new(server_config));
            axum_server::from_tcp_rustls(listener, config)
                .expect("tls server")
                .serve(app.into_make_service())
                .await
                .expect("HTTP mTLS collector serve");
        });
    });
    let addr = addr_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("collector must start");
    // Prefer localhost so the server cert SAN (localhost and 127.0.0.1) matches whatever the client/rustls hostname check uses
    format!("https://localhost:{}", addr.port())
}
