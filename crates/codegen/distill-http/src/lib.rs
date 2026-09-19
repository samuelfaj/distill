// Modified for Distill by Samuel Fajreldines, 2026.
//! HTTP clients for the application.
//!
//! Building a `reqwest::Client` is expensive (~95ms: it loads OS TLS roots), so
//! the shared clients are `OnceLock`-cached. Sampling traffic uses the
//! process-wide clients owned by `distill_sampler::shared_http`. TLS policy
//! (backend pin, roots, provider) lives in `distill_extra_ca`.

#![deny(clippy::indexing_slicing)]

use std::sync::OnceLock;

use distill_workspace::permission::ClientType;

/// Per-attempt ceiling for a startup `/settings` or `/v1/models` fetch.
pub const STARTUP_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Cap on auth during a non-interactive boot (token refresh or cold-start mint).
pub const STARTUP_AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Ceiling on a single startup token-refresh round trip, separate from
/// `STARTUP_FETCH_TIMEOUT` so the two tune independently.
pub const STARTUP_AUTH_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Outer bound on a single settings-reapply task (drives up to `SETTINGS_FETCH_MAX_ATTEMPTS` fetches).
pub const SETTINGS_REAPPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Attempt budget for the background settings fetch: bounds proxy load while covering a brief blip.
pub const SETTINGS_FETCH_MAX_ATTEMPTS: u32 = 3;
/// Backoff step between settings-fetch attempts.
pub const SETTINGS_RETRY_BACKOFF_STEP: std::time::Duration = std::time::Duration::from_millis(500);
const _: () = assert!(
    SETTINGS_REAPPLY_TIMEOUT.as_millis()
        > STARTUP_FETCH_TIMEOUT.as_millis() * (1 + SETTINGS_FETCH_MAX_ATTEMPTS as u128),
    "SETTINGS_REAPPLY_TIMEOUT must exceed STARTUP_FETCH_TIMEOUT * (1 + MAX_ATTEMPTS)"
);
/// Covers one fetch timeout plus one backoff step; must stay under `SETTINGS_REAPPLY_TIMEOUT`.
pub const STARTUP_SETTINGS_WAIT_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(5_500);
const _: () = assert!(
    STARTUP_SETTINGS_WAIT_DEADLINE.as_millis() < SETTINGS_REAPPLY_TIMEOUT.as_millis(),
    "STARTUP_SETTINGS_WAIT_DEADLINE must stay under SETTINGS_REAPPLY_TIMEOUT"
);
/// Covers the worst-case three-attempt settings ladder with backoff. Settings and
/// models run concurrently, each on its own budget, so a slow models fetch cannot
/// consume this window; must exceed `STARTUP_SETTINGS_WAIT_DEADLINE`.
pub const MANAGED_STARTUP_SETTINGS_WAIT_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(25);
const _: () = assert!(
    MANAGED_STARTUP_SETTINGS_WAIT_DEADLINE.as_millis() > STARTUP_SETTINGS_WAIT_DEADLINE.as_millis(),
    "the managed deadline is the longer of the two"
);
const _: () = assert!(
    MANAGED_STARTUP_SETTINGS_WAIT_DEADLINE.as_millis() < SETTINGS_REAPPLY_TIMEOUT.as_millis(),
    "MANAGED_STARTUP_SETTINGS_WAIT_DEADLINE must stay under SETTINGS_REAPPLY_TIMEOUT"
);

/// Lower bound on a client's connect-to-leader timeout: a slow but valid boot
/// (bounded startup auth + leader startup + handshake) must never be aborted.
pub const MIN_CLIENT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const _: () = assert!(
    MIN_CLIENT_CONNECT_TIMEOUT.as_millis() >= 2 * STARTUP_AUTH_TIMEOUT.as_millis(),
    "MIN_CLIENT_CONNECT_TIMEOUT must stay >= 2x STARTUP_AUTH_TIMEOUT"
);

macro_rules! startup_timer {
    ($name:literal) => {{
        use distill_telemetry::instrumentation::{
            InstrumentationMode, InstrumentationTimer, TARGET, current_mode,
        };
        let mode = current_mode();
        match mode {
            InstrumentationMode::Chrome => {
                let span = tracing::info_span!(target: TARGET, $name);
                InstrumentationTimer::new_with_span($name, mode, Some(span.entered()))
            }
            _ => InstrumentationTimer::new($name),
        }
    }};
}

