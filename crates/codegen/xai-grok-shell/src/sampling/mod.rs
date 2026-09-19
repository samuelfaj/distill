pub mod conversation;
pub mod error;
pub mod types;

// `Client` is the legacy alias used throughout the shell; it points at the sampler crate's `SamplingClient`
// The two have identical method sets, so call sites compile unchanged
pub use self::conversation::*;
pub use self::error::{ResponseModelMetadata, Result, SamplingError};
pub use self::types::*;
pub use xai_grok_sampler::ApiBackend;
pub use xai_grok_sampler::SamplingClient as Client;

// Re-export async-openai Responses API types under `rs` namespace
pub use async_openai::types::responses as rs;

// --------------------------------------------------------------------------- xai-grok-sampler re-exports --------------------------------------------------------------------------- The actual streaming / retry / HTTP-client logic lives in the `xai-grok-sampler` crate
// These re-exports keep `crate::sampling::{SamplerHandle, SamplerConfig, ...}` paths working for callers not yet ported to `xai_grok_sampler::*`
// There is no shell-side `sampling::client::Config` composite anymore; `MvpAgent` holds session-snapshot state in a `RefCell<SamplerConfig>`
pub use xai_grok_sampler::{
    ConversationGroupId, InferenceLatencyStats, OriginClientInfo, RequestId, SamplerActor,
    SamplerConfig, SamplerHandle, SamplingChannel, SamplingClient, SamplingErrorInfo,
    SamplingErrorKind, SamplingEvent,
};

const CONVERSATION_GROUP_NAMESPACE: &str = "xai:remote-code:conversation-group:";

/// Derive the stable group shared by a root session and every descendant session.
pub(crate) fn derive_conversation_group_id(root_session_id: &str) -> ConversationGroupId {
    let namespace_input = format!("{CONVERSATION_GROUP_NAMESPACE}{root_session_id}");
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, namespace_input.as_bytes())
        .to_string()
        .into()
}

#[cfg(test)]
mod conversation_group_tests {
    use pretty_assertions::{assert_eq, assert_ne};

    use super::*;

    #[test]
    fn derivation_is_stable_and_frozen() {
        let first = derive_conversation_group_id("root-session-123");
        let second = derive_conversation_group_id("root-session-123");

        assert_eq!(first.as_ref(), "111fc242-925b-5a7d-826e-2974daad239f");
        assert_eq!(first, second);
    }

    #[test]
    fn different_roots_have_different_groups() {
        assert_ne!(
            derive_conversation_group_id("root-a"),
            derive_conversation_group_id("root-b")
        );
    }
}
