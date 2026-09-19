// Modified for Distill by Samuel Fajreldines, 2026.
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Once};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::Version;
use axum::routing::post;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use rustls::server::WebPkiClientVerifier;
use support::{send_one, test_config};
use distill_sampler::{SamplerConfig, SamplingClient};
use distill_sampling_types::SamplingError;
use distill_test_support::{TestSandbox, spawn_counting_server};

const ONE_MIB: usize = 1024 * 1024;

struct TestTlsMaterial {
    ca_cert_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
}

struct TestTlsFixture {
    _sandbox: TestSandbox,
    ca_path: PathBuf,
    cert_dir: PathBuf,
    material: TestTlsMaterial,
}

#[derive(Default)]
struct TestTlsServerState {
    accepted_requests: AtomicUsize,
    http1_requests: AtomicUsize,
    http2_requests: AtomicUsize,
}

static TLS_FIXTURE: LazyLock<TestTlsFixture> = LazyLock::new(|| {
    let sandbox = TestSandbox::new();
    let material = generate_tls_material();
    let ca_path = sandbox.root().join("ca.crt");
    fs::write(&ca_path, &material.ca_cert_pem).expect("write CA certificate");
    let cert_dir = sandbox.root().join("identity");
    fs::create_dir(&cert_dir).expect("create identity directory");
    fs::write(cert_dir.join("client.crt"), &material.client_cert_pem)
        .expect("write client certificate");
    fs::write(cert_dir.join("client.key"), &material.client_key_pem)
        .expect("write client private key");
    TestTlsFixture {
        _sandbox: sandbox,
        ca_path,
        cert_dir,
        material,
    }
});

fn pin_env() {
    static PIN: Once = Once::new();
    PIN.call_once(|| {
        support::pin_env();
        let fixture = &*TLS_FIXTURE;
        // SAFETY: Every test in this process calls `pin_env` before building a client.
        // `PIN` serializes the one mutation while concurrent tests wait.
        unsafe { std::env::set_var("GROK_EXTRA_CA_BUNDLE", &fixture.ca_path) };
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_client_keeps_per_config_headers_isolated() {
    pin_env();
    let (base_url, _accepts, heads) = spawn_counting_server().await;
    let mut cfg_a = test_config(&base_url, "token-a");
    cfg_a
        .extra_headers
        .insert("x-test-extra".to_string(), "isolated-a".to_string());
    let mut cfg_b = test_config(&base_url, "token-b");
    cfg_b
        .extra_headers
        .insert("x-test-extra".to_string(), "isolated-b".to_string());
    let a = SamplingClient::new(cfg_a).unwrap();
    let b = SamplingClient::new(cfg_b).unwrap();
    send_one(&a).await;
    send_one(&b).await;

    let heads = heads.lock().unwrap();
    assert_eq!(heads.len(), 2);
    assert!(heads[0].contains("Bearer token-a") && heads[0].contains("isolated-a"));
    assert!(!heads[0].contains("token-b") && !heads[0].contains("isolated-b"));
    assert!(heads[1].contains("Bearer token-b") && heads[1].contains("isolated-b"));
    assert!(!heads[1].contains("token-a") && !heads[1].contains("isolated-a"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_http1_fallback_never_pools() {
    pin_env();
    let (base_url, accepts, _heads) = spawn_counting_server().await;
    let mut cfg = test_config(&base_url, "token-a");
    cfg.force_http1 = true;
    let client = SamplingClient::new(cfg).unwrap();
    send_one(&client).await;
    send_one(&client).await;
    assert_eq!(accepts.load(Ordering::SeqCst), 2);
}

fn mtls_config(cert_dir: &Path) -> SamplerConfig {
    SamplerConfig {
        base_url: "https://example.test".to_owned(),
        mtls_cert_dir: Some(cert_dir.to_owned()),
        ..Default::default()
    }
}

fn invalid_configuration_message(config: SamplerConfig) -> String {
    let error = SamplingClient::new(config).expect_err("mTLS configuration must fail");
    assert!(!error.is_retryable());
    match error {
        SamplingError::MtlsConfiguration(message) => message,
        other => panic!("expected invalid mTLS configuration, got {other:?}"),
    }
}

#[test]
fn mtls_path_selection_prefers_client_names_when_either_exists() {
    pin_env();
    let sandbox = TestSandbox::new();
    let fallback_dir = sandbox.root().join("fallback");
    fs::create_dir(&fallback_dir).unwrap();
    let message = invalid_configuration_message(mtls_config(&fallback_dir));
    let fallback_cert = fallback_dir.join("tls.crt").display().to_string();
    assert!(message.contains(fallback_cert.as_str()));

    for existing_name in ["client.crt", "client.key"] {
        let cert_dir = sandbox.root().join(existing_name);
        fs::create_dir(&cert_dir).unwrap();
        fs::write(cert_dir.join(existing_name), b"present").unwrap();
        let message = invalid_configuration_message(mtls_config(&cert_dir));
        let missing_name = if existing_name == "client.crt" {
            "client.key"
        } else {
            "client.crt"
        };
        let missing_path = cert_dir.join(missing_name).display().to_string();
        assert!(message.contains(missing_path.as_str()));
        assert!(!message.contains("tls.crt") && !message.contains("tls.key"));
    }
}

#[test]
fn mtls_oversized_file_is_rejected_before_identity_parsing() {
    pin_env();
    let sandbox = TestSandbox::new();
    let cert_dir = sandbox.root().join("identity");
    fs::create_dir(&cert_dir).unwrap();
    fs::write(cert_dir.join("client.crt"), vec![b'x'; ONE_MIB + 1]).unwrap();

    let message = invalid_configuration_message(mtls_config(&cert_dir));
    let cert_path = cert_dir.join("client.crt").display().to_string();
    assert!(message.contains(cert_path.as_str()));
    assert!(message.contains("exceeds the 1048576 byte limit"));
}

#[test]
fn mtls_invalid_pem_reports_both_identity_paths() {
    pin_env();
    let sandbox = TestSandbox::new();
    let cert_dir = sandbox.root().join("identity");
    fs::create_dir(&cert_dir).unwrap();
    fs::write(cert_dir.join("client.crt"), b"not a certificate").unwrap();
    fs::write(cert_dir.join("client.key"), b"not a private key").unwrap();

    let message = invalid_configuration_message(mtls_config(&cert_dir));
    let cert_path = cert_dir.join("client.crt").display().to_string();
    let key_path = cert_dir.join("client.key").display().to_string();
    assert!(message.contains("invalid mTLS client identity"));
    assert!(message.contains(cert_path.as_str()));
    assert!(message.contains(key_path.as_str()));
}

#[test]
fn mtls_configuration_errors_remain_directory_scoped() {
    pin_env();
    let sandbox = TestSandbox::new();
    let first_dir = sandbox.root().join("first");
    let second_dir = sandbox.root().join("second");
    fs::create_dir(&first_dir).unwrap();
    fs::create_dir(&second_dir).unwrap();

    let first = invalid_configuration_message(mtls_config(&first_dir));
    let second = invalid_configuration_message(mtls_config(&second_dir));
    let first_path = first_dir.join("tls.crt").display().to_string();
    let second_path = second_dir.join("tls.crt").display().to_string();
    assert!(first.contains(first_path.as_str()) && !first.contains(second_path.as_str()));
    assert!(second.contains(second_path.as_str()) && !second.contains(first_path.as_str()));
}

fn generate_tls_material() -> TestTlsMaterial {
    let ca_key = KeyPair::generate().expect("generate CA key");
    let mut ca_params = CertificateParams::new(Vec::new()).expect("CA params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign CA");

    let server_key = KeyPair::generate().expect("generate server key");
    let server_params =
        CertificateParams::new(vec!["localhost".to_owned()]).expect("server params");
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

async fn start_mtls_server(tls: &TestTlsMaterial, state: Arc<TestTlsServerState>) -> String {
    async fn accept_request(
        State(state): State<Arc<TestTlsServerState>>,
        request: Request,
    ) -> &'static str {
        state.accepted_requests.fetch_add(1, Ordering::SeqCst);
        match request.version() {
            Version::HTTP_11 => {
                state.http1_requests.fetch_add(1, Ordering::SeqCst);
            }
            Version::HTTP_2 => {
                state.http2_requests.fetch_add(1, Ordering::SeqCst);
            }
            version => panic!("unexpected negotiated HTTP version: {version:?}"),
        }
        r#"{"id":"test","object":"chat.completion","created":0,"model":"test","choices":[]}"#
    }

    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(tls.server_cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .expect("parse server certificate");
    let key = PrivateKeyDer::from_pem_slice(tls.server_key_pem.as_bytes())
        .expect("parse server private key");
    let mut client_roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(tls.ca_cert_pem.as_bytes()) {
        client_roots
            .add(ca.expect("parse client CA"))
            .expect("add client CA");
    }
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(client_roots))
        .build()
        .expect("build client certificate verifier");
    let mut server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, key)
        .expect("build server TLS config");
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let app = Router::new()
        .route("/chat/completions", post(accept_request))
        .with_state(state);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mTLS server");
    listener.set_nonblocking(true).expect("set nonblocking");
    let port = listener.local_addr().expect("server address").port();
    let config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));
    tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, config)
            .expect("create mTLS server")
            .serve(app.into_make_service())
            .await
            .expect("serve mTLS endpoint");
    });
    tokio::task::yield_now().await;
    format!("https://localhost:{port}")
}

