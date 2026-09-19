// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared fixtures for the sampler wire-test binaries.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::Once;

use distill_sampler::{SamplerConfig, SamplingClient};
use distill_sampling_types::{ContentPart, ConversationItem, ConversationRequest, UserItem};

/// These wire tests own a dedicated binary: the pinned env latches
/// process-wide at first use and must not leak into or be poisoned by other
/// test binaries.
pub fn pin_env() {
    static PIN: Once = Once::new();
    // SAFETY: runs before any test builds a client; the kill switch, pool knobs, and OnceLock'd pool_idle_timeout latch once at first use, and racing tests block on the Once.
    PIN.call_once(|| unsafe {
        std::env::remove_var("GROK_SAMPLER_SHARED_CLIENT");
        std::env::set_var("GROK_POOL_MAX_IDLE", "2");
        std::env::set_var("GROK_POOL_IDLE_TIMEOUT_SECS", "90");
    });
}

pub async fn settle_pool() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

pub fn test_config(base_url: &str, api_key: &str) -> SamplerConfig {
    SamplerConfig {
        api_key: Some(api_key.to_string()),
        base_url: base_url.to_string(),
        model: "test-model".to_string(),
        ..SamplerConfig::default()
    }
}

pub async fn send_one(client: &SamplingClient) {
    let request = ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from("hi"),
            }],
            ..Default::default()
        })],
        ..Default::default()
    };
    let _ = client.conversation(request).await;
}
