// Modified for Distill by Samuel Fajreldines, 2026.
// The bare builder panics when ring and aws-lc-rs are both compiled in.
#[test]
fn ensure_is_idempotent_and_bare_client_config_builder_does_not_panic() {
    distill_extra_ca::ensure_default_crypto_provider();
    distill_extra_ca::ensure_default_crypto_provider();
    let _ = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
}

#[test]
fn rustls_client_config_builds_and_is_shared() {
    let a = distill_extra_ca::rustls_client_config();
    let b = distill_extra_ca::rustls_client_config();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
}

#[test]
fn rustls_client_config_uses_the_process_default_provider() {
    let config = distill_extra_ca::rustls_client_config();
    let default =
        rustls::crypto::CryptoProvider::get_default().expect("ensure installed a default");
    assert!(std::sync::Arc::ptr_eq(config.crypto_provider(), default));
}
