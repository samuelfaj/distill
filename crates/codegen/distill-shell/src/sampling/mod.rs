// Modified for Distill by Samuel Fajreldines, 2026.
pub mod conversation;
pub mod error;
#[cfg(test)]
mod real_api_cache_tests;
pub mod types;

// `Client` is the legacy alias used throughout the shell; it points at the sampler crate's `SamplingClient`
// The two have identical method sets, so call sites compile unchanged
pub use self::conversation::*;
pub use self::error::{ResponseModelMetadata, Result, SamplingError};
pub use self::types::*;
pub use distill_sampler::ApiBackend;
pub use distill_sampler::SamplingClient as Client;

// Re-export async-openai Responses API types under `rs` namespace
pub use async_openai::types::responses as rs;

// --------------------------------------------------------------------------- distill-sampler re-exports --------------------------------------------------------------------------- The actual streaming / retry / HTTP-client logic lives in the `distill-sampler` crate
// These re-exports keep `crate::sampling::{SamplerHandle, SamplerConfig, ...}` paths working for callers not yet ported to `distill_sampler::*`
// There is no shell-side `sampling::client::Config` composite anymore; `MvpAgent` holds session-snapshot state in a `RefCell<SamplerConfig>`
pub use distill_sampler::{
    ConversationGroupId, InferenceLatencyStats, OriginClientInfo, RequestId, SamplerActor,
    SamplerConfig, SamplerHandle, SamplingChannel, SamplingClient, SamplingErrorInfo,
    SamplingErrorKind, SamplingEvent,
};

const CONVERSATION_GROUP_NAMESPACE: &str = "xai:Distill:conversation-group:";

/// Derive the stable group shared by a root session and every descendant session.
pub(crate) fn derive_conversation_group_id(root_session_id: &str) -> ConversationGroupId {
    let namespace_input = format!("{CONVERSATION_GROUP_NAMESPACE}{root_session_id}");
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, namespace_input.as_bytes())
        .to_string()
        .into()
}

/// Responses `prompt_cache_key` for a subagent child so siblings share one cache route.
/// A verbatim fork shares the parent's prefix and routes with the parent session id; a fresh child routes by `{group}:{subagent_type}`.
/// `None` keeps the default (the child's own session id).
pub(crate) fn subagent_prompt_cache_key(
    is_subagent: bool,
    verbatim_fork: bool,
    parent_session_id: Option<&str>,
    subagent_type: Option<&str>,
    group_id: Option<&ConversationGroupId>,
) -> Option<String> {
    if !is_subagent {
        return None;
    }
    if verbatim_fork {
        return parent_session_id.map(str::to_owned);
    }
    Some(format!("{}:{}", group_id?.as_ref(), subagent_type?))
}

/// The routing key of a session's main call: the subagent key when one applies, else the session id.
/// The main call sends it as `prompt_cache_key`, and so must every request that replays the main prefix
/// (compaction pass 1, recap, `/btw`), or that request lands on a cold route; their conv id is [`conv_id_for`].
pub(crate) fn main_cache_key(
    session_id: &str,
    is_subagent: bool,
    verbatim_fork: bool,
    parent_session_id: Option<&str>,
    subagent_type: Option<&str>,
    group_id: Option<&ConversationGroupId>,
) -> String {
    subagent_prompt_cache_key(
        is_subagent,
        verbatim_fork,
        parent_session_id,
        subagent_type,
        group_id,
    )
    .unwrap_or_else(|| session_id.to_owned())
}

/// Stable routing key for a session's call whose prompt is not the main one (goal eval, compaction pass 2, memory, classifiers).
/// It must not be the main key: a provider keeps one cache entry per key, so a different prompt there evicts the main prefix.
/// It is stable per purpose so repeated calls with the same system prompt read their own cached prefix.
pub(crate) fn purpose_cache_key(session_id: &str, purpose: &str) -> String {
    format!("{session_id}:{purpose}")
}

/// Conversation id for a request routed on `cache_key`. A purpose call is its own conversation, so its key is its id.
/// A replay of the main prefix keeps the session's own id, as the main call does: Codex derives `thread-id` and
/// the per-turn state from it, and sibling subagents must not share one thread.
pub(crate) fn conv_id_for(session_id: &str, cache_key: &str) -> String {
    if cache_key.starts_with(&format!("{session_id}:")) {
        cache_key.to_owned()
    } else {
        session_id.to_owned()
    }
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

    #[test]
    fn subagent_cache_key_routes_siblings_and_forks() {
        let group = derive_conversation_group_id("root");
        let fresh_a =
            subagent_prompt_cache_key(true, false, Some("parent"), Some("explore"), Some(&group));
        let fresh_b =
            subagent_prompt_cache_key(true, false, Some("parent"), Some("explore"), Some(&group));
        assert_eq!(fresh_a, Some(format!("{}:explore", group.as_ref())));
        assert_eq!(fresh_a, fresh_b);
        assert_ne!(
            fresh_a,
            subagent_prompt_cache_key(true, false, Some("parent"), Some("plan"), Some(&group))
        );
        assert_eq!(
            subagent_prompt_cache_key(true, true, Some("parent"), Some("explore"), Some(&group)),
            Some("parent".to_string())
        );
        assert_eq!(
            subagent_prompt_cache_key(false, false, None, None, Some(&group)),
            None
        );
    }

    /// Compaction and recap replay the main prefix, so they must route where the main call routes:
    /// a verbatim fork on its parent, a fresh child on its sibling route, a root on its own id.
    #[test]
    fn main_cache_key_is_where_the_main_prefix_is_cached() {
        let group = derive_conversation_group_id("root");
        assert_eq!(
            main_cache_key("root", false, false, None, None, Some(&group)),
            "root"
        );
        assert_eq!(
            main_cache_key(
                "child",
                true,
                true,
                Some("parent"),
                Some("explore"),
                Some(&group)
            ),
            "parent",
            "a verbatim fork's prefix is the parent's, so its replays read the parent's cache"
        );
        assert_eq!(
            main_cache_key(
                "child",
                true,
                false,
                Some("parent"),
                Some("explore"),
                Some(&group)
            ),
            format!("{}:explore", group.as_ref())
        );
        // No group yet: the child routes on its own id, never on an empty key.
        assert_eq!(
            main_cache_key("child", true, false, Some("parent"), Some("explore"), None),
            "child"
        );
    }

    /// A call with a different prompt must not overwrite the main call's cache entry,
    /// and its own prefix stays warm only if the key is the same every time.
    #[test]
    fn a_main_replay_keeps_the_sessions_thread_and_a_purpose_call_is_its_own() {
        assert_eq!(
            conv_id_for("child", "parent"),
            "child",
            "a fork's replay routes on the parent's key but keeps its own thread, as its main call does"
        );
        assert_eq!(conv_id_for("child", "group-1:explore"), "child");
        assert_eq!(
            conv_id_for("s1", &purpose_cache_key("s1", "goal-eval")),
            "s1:goal-eval",
            "a purpose call is a separate conversation that must not evict the main thread"
        );
    }

    #[test]
    fn purpose_cache_key_is_stable_and_never_the_main_key() {
        let main = main_cache_key("s1", false, false, None, None, None);
        let goal = purpose_cache_key("s1", "goal-eval");
        assert_ne!(goal, main);
        assert_eq!(goal, purpose_cache_key("s1", "goal-eval"));
        assert_ne!(goal, purpose_cache_key("s1", "compact-pass2"));
        assert_ne!(goal, purpose_cache_key("s2", "goal-eval"));
    }
}
