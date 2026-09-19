// Modified for Distill by Samuel Fajreldines, 2026.
use super::{PromptResponseMetaArgs, build_prompt_response_meta};
use distill_sampling_types::TokenUsage;

/// Baseline args with no usage, cancellation, or structured output.
fn args<'a>(
    session_id: &'a str,
    prompt_id: &'a str,
    total_tokens: u64,
    model_id: &'a str,
) -> PromptResponseMetaArgs<'a> {
    PromptResponseMetaArgs {
        session_id,
        tool_overrides: None,
        prompt_id,
        total_tokens,
        model_id,
        last_turn_usage: None,
        prompt_usage: None,
        cancellation_category: None,
        cancellation_context: None,
        cancel_trigger: None,
        structured_output: None,
        completion_kind: None,
    }
}

#[test]
fn includes_baseline_keys_without_usage() {
    let meta = build_prompt_response_meta(args("sess-1", "prompt-1", 42_000, "grok-4.5"));
    assert_eq!(
        meta.get("sessionId").and_then(|v| v.as_str()),
        Some("sess-1")
    );
    assert_eq!(
        meta.get("requestId").and_then(|v| v.as_str()),
        Some("prompt-1")
    );
    assert_eq!(
        meta.get("promptId").and_then(|v| v.as_str()),
        Some("prompt-1")
    );
    assert_eq!(
        meta.get("totalTokens").and_then(|v| v.as_u64()),
        Some(42_000)
    );
    assert_eq!(
        meta.get("modelId").and_then(|v| v.as_str()),
        Some("grok-4.5")
    );
    // No per-turn keys when usage is absent.
    assert!(meta.get("inputTokens").is_none());
    assert!(meta.get("outputTokens").is_none());
    assert!(meta.get("cachedReadTokens").is_none());
}

#[test]
fn enriches_meta_with_camelcase_token_keys() {
    let usage = TokenUsage {
        prompt_tokens: 1500,
        completion_tokens: 200,
        total_tokens: 1700,
        reasoning_tokens: 75,
        cached_prompt_tokens: 1000,
        cache_creation_prompt_tokens: 0,
    };
    let meta = build_prompt_response_meta(PromptResponseMetaArgs {
        last_turn_usage: Some(&usage),
        ..args("sess-1", "prompt-1", 1_700, "grok-4.5")
    });
    // Bot's _META_TOKEN_KEY_MAP expects exactly these camelCase keys.
    assert_eq!(meta.get("inputTokens").and_then(|v| v.as_u64()), Some(1500));
    assert_eq!(meta.get("outputTokens").and_then(|v| v.as_u64()), Some(200));
    assert_eq!(
        meta.get("cachedReadTokens").and_then(|v| v.as_u64()),
        Some(1000)
    );
    // Reasoning tokens are carried through so diagnostics can read them
    assert_eq!(
        meta.get("reasoningTokens").and_then(|v| v.as_u64()),
        Some(75)
    );
}

#[test]
fn preserves_zero_token_values() {
    // Responses API hits with no cache return a cached_prompt_tokens of 0
    // The key is still emitted as 0 so the bot can tell "no cache hit" from "no usage data"
    // The bot's _merge_meta_usage requires the key to be present and integer-typed
    let usage = TokenUsage {
        prompt_tokens: 100,
        completion_tokens: 10,
        total_tokens: 110,
        reasoning_tokens: 0,
        cached_prompt_tokens: 0,
        cache_creation_prompt_tokens: 0,
    };
    let meta = build_prompt_response_meta(PromptResponseMetaArgs {
        last_turn_usage: Some(&usage),
        ..args("s", "p", 110, "m")
    });
    assert_eq!(
        meta.get("cachedReadTokens").and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        meta.get("reasoningTokens").and_then(|v| v.as_u64()),
        Some(0)
    );
}

