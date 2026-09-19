// Modified for Distill by Samuel Fajreldines, 2026.
#[test]
fn websocket_config_offers_http11_only() {
    let config = distill_extra_ca::rustls_client_config();
    assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
}