static CLIENT_TYPE: OnceLock<ClientType> = OnceLock::new();

pub use distill_sampler::OriginClientInfo;

pub fn origin_client_info_from_env() -> Option<OriginClientInfo> {
    std::env::var("GROK_CLIENT_NAME")
        .ok()
        .map(|product| OriginClientInfo {
            product,
            version: std::env::var("GROK_CLIENT_VERSION").ok(),
        })
}

pub fn origin_client_info_from_client_type(
    client_type: ClientType,
    version: Option<String>,
) -> OriginClientInfo {
    OriginClientInfo {
        product: client_type.user_agent_label().to_string(),
        version,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformInfo {
    os: String,
    arch: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UserAgent {
    origin: OriginClientInfo,
    agent_product: &'static str,
    agent_version: String,
    platform: PlatformInfo,
}

impl PlatformInfo {
    fn current() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            other => other,
        }
        .to_string();

        let arch = match std::env::consts::ARCH {
            "arm64" => "aarch64",
            "x86_64" => "x86_64",
            other => other,
        }
        .to_string();

        Self { os, arch }
    }
}

impl UserAgent {
    fn render(&self) -> String {
        if self.origin.product == self.agent_product
            && self.origin.version.as_deref() == Some(self.agent_version.as_str())
        {
            return format!(
                "{}/{} ({}; {})",
                self.agent_product, self.agent_version, self.platform.os, self.platform.arch,
            );
        }

        match self.origin.version.as_deref() {
            Some(origin_version) => format!(
                "{}/{} {}/{} ({}; {})",
                self.origin.product,
                origin_version,
                self.agent_product,
                self.agent_version,
                self.platform.os,
                self.platform.arch,
            ),
            None => format!(
                "{} {}/{} ({}; {})",
                self.origin.product,
                self.agent_product,
                self.agent_version,
                self.platform.os,
                self.platform.arch,
            ),
        }
    }
}

fn agent_version() -> String {
    distill_version::VERSION.to_string()
}

pub fn set_client_name(client_type: ClientType) {
    CLIENT_TYPE
        .set(client_type)
        .expect("set_client_name called more than once");
}

pub fn process_user_agent_string() -> String {
    let agent_version = agent_version();
    let origin = origin_client_info_from_env().unwrap_or_else(|| {
        origin_client_info_from_client_type(
            CLIENT_TYPE.get().copied().unwrap_or(ClientType::Generic),
            Some(agent_version.clone()),
        )
    });

    UserAgent {
        origin,
        agent_product: "grok-shell",
        agent_version,
        platform: PlatformInfo::current(),
    }
    .render()
}

pub fn session_user_agent_string(origin: &OriginClientInfo) -> String {
    UserAgent {
        origin: origin.clone(),
        agent_product: "grok-shell",
        agent_version: agent_version(),
        platform: PlatformInfo::current(),
    }
    .render()
}

pub fn origin_client_info_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<OriginClientInfo> {
    let product = meta
        .and_then(|m| m.get("clientIdentifier"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            meta.and_then(|m| m.get("clientType"))
                .and_then(|v| serde_json::from_value::<ClientType>(v.clone()).ok())
                .map(|client_type| client_type.user_agent_label().to_string())
        });

    product.map(|product| OriginClientInfo {
        product,
        version: meta
            .and_then(|m| m.get("clientVersion"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

pub fn merge_origin_client_info(
    primary: Option<OriginClientInfo>,
    fallback: Option<OriginClientInfo>,
) -> Option<OriginClientInfo> {
    match (primary, fallback) {
        (Some(primary), Some(fallback)) => Some(OriginClientInfo {
            product: primary.product,
            version: primary.version.or(fallback.version),
        }),
        (Some(primary), None) => Some(primary),
        (None, Some(fallback)) => Some(fallback),
        (None, None) => None,
    }
}

pub fn client_type_from_origin(origin: Option<&OriginClientInfo>) -> ClientType {
    ClientType::from_client_identifier(origin.map(|o| o.product.as_str()))
}

pub fn process_client_identifier() -> String {
    std::env::var("GROK_CLIENT_NAME").unwrap_or_else(|_| "grok-shell".to_string())
}

pub const CLIENT_MODE_HEADER: &str = "x-grok-client-mode";

static CLIENT_MODE: OnceLock<&'static str> = OnceLock::new();

pub fn set_process_client_mode_headless() {
    let _ = CLIENT_MODE.set("headless");
}

pub fn process_client_mode() -> &'static str {
    CLIENT_MODE.get().copied().unwrap_or("interactive")
}

pub fn user_agent_string_for(origin: &OriginClientInfo) -> String {
    session_user_agent_string(origin)
}

pub fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let _timer = startup_timer!("startup.http_client_build");
            distill_extra_ca::build_reqwest_client(|builder| {
                builder
                    .connect_timeout(std::time::Duration::from_secs(30))
                    .user_agent(process_user_agent_string())
                    .pool_idle_timeout(std::time::Duration::from_secs(30))
                    .http2_keep_alive_interval(std::time::Duration::from_secs(20))
                    .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
                    .http2_keep_alive_while_idle(true)
                    .tcp_keepalive(std::time::Duration::from_secs(30))
            })
            .expect("failed to build shared HTTP client")
        })
        .clone()
}

pub fn with_auth_retry(
    client: reqwest::Client,
    credentials: std::sync::Arc<dyn distill_auth::AuthCredentialProvider>,
) -> reqwest_middleware::ClientWithMiddleware {
    reqwest_middleware::ClientBuilder::new(client)
        .with(distill_auth::AuthRetryMiddleware::new(credentials, 1))
        .build()
}

pub fn shared_upload_client() -> reqwest::Client {
    static UPLOAD_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    UPLOAD_CLIENT
        .get_or_init(|| {
            distill_extra_ca::build_reqwest_client(|builder| {
                builder
                    .http1_only()
                    .pool_max_idle_per_host(2)
                    .pool_idle_timeout(std::time::Duration::from_secs(10))
                    .user_agent(process_user_agent_string())
            })
            .expect("failed to build shared upload HTTP client")
        })
        .clone()
}

pub(crate) fn fresh_http1_client() -> reqwest::Result<reqwest::Client> {
    distill_extra_ca::build_reqwest_client(|builder| {
        builder
            .http1_only()
            .pool_max_idle_per_host(0)
            .user_agent(process_user_agent_string())
    })
}

pub fn error_cause_chain(err: &dyn std::error::Error) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        msg.push_str(": ");
        msg.push_str(&cause.to_string());
        source = cause.source();
    }
    msg
}

