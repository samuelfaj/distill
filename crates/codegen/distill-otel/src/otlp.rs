// Modified for Distill by Samuel Fajreldines, 2026.
use async_trait::async_trait;
use bytes::Bytes;
use opentelemetry_http::{HttpClient, HttpError};
#[derive(Debug, Clone)]
pub struct BlockingOtlpClient(reqwest::blocking::Client);

#[async_trait]
impl HttpClient for BlockingOtlpClient {
    async fn send_bytes(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        let request = request.try_into()?;
        let mut response = self.0.execute(request)?.error_for_status()?;
        let headers = std::mem::take(response.headers_mut());
        let mut http_response = http::Response::builder()
            .status(response.status())
            .body(response.bytes()?)?;
        *http_response.headers_mut() = headers;
        Ok(http_response)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ClientIdentityPaths<'a> {
    pub certificate: &'a str,
    pub key: &'a str,
}

pub fn pem_contains_certificate(pem: &[u8]) -> bool {
    const MARKER: &[u8] = b"-----BEGIN CERTIFICATE-----";
    pem.windows(MARKER.len()).any(|window| window == MARKER)
}

fn pem_begin_line(label_parts: &[&[u8]]) -> Vec<u8> {
    let mut line = Vec::with_capacity(32);
    line.extend_from_slice(b"-----BEGIN ");
    for part in label_parts {
        line.extend_from_slice(part);
    }
    line.extend_from_slice(b"-----");
    line
}

pub fn pem_contains_private_key(pem: &[u8]) -> bool {
    const LABELS: &[&[&[u8]]] = &[
        &[b"PRIVATE", b" KEY"],
        &[b"RSA ", b"PRIVATE", b" KEY"],
        &[b"EC ", b"PRIVATE", b" KEY"],
    ];
    LABELS.iter().any(|parts| {
        let marker = pem_begin_line(parts);
        pem.windows(marker.len()).any(|window| window == marker)
    })
}

pub fn build_blocking_client(
    timeout: std::time::Duration,
    extra_ca_pem_files: &[&str],
) -> Result<BlockingOtlpClient, String> {
    build_blocking_client_with_identity(timeout, extra_ca_pem_files, None)
}

pub fn build_blocking_client_with_identity(
    timeout: std::time::Duration,
    extra_ca_pem_files: &[&str],
    client_identity: Option<ClientIdentityPaths<'_>>,
) -> Result<BlockingOtlpClient, String> {
    let mut extra_roots = Vec::new();
    for path in extra_ca_pem_files {
        let pem = std::fs::read(path)
            .map_err(|e| format!("reading OTEL_EXPORTER_OTLP_CERTIFICATE {path:?}: {e}"))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| format!("parsing OTEL_EXPORTER_OTLP_CERTIFICATE {path:?}: {e}"))?;

        if certs.is_empty() {
            return Err(format!(
                "OTEL_EXPORTER_OTLP_CERTIFICATE {path:?} contains no certificates"
            ));
        }
        extra_roots.extend(certs);
    }
    let identity_pem = match client_identity {
        Some(paths) => {
            let mut cert = std::fs::read(paths.certificate).map_err(|e| {
                format!(
                    "reading OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE {:?}: {e}",
                    paths.certificate
                )
            })?;
            let key = std::fs::read(paths.key).map_err(|e| {
                format!("reading OTEL_EXPORTER_OTLP_CLIENT_KEY {:?}: {e}", paths.key)
            })?;
            if !pem_contains_certificate(&cert) {
                return Err(format!(
                    "OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE {:?} contains no certificates",
                    paths.certificate
                ));
            }
            if !pem_contains_private_key(&key) {
                return Err(format!(
                    "OTEL_EXPORTER_OTLP_CLIENT_KEY {:?} contains no private key",
                    paths.key
                ));
            }
            if !cert.ends_with(b"\n") {
                cert.push(b'\n');
            }
            cert.extend_from_slice(&key);
            Some(cert)
        }
        None => None,
    };
    std::thread::Builder::new()
        .name("otlp-client-build".into())
        .spawn(move || {
            let identity = match identity_pem {
                Some(pem) => Some(reqwest::Identity::from_pem(&pem).map_err(|e| {
                    format!("parsing OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE/KEY: {e}")
                })?),
                None => None,
            };

            distill_extra_ca::build_blocking_reqwest_client(|builder| {
                let mut builder = builder.timeout(timeout).connect_timeout(timeout);
                for cert in &extra_roots {
                    builder = builder.add_root_certificate(cert.clone());
                }
                if let Some(identity) = &identity {
                    builder = builder.identity(identity.clone());
                }
                builder
            })
            .map_err(|e| {
                let mut detail = e.to_string();
                let mut source = std::error::Error::source(&e);
                while let Some(s) = source {
                    detail.push_str(": ");
                    detail.push_str(&s.to_string());
                    source = s.source();
                }
                format!("building blocking OTLP HTTP client: {detail}")
            })
            .map(BlockingOtlpClient)
        })
        .map_err(|e| format!("spawning OTLP client builder thread: {e}"))?
        .join()
        .map_err(|_| "OTLP client builder thread panicked".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_otlp_client_builds_with_embedded_roots() {
        build_blocking_client(std::time::Duration::from_secs(5), &[])
            .expect("client with embedded webpki roots must build on any host");
    }

    #[test]
    fn blocking_otlp_client_fails_closed_on_missing_ca_file() {
        let err = build_blocking_client(
            std::time::Duration::from_secs(5),
            &["/nonexistent/corp-ca.pem"],
        )
        .expect_err("missing CA bundle must fail construction");
        assert!(err.contains("OTEL_EXPORTER_OTLP_CERTIFICATE"));
    }

    #[test]
    fn blocking_otlp_client_fails_closed_on_empty_ca_bundle() {
        let file = tempfile::NamedTempFile::new().expect("temp CA file");
        std::fs::write(file.path(), "# readable, but no PEM certificate blocks\n")
            .expect("write empty bundle");
        let err = build_blocking_client(
            std::time::Duration::from_secs(5),
            &[file.path().to_str().expect("utf-8 path")],
        )
        .expect_err("certificate-less bundle must fail construction");
        assert!(err.contains("no certificates"));
    }

    #[test]
    fn blocking_otlp_client_fails_closed_on_missing_client_cert() {
        let err = build_blocking_client_with_identity(
            std::time::Duration::from_secs(5),
            &[],
            Some(ClientIdentityPaths {
                certificate: "/nonexistent/client.crt",
                key: "/nonexistent/client.key",
            }),
        )
        .expect_err("missing client cert must fail construction");
        assert!(err.contains("OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE"));
    }

    #[test]
    fn blocking_otlp_client_builds_with_generated_client_identity() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

        let ca_key = KeyPair::generate().expect("ca key");
        let mut ca_params = CertificateParams::new(Vec::new()).expect("ca params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).expect("ca");

        let client_key = KeyPair::generate().expect("client key");
        let client_params =
            CertificateParams::new(vec!["grok-client".into()]).expect("client params");
        let client_cert = client_params
            .signed_by(&client_key, &ca_cert, &ca_key)
            .expect("sign client");

        let cert_file = tempfile::NamedTempFile::new().expect("cert file");
        let key_file = tempfile::NamedTempFile::new().expect("key file");
        let ca_file = tempfile::NamedTempFile::new().expect("ca file");
        std::fs::write(cert_file.path(), client_cert.pem()).expect("write cert");
        std::fs::write(key_file.path(), client_key.serialize_pem()).expect("write key");
        std::fs::write(ca_file.path(), ca_cert.pem()).expect("write ca");

        build_blocking_client_with_identity(
            std::time::Duration::from_secs(5),
            &[ca_file.path().to_str().expect("utf-8")],
            Some(ClientIdentityPaths {
                certificate: cert_file.path().to_str().expect("utf-8"),
                key: key_file.path().to_str().expect("utf-8"),
            }),
        )
        .expect("HTTP client with mTLS identity must build");
    }
}