#[tokio::test(flavor = "current_thread")]
async fn configured_identity_completes_a_required_mtls_handshake() {
    pin_env();
    let fixture = &*TLS_FIXTURE;
    // Initialize the sampler's approved rustls provider before the test server
    // builds its own rustls config in this feature-unified test binary.
    drop(
        SamplingClient::new(test_config("https://localhost", "test-token"))
            .expect("initialize sampler TLS policy"),
    );
    let state = Arc::new(TestTlsServerState::default());
    let base_url = start_mtls_server(&fixture.material, state.clone()).await;

    let without_identity = test_config(&base_url, "test-token");
    send_one(&SamplingClient::new(without_identity).expect("build uncredentialed client")).await;
    assert_eq!(
        state.accepted_requests.load(Ordering::SeqCst),
        0,
        "the server must reject a client that presents no certificate"
    );

    let mut with_identity = test_config(&base_url, "test-token");
    with_identity.mtls_cert_dir = Some(fixture.cert_dir.clone());
    send_one(&SamplingClient::new(with_identity.clone()).expect("build HTTP/2 mTLS client")).await;
    assert_eq!(
        state.http2_requests.load(Ordering::SeqCst),
        1,
        "the production-default client must negotiate HTTP/2 with its mTLS identity"
    );

    with_identity.force_http1 = true;
    send_one(&SamplingClient::new(with_identity).expect("build HTTP/1.1 mTLS client")).await;
    assert_eq!(
        state.http1_requests.load(Ordering::SeqCst),
        1,
        "the fallback client must negotiate HTTP/1.1 with its mTLS identity"
    );
    assert_eq!(
        state.accepted_requests.load(Ordering::SeqCst),
        2,
        "both credentialed protocol paths must complete the mTLS handshake"
    );
}