#[test]
fn usage_object_lands_on_meta() {
    let mut ledger = distill_chat_state::UsageLedger::default();
    ledger.record_main_loop_call(
        "m",
        &TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 10,
            total_tokens: 999_999,
            reasoning_tokens: 0,
            cached_prompt_tokens: 0,
            cache_creation_prompt_tokens: 0,
        },
        None,
        None,
    );
    let meta = build_prompt_response_meta(PromptResponseMetaArgs {
        prompt_usage: Some(crate::extensions::notification::PromptUsage::from(&ledger)),
        ..args("s", "p", 110, "m")
    });
    assert_eq!(
        meta.get("usage")
            .and_then(|u| u.get("totalTokens"))
            .and_then(|v| v.as_u64()),
        Some(110)
    );
    assert_eq!(
        meta.get("usage")
            .and_then(|u| u.get("modelUsage"))
            .and_then(|m| m.get("m"))
            .and_then(|m| m.get("inputTokens"))
            .and_then(|v| v.as_u64()),
        Some(100)
    );
    assert!(
        build_prompt_response_meta(args("s", "p", 0, "m"))
            .get("usage")
            .is_none()
    );
}

#[test]
fn cancel_trigger_lands_as_camelcase_meta_key() {
    // When send-now cancels a turn, the PromptResponse `_meta` carries `cancelTrigger: "send_now"`
    let meta = build_prompt_response_meta(PromptResponseMetaArgs {
        cancel_trigger: Some("send_now".to_string()),
        ..args("s", "p", 0, "m")
    });
    assert_eq!(
        meta.get("cancelTrigger").and_then(|v| v.as_str()),
        Some("send_now")
    );

    // When nothing was cancelled, the key must not appear
    let none = build_prompt_response_meta(args("s", "p", 0, "m"));
    assert!(none.get("cancelTrigger").is_none());
}

#[test]
fn tool_overrides_land_as_camelcase_meta_key() {
    let overrides = distill_sampling_types::ToolOverrides {
        x_search: Some(distill_sampling_types::XSearchOptions {
            date_bound: Some(
                distill_sampling_types::SearchDateBound::new(None, Some("2024-03-15".to_string()))
                    .unwrap(),
            ),
        }),
        web_search: None,
    };
    let meta = build_prompt_response_meta(PromptResponseMetaArgs {
        tool_overrides: Some(overrides),
        ..args("s", "p", 0, "m")
    });
    assert_eq!(
        meta.get("toolOverrides")
            .and_then(|t| t.get("xSearch"))
            .and_then(|x| x.get("dateBound"))
            .and_then(|d| d.get("toDate"))
            .and_then(|v| v.as_str()),
        Some("2024-03-15")
    );
    assert!(
        meta.get("toolOverrides")
            .and_then(|t| t.get("webSearch"))
            .is_none()
    );

    let none = build_prompt_response_meta(args("s", "p", 0, "m"));
    assert!(none.get("toolOverrides").is_none());
}

#[test]
fn structured_output_maps_to_camelcase_meta_keys() {
    // Success carries the validated value under `structuredOutput`; no error key.
    let ok = build_prompt_response_meta(PromptResponseMetaArgs {
        structured_output: Some(Ok(serde_json::json!({"name": "ada"}))),
        ..args("s", "p", 0, "m")
    });
    assert_eq!(
        ok.get("structuredOutput")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str()),
        Some("ada")
    );
    assert!(ok.get("structuredOutputError").is_none());

    // Failure carries the message under `structuredOutputError`; no value key.
    let err = build_prompt_response_meta(PromptResponseMetaArgs {
        structured_output: Some(Err("output does not match the required schema".to_string())),
        ..args("s", "p", 0, "m")
    });
    assert_eq!(
        err.get("structuredOutputError").and_then(|v| v.as_str()),
        Some("output does not match the required schema")
    );
    assert!(err.get("structuredOutput").is_none());

    // When no schema was requested, neither key is present
    let none = build_prompt_response_meta(args("s", "p", 0, "m"));
    assert!(none.get("structuredOutput").is_none());
    assert!(none.get("structuredOutputError").is_none());
}

#[test]
fn removed_from_queue_stamps_completion_kind() {
    let none = build_prompt_response_meta(args("s", "p", 0, "m"));
    assert!(none.get("completionKind").is_none());
    let meta = build_prompt_response_meta(PromptResponseMetaArgs {
        completion_kind: Some(crate::session::commands::REMOVED_FROM_QUEUE_KIND.to_string()),
        ..args("s", "p", 0, "m")
    });
    assert_eq!(
        meta.get("completionKind").and_then(|v| v.as_str()),
        Some("removedFromQueue")
    );
}
