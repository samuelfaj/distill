use std::sync::atomic::{AtomicUsize, Ordering};

use super::{cached_client, client_cache_key};

#[test]
fn cache_key_changes_for_rotated_material_and_protocol() {
    let original = client_cache_key(b"certificate", b"private-key", false);
    assert!(original != client_cache_key(b"certificate-2", b"private-key", false));
    assert!(original != client_cache_key(b"certificate", b"private-key-2", false));
    assert!(original != client_cache_key(b"certificate", b"private-key", true));
}

#[test]
fn clients_are_reused_for_the_same_identity() {
    static BUILD_CALLS: AtomicUsize = AtomicUsize::new(0);
    let key = client_cache_key(b"cache-test-certificate", b"cache-test-key", false);
    let build = || {
        BUILD_CALLS.fetch_add(1, Ordering::SeqCst);
        reqwest::Client::builder().build()
    };
    assert!(cached_client(key.clone(), build).is_ok());
    assert!(
        cached_client(key, || -> Result<reqwest::Client, reqwest::Error> {
            panic!("cached client must be reused")
        })
        .is_ok()
    );
    assert_eq!(BUILD_CALLS.load(Ordering::SeqCst), 1);
}