pub fn find_os_error_code(err: &(dyn std::error::Error + 'static)) -> Option<i32> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(code) = e.downcast_ref::<std::io::Error>().and_then(|ioe| {
            ioe.raw_os_error()
                .or_else(|| parse_os_error(&ioe.to_string()))
        }) {
            return Some(code);
        }
        cur = e.source();
    }
    None
}

fn parse_os_error(msg: &str) -> Option<i32> {
    msg.rsplit_once("(os error ")?
        .1
        .trim_end_matches(')')
        .parse()
        .ok()
}

#[derive(Debug, PartialEq, Eq)]
pub enum TransportFailureKind {
    Unreachable,
    CertificateUntrusted,
    CertificateInvalid,
    Interrupted,
    Permanent,
}

#[derive(Debug, PartialEq)]
pub struct TransportFailure {
    pub kind: TransportFailureKind,
    pub detail: String,
}

impl TransportFailure {
    pub fn classify(e: &reqwest::Error) -> Self {
        let kind = transport_kind(
            certificate_error(e),
            e.is_connect(),
            e.is_timeout() || e.is_request() || e.is_body(),
        );
        Self {
            kind,
            detail: error_cause_chain(e),
        }
    }
}

fn transport_kind(
    cert: Option<CertVerdict>,
    is_connect: bool,
    is_interrupted: bool,
) -> TransportFailureKind {
    match cert {
        Some(CertVerdict::UntrustedIssuer) => TransportFailureKind::CertificateUntrusted,
        Some(CertVerdict::Other) => TransportFailureKind::CertificateInvalid,
        None if is_connect => TransportFailureKind::Unreachable,
        None if is_interrupted => TransportFailureKind::Interrupted,
        None => TransportFailureKind::Permanent,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CertVerdict {
    UntrustedIssuer,
    Other,
}

fn certificate_error(err: &(dyn std::error::Error + 'static)) -> Option<CertVerdict> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(rustls::Error::InvalidCertificate(cert)) = e.downcast_ref::<rustls::Error>() {
            return Some(match cert {
                rustls::CertificateError::UnknownIssuer => CertVerdict::UntrustedIssuer,
                _ => CertVerdict::Other,
            });
        }
        cur = match e
            .downcast_ref::<std::io::Error>()
            .and_then(|ioe| ioe.get_ref())
        {
            Some(payload) => Some(payload as &(dyn std::error::Error + 'static)),
            None => e.source(),
        };
    }
    None
}

pub async fn send_with_retry_escaping_pool<T, E, Op, OpFut, Backoff, BackoffFut>(
    op: Op,
    max_attempts: u32,
    is_retryable: impl Fn(&E) -> bool,
    backoff: Backoff,
) -> Result<T, E>
where
    E: std::fmt::Display,
    Op: Fn(reqwest::Client) -> OpFut,
    OpFut: std::future::Future<Output = Result<T, E>>,
    Backoff: Fn(u32) -> BackoffFut,
    BackoffFut: std::future::Future<Output = ()>,
{
    let max_attempts = max_attempts.max(1);
    let pooled = shared_client();
    let mut fresh: Option<reqwest::Client> = None;
    let mut last_err: Option<E> = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            backoff(attempt).await;
        }
        let client = if attempt > 0 && attempt + 1 == max_attempts {
            match &fresh {
                Some(c) => c.clone(),
                None => match fresh_http1_client() {
                    Ok(c) => fresh.insert(c).clone(),
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to build pool-escape client; final attempt stays on pooled client");
                        pooled.clone()
                    }
                },
            }
        } else {
            pooled.clone()
        };
        match op(client).await {
            Ok(value) => return Ok(value),
            Err(e) if is_retryable(&e) => {
                tracing::debug!(attempt, error = %e, "send_with_retry_escaping_pool: retrying after transient failure");
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_err.expect("send_with_retry_escaping_pool ran at least one attempt"))
}

pub fn shared_startup_blocking_client() -> reqwest::blocking::Client {
    static BLOCKING_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    BLOCKING_CLIENT
        .get_or_init(|| {
            let _timer = startup_timer!("startup.http_blocking_client_build");
            distill_extra_ca::build_blocking_reqwest_client(|builder| {
                builder
                    .connect_timeout(STARTUP_FETCH_TIMEOUT)
                    .timeout(STARTUP_FETCH_TIMEOUT)
                    .user_agent(process_user_agent_string())
                    .pool_idle_timeout(std::time::Duration::from_secs(30))
                    .tcp_keepalive(std::time::Duration::from_secs(30))
            })
            .expect("failed to build shared blocking HTTP client")
        })
        .clone()
}

#[allow(clippy::disallowed_methods)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_cause_chain_appends_hidden_sources() {
        #[derive(Debug)]
        struct Leaf;
        impl std::fmt::Display for Leaf {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "connection closed before message completed")
            }
        }
        impl std::error::Error for Leaf {}

        #[derive(Debug)]
        struct Wrapper(Leaf);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "error sending request")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        assert_eq!(
            error_cause_chain(&Wrapper(Leaf)),
            "error sending request: connection closed before message completed",
            "the hidden source cause must be appended after ': '"
        );
    }

    #[test]
    fn find_os_error_code_walks_source_chain() {
        #[derive(Debug)]
        struct IoLeaf(std::io::Error);
        impl std::fmt::Display for IoLeaf {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "io leaf")
            }
        }
        impl std::error::Error for IoLeaf {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        #[derive(Debug)]
        struct Wrapper(IoLeaf);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "wrapper")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let err = Wrapper(IoLeaf(std::io::Error::from_raw_os_error(104)));
        assert_eq!(find_os_error_code(&err), Some(104));
        assert_eq!(find_os_error_code(&std::io::Error::other("no code")), None);
    }

    #[test]
    fn recovers_code_from_a_custom_io_error() {
        let tls_shaped = std::io::Error::other("Connection reset by peer (os error 54)");
        assert_eq!(tls_shaped.raw_os_error(), None, "precondition: no raw code");
        assert_eq!(find_os_error_code(&tls_shaped), Some(54));

        let windows_shaped = std::io::Error::other(
            "An existing connection was forcibly closed by the remote host. (os error 10054)",
        );
        assert_eq!(find_os_error_code(&windows_shaped), Some(10054));
    }

    #[test]
    fn real_connection_reset_classifies_as_interrupted_with_os_code() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let sock = listener.accept().expect("accept").0;
            sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("read timeout");
            let _ = sock.peek(&mut [0u8; 64]);
            drop(sock);
        });

        let err = reqwest::blocking::Client::new()
            .get(format!("http://127.0.0.1:{port}/oauth2/device/code"))
            .send()
            .expect_err("reset must fail the request");

        assert_eq!(
            TransportFailure::classify(&err).kind,
            TransportFailureKind::Interrupted
        );
        assert!(
            matches!(find_os_error_code(&err), Some(54 | 104 | 10054)),
            "reset must carry an OS code, got {:?}",
            find_os_error_code(&err)
        );
    }

    #[test]
    fn certificate_errors_split_untrusted_issuer_from_other_invalid() {
        let wrap = |e: rustls::CertificateError| {
            std::io::Error::other(rustls::Error::InvalidCertificate(e))
        };
        assert_eq!(
            certificate_error(&wrap(rustls::CertificateError::UnknownIssuer)),
            Some(CertVerdict::UntrustedIssuer)
        );
        assert_eq!(
            certificate_error(&wrap(rustls::CertificateError::Expired)),
            Some(CertVerdict::Other)
        );
        assert_eq!(
            certificate_error(&wrap(rustls::CertificateError::NotValidForName)),
            Some(CertVerdict::Other)
        );
        assert_eq!(
            certificate_error(&std::io::Error::other("connection reset")),
            None
        );
    }

    #[test]
    fn transport_kind_maps_every_certificate_verdict_before_connect() {
        assert_eq!(
            transport_kind(Some(CertVerdict::UntrustedIssuer), true, false),
            TransportFailureKind::CertificateUntrusted
        );
        assert_eq!(
            transport_kind(Some(CertVerdict::Other), true, false),
            TransportFailureKind::CertificateInvalid
        );
        assert_eq!(
            transport_kind(None, true, false),
            TransportFailureKind::Unreachable
        );
        assert_eq!(
            transport_kind(None, false, true),
            TransportFailureKind::Interrupted
        );
        assert_eq!(
            transport_kind(None, false, false),
            TransportFailureKind::Permanent
        );
    }

    #[test]
    fn untrusted_certificate_over_real_handshake_classifies_as_certificate_untrusted() {
        if ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
            .iter()
            .any(|v| std::env::var_os(v).is_some())
        {
            eprintln!("skipping: proxy environment set");
            return;
        }
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("cert");
        let server_config = rustls::ServerConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
        )
        .expect("server config");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut conn = rustls::ServerConnection::new(std::sync::Arc::new(server_config))
                .expect("server conn");
            let _ = conn.complete_io(&mut sock);
        });

        let err = shared_startup_blocking_client()
            .get(format!("https://localhost:{port}/"))
            .send()
            .expect_err("an untrusted certificate must fail the request");

        let failure = TransportFailure::classify(&err);
        assert_eq!(
            failure.kind,
            TransportFailureKind::CertificateUntrusted,
            "must not be mistaken for an unreachable server: {}",
            failure.detail
        );
    }

    #[test]
    fn parse_os_error_ignores_messages_without_a_code() {
        assert_eq!(
            parse_os_error("connection closed before message completed"),
            None
        );
        assert_eq!(
            parse_os_error("invalid peer certificate (os error oops)"),
            None
        );
        assert_eq!(parse_os_error("broken pipe (os error 32)"), Some(32));
    }

    #[test]
    fn origin_client_info_from_meta_extracts_identifier_and_version() {
        let meta = serde_json::json!({
            "clientIdentifier": "grok-desktop",
            "clientVersion": "1.2.3",
        })
        .as_object()
        .cloned()
        .unwrap();
        assert_eq!(
            origin_client_info_from_meta(Some(&meta)),
            Some(OriginClientInfo {
                product: "grok-desktop".to_string(),
                version: Some("1.2.3".to_string()),
            })
        );
    }

    #[test]
    fn origin_client_info_from_meta_uses_client_type_when_identifier_absent() {
        let meta = serde_json::json!({
            "clientType": "grok_pager",
            "clientVersion": "0.1.2",
        })
        .as_object()
        .cloned()
        .unwrap();
        assert_eq!(
            origin_client_info_from_meta(Some(&meta)),
            Some(OriginClientInfo {
                product: "grok-pager".to_string(),
                version: Some("0.1.2".to_string()),
            })
        );
    }

    #[test]
    fn merge_origin_client_info_preserves_primary_product_and_backfills_version() {
        let merged = merge_origin_client_info(
            Some(OriginClientInfo {
                product: "grok-web".to_string(),
                version: None,
            }),
            Some(OriginClientInfo {
                product: "grok-desktop".to_string(),
                version: Some("1.2.3".to_string()),
            }),
        );
        assert_eq!(
            merged,
            Some(OriginClientInfo {
                product: "grok-web".to_string(),
                version: Some("1.2.3".to_string()),
            })
        );
    }

    #[test]
    fn session_user_agent_string_renders_expected_variants() {
        let with_version = session_user_agent_string(&OriginClientInfo {
            product: "grok-desktop".to_string(),
            version: Some("1.2.3".to_string()),
        });
        assert!(with_version.starts_with("grok-desktop/1.2.3 grok-shell/"));
        assert!(with_version.contains(" ("));

        let without_version = session_user_agent_string(&OriginClientInfo {
            product: "grok-web".to_string(),
            version: None,
        });
        assert!(without_version.starts_with("grok-web grok-shell/"));
        assert!(!without_version.starts_with("grok-web/"));
    }

    #[test]
    fn user_agent_render_collapses_duplicate_origin_and_agent_identity() {
        let ua = UserAgent {
            origin: OriginClientInfo {
                product: "grok-shell".to_string(),
                version: Some("0.1.171".to_string()),
            },
            agent_product: "grok-shell",
            agent_version: "0.1.171".to_string(),
            platform: PlatformInfo {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
            },
        };

        assert_eq!(ua.render(), "grok-shell/0.1.171 (macos; aarch64)");
    }

    #[tokio::test]
    async fn send_with_retry_escaping_pool_combinator_behavior() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let op_calls = AtomicU32::new(0);
        let backoffs = AtomicU32::new(0);
        let exhausted: Result<(), u32> = send_with_retry_escaping_pool(
            |_client| {
                let n = op_calls.fetch_add(1, Ordering::SeqCst);
                async move { Err(n) }
            },
            3,
            |_e: &u32| true,
            |_attempt| {
                backoffs.fetch_add(1, Ordering::SeqCst);
                std::future::ready(())
            },
        )
        .await;
        assert_eq!(exhausted, Err(2), "returns the last attempt's error");
        assert_eq!(
            op_calls.load(Ordering::SeqCst),
            3,
            "op runs max_attempts times"
        );
        assert_eq!(
            backoffs.load(Ordering::SeqCst),
            2,
            "backoff awaited max_attempts-1 times"
        );

        let op_calls = AtomicU32::new(0);
        let backoffs = AtomicU32::new(0);
        let fast: Result<(), u32> = send_with_retry_escaping_pool(
            |_client| {
                op_calls.fetch_add(1, Ordering::SeqCst);
                async { Err(7) }
            },
            5,
            |_e: &u32| false,
            |_attempt| {
                backoffs.fetch_add(1, Ordering::SeqCst);
                std::future::ready(())
            },
        )
        .await;
        assert_eq!(fast, Err(7));
        assert_eq!(
            op_calls.load(Ordering::SeqCst),
            1,
            "a non-retryable error fails fast"
        );
        assert_eq!(
            backoffs.load(Ordering::SeqCst),
            0,
            "no backoff on a fast failure"
        );

        let op_calls = AtomicU32::new(0);
        let ok: Result<u32, u32> = send_with_retry_escaping_pool(
            |_client| {
                let n = op_calls.fetch_add(1, Ordering::SeqCst);
                let outcome: Result<u32, u32> = if n == 0 { Err(1) } else { Ok(42) };
                async move { outcome }
            },
            5,
            |_e: &u32| true,
            |_attempt| std::future::ready(()),
        )
        .await;
        assert_eq!(ok, Ok(42));
        assert_eq!(
            op_calls.load(Ordering::SeqCst),
            2,
            "stops at the first success"
        );
    }
}
