// Modified for Distill by Samuel Fajreldines, 2026.
//! Sampling error types.
//!
//! The canonical error types live in `distill_sampling_types::error`.
//! This module re-exports them and adds `map_sampling_err_to_acp`, which depends on `agent_client_protocol::Error` (a grok-shell dependency).

pub use distill_sampling_types::error::*;

// Clients carry this typed kind from parsing the wire error to choosing the user-facing copy; re-exported so the pager shares the exact type
pub use distill_sampler::SamplingErrorKind;

use agent_client_protocol as acp;

/// ACP error code for rate-limited requests (HTTP 429). Uses the JSON-RPC implementation-defined server error range (-32000 to -32099). Contract: set only for actual HTTP 429 responses from the sampling client.
/// Clients derive user-facing text via [`format_rate_limited_user_message`]. The desktop path (`prompt_complete_fields`) reports the stop reason with no detail.
pub const RATE_LIMITED_ERROR_CODE: i32 = -32003;

/// OAuth / session rate-limit copy (personal plan upgrade path).
pub const RATE_LIMITED_USER_MESSAGE_OAUTH: &str =
    "You\u{2019}ve hit the rate limit for your plan. Upgrade your account or try again later.";

/// API key / team rate-limit copy.
/// Personal grok.com upgrades do not raise API team limits; admins purchase credits or a higher spend-based tier.
/// See https://docs.x.ai/developers/rate-limits#rate-limit-tiers
pub const RATE_LIMITED_USER_MESSAGE_API_KEY: &str = "You\u{2019}ve hit your team\u{2019}s API rate limit. Ask a team admin to purchase more credits for higher limits, or try again later. See https://docs.x.ai/developers/rate-limits#rate-limit-tiers";

/// Well-known free-usage exhaustion code CCP returns on HTTP 429.
/// Matches `prod_util_well_known_errors::SUBSCRIPTION_FREE_USAGE_EXHAUSTED`.
/// sampling-types' `parse_error_bytes` prepends the flat `code` to the flattened message, so this reaches clients embedded in error detail.
pub const FREE_USAGE_EXHAUSTED_ERROR_CODE: &str = "subscription:free-usage-exhausted";

/// User-facing free-usage exhaustion copy (paywall).
/// Promises no reset duration; the backend config drives the quota window.
pub const FREE_USAGE_USER_MESSAGE: &str = "You\u{2019}ve reached your free Distill usage limit for now. Get SuperGrok for much higher limits, or try again later: https://grok.com/supergrok?referrer=Distill";

/// Whether flattened server detail is free-usage-quota exhaustion (paywall), not transient throttling.
/// Sniffs the well-known code embedded by `parse_error_bytes`.
pub fn is_free_usage_exhausted_error(detail: &str) -> bool {
    detail.contains(FREE_USAGE_EXHAUSTED_ERROR_CODE)
}

/// User-facing text for an ACP -32003 rate-limit error. The free-usage code wins first (consumer-only; checked before the API-key rewrite).
/// An API-key caller whose detail pushes the personal SuperGrok upsell gets the team credits copy instead. Otherwise the body is shown after stripping the `API error (status …):` prefix (SamplingError Display).
/// An empty detail falls back to the OAuth or API-key message. Callers that show this in UI should still run their usual sanitizer (scrub/cap).
pub fn format_rate_limited_user_message(
    server_detail: Option<&str>,
    is_api_key_auth: bool,
) -> String {
    // Free-usage sniff works on the prefixed wire string (`contains` the code).
    if server_detail.is_some_and(is_free_usage_exhausted_error) {
        return FREE_USAGE_USER_MESSAGE.to_string();
    }
    if let Some(detail) = server_detail.map(str::trim).filter(|s| !s.is_empty()) {
        let detail = strip_sampling_api_error_prefix(detail);
        if is_api_key_auth && pushes_consumer_subscription_upsell(detail) {
            return RATE_LIMITED_USER_MESSAGE_API_KEY.to_string();
        }
        return detail.to_string();
    }
    if is_api_key_auth {
        RATE_LIMITED_USER_MESSAGE_API_KEY
    } else {
        RATE_LIMITED_USER_MESSAGE_OAUTH
    }
    .to_string()
}

/// Drop `SamplingError::Api`'s Display prefix so users see the IC body, not `API error (status 429 Too Many Requests): …`.
fn strip_sampling_api_error_prefix(detail: &str) -> &str {
    const PREFIX: &str = "API error (status ";
    const SEP: &str = "): ";
    if let Some(rest) = detail.strip_prefix(PREFIX)
        && let Some(idx) = rest.find(SEP)
        && let Some(body) = rest.get(idx + SEP.len()..)
    {
        return body.trim();
    }
    detail.trim()
}

/// IC sometimes reuses OAuth free-tier upsell copy on 429s ("upgrade to a Grok subscription" / grok.com/supergrok).
/// That is wrong for API-key / team auth: higher limits come from credits and spend-based rate-limit tiers, not a personal SuperGrok plan.
fn pushes_consumer_subscription_upsell(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    d.contains("grok.com/supergrok") || d.contains("upgrade to a grok subscription")
}

/// User-facing copy for capacity/overload failures (stream `overloaded_error`, HTTP 529, proxy-wrapped 5xx).
/// See [`SamplingError::is_overloaded`].
pub const OVERLOADED_USER_MESSAGE: &str = "Model is temporarily overloaded. Try again in a moment.";

/// Map a `SamplingError` to an ACP `Error` for client-facing responses.
/// This stays in distill-shell because it depends on `agent_client_protocol::Error`.
pub(crate) fn map_sampling_err_to_acp(err: SamplingError) -> acp::Error {
    use reqwest::StatusCode;
    // Capacity/overload gets the same short copy everywhere
    // Message only, `data` unset: `Display` appends JSON-encoded `data`, and this string is meant for direct display
    if err.is_overloaded() {
        return acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            OVERLOADED_USER_MESSAGE,
        );
    }
    match err {
        SamplingError::Auth { message, .. } => acp::Error::auth_required().data(message),
        SamplingError::InvalidConfiguration(msg) => acp::Error::invalid_params().data(msg),
        SamplingError::MtlsConfiguration(msg) => acp::Error::invalid_params().data(msg),
        SamplingError::Http(e) => {
            acp::Error::internal_error().data(format!("http client init failed: {e}"))
        }
        SamplingError::Serialization(_) => acp::Error::invalid_params().data(err.to_string()),
        SamplingError::Api {
            status, message, ..
        } => match status {
            StatusCode::UNAUTHORIZED => acp::Error::auth_required().data(message),
            // 403 Forbidden is not an auth error: the request was authenticated, but the action is not permitted
            // Examples: content-safety blocks, ZDR-gated operations, remote-settings-blocked users
            // Passing the proxy's message via internal_error keeps the explanation visible without triggering the client's re-auth flow on -32000
            StatusCode::FORBIDDEN => {
                let message = if message.contains("requires a Grok subscription")
                    && crate::agent::auth_method::has_xai_api_key_env()
                {
                    format!(
                        "{message}\n\nYou have an API key set (XAI_API_KEY). \
                         Your cached OAuth session is being used instead. \
                         To use your API key, run `grok logout` or type /logout in the TUI."
                    )
                } else {
                    message
                };
                // 403 is content-safety, never auth: on this setup path it stays `internal_error`, which maps to `server_error`
                acp::Error::internal_error().data(message)
            }
            StatusCode::BAD_REQUEST => acp::Error::invalid_params().data(message),
            StatusCode::NOT_FOUND => acp::Error::resource_not_found(None).data(message),
            StatusCode::PAYLOAD_TOO_LARGE => acp::Error::invalid_params().data(message),
            StatusCode::TOO_MANY_REQUESTS => {
                acp::Error::new(RATE_LIMITED_ERROR_CODE, "Rate limited".to_string()).data(message)
            }
            // Preserve the HTTP status in data so the classifier folds capacity errors (503/529) into `rate_limit`
            _ => acp::Error::internal_error()
                .data(error_data_with_status(message, Some(status.as_u16()))),
        },
        SamplingError::EventStreamError(message) => acp::Error::internal_error().data(message),
        SamplingError::StreamError {
            error_type,
            message,
            ..
        } => acp::Error::internal_error().data(format!("{error_type}: {message}")),
        SamplingError::EmptyResponse { context } => acp::Error::internal_error().data(format!(
            "empty response from model ({}): model={}, had_reasoning={}, finish_reason={}",
            context.reason,
            context.model,
            context.had_reasoning,
            context.finish_reason_str(),
        )),
        SamplingError::MaxTokensTruncation => {
            acp::Error::internal_error().data(terminal_error_data(
                err.to_string(),
                None,
                distill_sampler::SamplingErrorKind::MaxTokensTruncation,
            ))
        }
        SamplingError::IdleTimeout { elapsed_secs } => acp::Error::internal_error().data(format!(
            "No response from model for {elapsed_secs}s — the model may be stuck"
        )),
        // Recovery consumes these inside the sampler's retry loop; a stray terminal one still renders its labels
        SamplingError::DoomLoopDetected { .. } => {
            acp::Error::internal_error().data(err.to_string())
        }
    }
}

pub(crate) fn error_data_with_status(
    message: String,
    http_status: Option<u16>,
) -> serde_json::Value {
    match http_status {
        Some(sc) => serde_json::json!({ "message": message, "http_status": sc }),
        None => serde_json::Value::String(message),
    }
}

pub(crate) fn local_error(code: &str, message: impl Into<String>) -> acp::Error {
    let mut data = serde_json::json!({ "message": message.into() });
    if let Some(obj) = data.as_object_mut() {
        obj.insert(ERROR_CODE_DATA_KEY.to_string(), serde_json::json!(code));
    }
    acp::Error::internal_error().data(data)
}

/// `acp::Error.data` key of the typed terminal-error kind marker (stamped by [`terminal_error_data`]).
/// Snake_case like its shipped `data` siblings (`http_status`); frozen wire format.
/// The notification paths carry the kind under their own keys/fields (see `extensions::notification::PROMPT_COMPLETE_ERROR_KIND_KEY`).
const ERROR_KIND_DATA_KEY: &str = "error_kind";

/// `acp::Error.data` key for a local failure's stable `code`; shared by [`local_error`], [`error_code_from_data`], and `session::persistence::io_error_to_acp`.
pub(crate) const ERROR_CODE_DATA_KEY: &str = "code";

/// `salvage_cause` values stamped on mid-salvage terminal errors and forwarded onto the `shell.turn.length_empty_continuation` event.
/// EMPTY covers every continuation that cannot be salvaged at the cap: nothing visible, or a truncated tool-call tail.
/// The sampler folds both into `MaxTokensTruncation`; OVERFLOW means the request no longer fit.
pub(crate) const SALVAGE_CAUSE_KEY: &str = "salvage_cause";
pub(crate) const SALVAGE_CAUSE_EMPTY: &str = "empty_continuation";
pub(crate) const SALVAGE_CAUSE_OVERFLOW: &str = "context_overflow";

/// Terminal-failure `acp::Error.data`.
/// Only max-tokens truncation opts into the object shape with an `error_kind` marker.
/// Every other kind keeps the legacy string/status shape because old clients render `data` via `Display` and would show the raw JSON object.
pub(crate) fn terminal_error_data(
    message: String,
    http_status: Option<u16>,
    kind: SamplingErrorKind,
) -> serde_json::Value {
    if kind != SamplingErrorKind::MaxTokensTruncation {
        return error_data_with_status(message, http_status);
    }
    let mut data = serde_json::json!({ "message": message });
    if let Some(obj) = data.as_object_mut() {
        obj.insert(
            ERROR_KIND_DATA_KEY.to_string(),
            serde_json::json!(kind.as_ref()),
        );
        if let Some(sc) = http_status {
            obj.insert("http_status".to_string(), serde_json::json!(sc));
        }
    }
    data
}

/// The raw `error_kind` marker string from `acp::Error.data`, unparsed, for readers with their own vocabulary.
/// The pager maps an unknown kind to its `Other`, keeping it immune to text recovery.
pub fn error_kind_str_from_error(err: &acp::Error) -> Option<&str> {
    err.data.as_ref()?.get(ERROR_KIND_DATA_KEY)?.as_str()
}

pub fn error_code_from_data(err: &acp::Error) -> Option<&str> {
    err.data.as_ref()?.get(ERROR_CODE_DATA_KEY)?.as_str()
}

/// Typed view of [`error_kind_str_from_error`] for the shell's own classification, where an unknown kind degrading to `None` (generic) is correct.
pub fn error_kind_from_error(err: &acp::Error) -> Option<SamplingErrorKind> {
    error_kind_str_from_error(err)?.parse().ok()
}

/// Whether a mapped turn error carries the max-tokens truncation marker.
pub(crate) fn is_max_tokens_turn_error(err: &acp::Error) -> bool {
    error_kind_from_error(err) == Some(SamplingErrorKind::MaxTokensTruncation)
}

/// `turn_result.json` stop_reason for a failed turn: "MaxTokens" when the marker is present, else "Error".
/// Matches the success path's `acp::StopReason` names.
pub fn stop_reason_for_turn_error(err: &acp::Error) -> &'static str {
    if is_max_tokens_turn_error(err) {
        "MaxTokens"
    } else {
        "Error"
    }
}

fn error_message_from_data(data: &serde_json::Value) -> serde_json::Value {
    data.get("message").cloned().unwrap_or_else(|| data.clone())
}

/// Internal service names that upstream error bodies echo, rewritten to distinct sentence-friendly backend labels before display. The labels stay distinct so a user paste keeps the failing hop.
/// Shared by shell and pager so the redaction cannot drift; apply via [`rewrite_service_names`] (case-insensitive, no cased variants here). No replacement value may re-match a pattern (pinned by test).
pub const SERVICE_NAME_REWRITES: &[(&str, &str)] = &[
    ("cli-chat-proxy", "build backend"),
    ("cli_chat_proxy", "build backend"),
    ("inference-api", "inference backend"),
    ("inference_api", "inference backend"),
    ("research-api", "research backend"),
    ("research_api", "research backend"),
    ("grok-code-backend", "code backend"),
    ("grok_code_backend", "code backend"),
];

/// Scrub every [`SERVICE_NAME_REWRITES`] entry out of `text`, ASCII-case-insensitively (upstream bodies title-case service names).
/// Each replacement keeps its own casing.
pub fn rewrite_service_names(text: &str) -> String {
    let mut result = text.to_owned();
    for (pattern, replacement) in SERVICE_NAME_REWRITES {
        result = replace_ascii_case_insensitive(&result, pattern, replacement);
    }
    result
}

/// ASCII-case-insensitive `replace`.
/// Indices found on the lowercased copy map 1:1 onto `text`: `to_ascii_lowercase` never changes byte lengths.
fn replace_ascii_case_insensitive(text: &str, pattern: &str, replacement: &str) -> String {
    // An empty pattern would never advance `idx`; fail safe in release too.
    if pattern.is_empty() {
        return text.to_owned();
    }
    let lower_text = text.to_ascii_lowercase();
    let lower_pattern = pattern.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut idx = 0;
    while let Some(pos) = lower_text.get(idx..).and_then(|s| s.find(&lower_pattern)) {
        let start = idx + pos;
        if let Some(chunk) = text.get(idx..start) {
            out.push_str(chunk);
        }
        out.push_str(replacement);
        idx = start + pattern.len();
    }
    if let Some(tail) = text.get(idx..) {
        out.push_str(tail);
    }
    out
}

pub fn error_detail_from_data(data: &serde_json::Value) -> Option<String> {
    if let Some(m) = data.get("message").and_then(|v| v.as_str()) {
        return Some(m.to_owned());
    }
    if let Some(s) = data.as_str() {
        return Some(s.to_owned());
    }
    data.get("detail")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Detail an ACP error carries: `data` via [`error_detail_from_data`], else the JSON-RPC `message`, so foreign shapes classify as the safe `Other`.
pub(crate) fn acp_error_message(err: &acp::Error) -> String {
    err.data
        .as_ref()
        .and_then(error_detail_from_data)
        .unwrap_or_else(|| err.message.clone())
}

pub fn http_status_from_error(err: &acp::Error) -> Option<u16> {
    err.data
        .as_ref()?
        .get("http_status")?
        .as_u64()
        .map(|s| s as u16)
}

const PROMPT_USAGE_DATA_KEY: &str = "promptUsage";

pub fn attach_prompt_usage(
    err: acp::Error,
    usage: Option<crate::extensions::notification::PromptUsage>,
) -> acp::Error {
    let Some(usage) = usage else {
        return err;
    };
    let Ok(usage_val) = serde_json::to_value(&usage) else {
        tracing::warn!(
            "attach_prompt_usage: failed to serialize PromptUsage; leaving error unchanged"
        );
        return err;
    };
    let mut map = match err.data.clone() {
        Some(serde_json::Value::Object(map)) => map,
        Some(serde_json::Value::String(message)) => {
            let mut m = serde_json::Map::new();
            m.insert("message".into(), serde_json::Value::String(message));
            m
        }
        Some(other) => {
            let mut m = serde_json::Map::new();
            m.insert("message".into(), other);
            m
        }
        None => {
            let mut m = serde_json::Map::new();
            m.insert(
                "message".into(),
                serde_json::Value::String(err.message.clone()),
            );
            m
        }
    };
    map.insert(PROMPT_USAGE_DATA_KEY.into(), usage_val);
    err.data(serde_json::Value::Object(map))
}

pub fn prompt_usage_from_error(
    err: &acp::Error,
) -> Option<crate::extensions::notification::PromptUsage> {
    let data = err.data.as_ref()?;
    let raw = data.get(PROMPT_USAGE_DATA_KEY)?;
    serde_json::from_value(raw.clone()).ok()
}

/// Derive `(stop reason, agent result, error kind)` for the turn-end payloads (`prompt_complete`, durable `TurnCompleted`) from a prompt result.
/// Rate-limit errors produce `("rate_limit", null)` so the client shows its own upgrade message; other errors produce `("error", <detail>)`.
/// The error kind ([`error_kind_from_error`]) is `None` for successes and errors without a kind marker.
pub(crate) fn prompt_complete_fields(
    result: &std::result::Result<acp::StopReason, acp::Error>,
) -> (
    serde_json::Value,
    serde_json::Value,
    Option<SamplingErrorKind>,
) {
    match result {
        Ok(reason) => (serde_json::json!(*reason), serde_json::Value::Null, None),
        Err(err) => {
            let is_rate_limit = i32::from(err.code) == RATE_LIMITED_ERROR_CODE;
            let stop = if is_rate_limit { "rate_limit" } else { "error" };
            let result = if is_rate_limit {
                serde_json::Value::Null
            } else {
                err.data
                    .as_ref()
                    .map(error_message_from_data)
                    .unwrap_or_else(|| serde_json::Value::String(err.message.clone()))
            };
            (serde_json::json!(stop), result, error_kind_from_error(err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn rewrite_service_names_is_ascii_case_insensitive() {
        // Idempotency: no replacement value may re-match any pattern.
        for (_, replacement) in SERVICE_NAME_REWRITES {
            for (pattern, _) in SERVICE_NAME_REWRITES {
                assert!(
                    !replacement
                        .to_ascii_lowercase()
                        .contains(&pattern.to_ascii_lowercase()),
                    "value {replacement:?} re-matches pattern {pattern:?}"
                );
            }
        }
        // Derive cased variants from the table so fixtures never respell a name.
        for (pattern, replacement) in SERVICE_NAME_REWRITES {
            let upper = pattern.to_ascii_uppercase();
            let title: String = pattern
                .split_inclusive(['-', '_'])
                .map(|seg| {
                    let mut chars = seg.chars();
                    chars
                        .next()
                        .map(|f| f.to_ascii_uppercase().to_string() + chars.as_str())
                        .unwrap_or_default()
                })
                .collect();
            for variant in [pattern.to_string(), upper, title] {
                let out = rewrite_service_names(&format!("error from {variant} upstream"));
                assert_eq!(
                    out,
                    format!("error from {replacement} upstream"),
                    "variant {variant:?} must scrub to the replacement's own casing"
                );
            }
        }
    }

    #[test]
    fn attach_prompt_usage_preserves_error_kind_and_round_trips() {
        let mut ledger = distill_chat_state::UsageLedger::default();
        ledger.record_main_loop_call(
            "m",
            &distill_sampling_types::TokenUsage {
                prompt_tokens: 3,
                completion_tokens: 1,
                total_tokens: 4,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            },
            None,
            Some(10),
        );
        let usage = crate::extensions::notification::PromptUsage::from(&ledger);
        let err = attach_prompt_usage(
            acp::Error::internal_error().data(terminal_error_data(
                "truncated".into(),
                None,
                distill_sampler::SamplingErrorKind::MaxTokensTruncation,
            )),
            Some(usage.clone()),
        );
        assert_eq!(stop_reason_for_turn_error(&err), "MaxTokens");
        let back = prompt_usage_from_error(&err).expect("usage attached");
        assert_eq!(back.totals.input_tokens, 3);
        assert_eq!(back.num_turns, 1);
    }

    #[test]
    fn attach_prompt_usage_keeps_string_message_readable() {
        let usage = crate::extensions::notification::PromptUsage {
            totals: Default::default(),
            model_usage: Default::default(),
            num_turns: 1,
            usage_is_incomplete: false,
            effort_usage: Vec::new(),
            jev_calls: 0,
            reasoning_consults: Vec::new(),
        };
        let free = "subscription:free-usage-exhausted quota hit";
        let err = attach_prompt_usage(
            acp::Error::new(RATE_LIMITED_ERROR_CODE, "Rate limited").data(free),
            Some(usage),
        );
        let msg = err
            .data
            .as_ref()
            .and_then(|d| {
                d.as_str()
                    .or_else(|| d.get("message").and_then(|m| m.as_str()))
            })
            .unwrap_or("");
        assert!(msg.contains("subscription:free-usage-exhausted"));
        assert!(prompt_usage_from_error(&err).is_some());
        assert!(!err.data.as_ref().unwrap().is_string());
    }

    #[test]
    fn error_detail_from_data_reads_message_field() {
        let data = error_data_with_status("upstream unavailable".into(), Some(503));
        assert_eq!(
            error_detail_from_data(&data).as_deref(),
            Some("upstream unavailable")
        );
    }

    #[test]
    fn rate_limited_fallback_oauth_vs_api_key() {
        assert_eq!(
            format_rate_limited_user_message(None, false),
            RATE_LIMITED_USER_MESSAGE_OAUTH
        );
        assert_eq!(
            format_rate_limited_user_message(None, true),
            RATE_LIMITED_USER_MESSAGE_API_KEY
        );
        assert!(RATE_LIMITED_USER_MESSAGE_OAUTH.contains("Upgrade your account"));
        assert!(RATE_LIMITED_USER_MESSAGE_API_KEY.contains("team"));
        assert!(RATE_LIMITED_USER_MESSAGE_API_KEY.contains("credits"));
        assert!(
            RATE_LIMITED_USER_MESSAGE_API_KEY
                .contains("https://docs.x.ai/developers/rate-limits#rate-limit-tiers")
        );
        assert!(!RATE_LIMITED_USER_MESSAGE_API_KEY.contains("Upgrade your account"));
    }

    #[test]
    fn format_rate_limited_surfaces_nonempty_server_detail() {
        let body = "The service is temporarily at capacity. Please retry your request shortly.";
        // Production detail is SamplingError::Api Display (prefixed).
        let wire = format!("API error (status 429 Too Many Requests): {body}");
        assert_eq!(format_rate_limited_user_message(Some(&wire), false), body);
        assert_eq!(format_rate_limited_user_message(Some(&wire), true), body);

        // Team console rate-limit copy has no personal SuperGrok upsell; it passes through as-is
        let team = "resource-exhausted: Too many requests for team abc. See https://console.x.ai/team/default/rate-limits.";
        let team_wire = format!("API error (status 429 Too Many Requests): {team}");
        assert_eq!(
            format_rate_limited_user_message(Some(&team_wire), true),
            team
        );
        assert_eq!(
            format_rate_limited_user_message(Some("slow down"), false),
            "slow down"
        );
    }

    #[test]
    fn format_rate_limited_api_key_rewrites_consumer_subscription_upsell() {
        let body = "Some resource has been exhausted: You are sending requests too quickly. \
             Please slow down, or upgrade to a Grok subscription for higher limits: \
             https://grok.com/supergrok";
        let wire = format!("API error (status 429 Too Many Requests): {body}");
        // OAuth keeps the IC body (personal plan upgrade is correct).
        assert_eq!(format_rate_limited_user_message(Some(&wire), false), body);
        // API key must not push grok.com SuperGrok; it gets the team credits / rate-limit tiers copy
        assert_eq!(
            format_rate_limited_user_message(Some(&wire), true),
            RATE_LIMITED_USER_MESSAGE_API_KEY
        );
    }

    #[test]
    fn format_rate_limited_strips_api_error_display_prefix() {
        let body = "The service is temporarily at capacity.";
        let wire = format!("API error (status 429 Too Many Requests): {body}");
        assert_eq!(format_rate_limited_user_message(Some(&wire), false), body);
        assert!(!format_rate_limited_user_message(Some(&wire), false).contains("API error"));
    }

    #[test]
    fn is_free_usage_exhausted_error_sniffs_well_known_code() {
        assert!(is_free_usage_exhausted_error(
            "subscription:free-usage-exhausted: You have used all your free usage."
        ));
        assert!(is_free_usage_exhausted_error(
            "API error (status 429): subscription:free-usage-exhausted quota hit"
        ));
        assert!(!is_free_usage_exhausted_error("throttled"));
        assert!(!is_free_usage_exhausted_error(
            "The service is temporarily at capacity."
        ));
    }

    #[test]
    fn format_rate_limited_free_usage_uses_paywall_copy() {
        let wire = "API error (status 429 Too Many Requests): \
            subscription:free-usage-exhausted: You have used all your free usage.";
        assert_eq!(
            format_rate_limited_user_message(Some(wire), false),
            FREE_USAGE_USER_MESSAGE
        );
        // Free-usage code is consumer-only; still wins for API-key callers.
        assert_eq!(
            format_rate_limited_user_message(Some(wire), true),
            FREE_USAGE_USER_MESSAGE
        );
    }

    #[test]
    fn format_rate_limited_empty_detail_uses_auth_aware_fallback() {
        assert_eq!(
            format_rate_limited_user_message(None, false),
            RATE_LIMITED_USER_MESSAGE_OAUTH
        );
        assert_eq!(
            format_rate_limited_user_message(Some(""), false),
            RATE_LIMITED_USER_MESSAGE_OAUTH
        );
        assert_eq!(
            format_rate_limited_user_message(None, true),
            RATE_LIMITED_USER_MESSAGE_API_KEY
        );
        assert_eq!(
            format_rate_limited_user_message(Some("   "), true),
            RATE_LIMITED_USER_MESSAGE_API_KEY
        );
    }

    #[test]
    fn overload_maps_to_display_message_without_data() {
        let err = SamplingError::StreamError {
            error_type: "overloaded_error".into(),
            message: "Overloaded".into(),
            code: None,
        };
        let acp_err = map_sampling_err_to_acp(err);
        assert_eq!(acp_err.code, acp::ErrorCode::InternalError);
        assert_eq!(acp_err.message, OVERLOADED_USER_MESSAGE);
        // Display appends JSON-encoded `data`; direct-display copy must not carry any
        assert_eq!(acp_err.data, None);

        let err_529 = SamplingError::Api {
            status: StatusCode::from_u16(529).expect("valid status"),
            message: "capacity".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
        };
        let acp_529 = map_sampling_err_to_acp(err_529);
        assert_eq!(acp_529.message, OVERLOADED_USER_MESSAGE);
        assert_eq!(acp_529.data, None);
    }

    #[test]
    fn rate_limit_error_uses_dedicated_code() {
        let err = SamplingError::Api {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "Rate limit exceeded".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
        };
        let acp_err = map_sampling_err_to_acp(err);
        assert_eq!(acp_err.code, acp::ErrorCode::from(RATE_LIMITED_ERROR_CODE));
        assert_eq!(acp_err.message, "Rate limited");
        assert_eq!(
            acp_err.data,
            Some(serde_json::Value::String("Rate limit exceeded".into()))
        );
    }

    #[test]
    fn rate_limit_mapping_is_stable_with_retry_after() {
        let err = SamplingError::Api {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "Rate limit exceeded".into(),
            model_metadata: None,
            retry_after_secs: Some(60),
            should_retry: None,
            error_code: None,
        };
        assert_eq!(err.retry_after(), Some(60));
        let acp_err = map_sampling_err_to_acp(err);
        assert_eq!(acp_err.code, acp::ErrorCode::from(RATE_LIMITED_ERROR_CODE));
        assert_eq!(acp_err.message, "Rate limited");
    }

    #[test]
    fn rate_limit_code_differs_from_internal_error() {
        let rate_err = SamplingError::Api {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "limited".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
        };
        let server_err = SamplingError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "oops".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
        };
        let rate_acp = map_sampling_err_to_acp(rate_err);
        let server_acp = map_sampling_err_to_acp(server_err);

        assert_eq!(rate_acp.code, acp::ErrorCode::from(RATE_LIMITED_ERROR_CODE));
        assert_ne!(rate_acp.code, server_acp.code);
        assert_eq!(server_acp.code, acp::Error::internal_error().code);
    }

    #[test]
    fn service_unavailable_retains_http_status_for_classification() {
        let err = SamplingError::Api {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "at capacity".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
        };
        let acp_err = map_sampling_err_to_acp(err);
        assert_eq!(acp_err.code, acp::Error::internal_error().code);
        assert_eq!(http_status_from_error(&acp_err), Some(503));
    }

    #[test]
    fn auth_errors_map_to_auth_required() {
        let err = SamplingError::Api {
            status: StatusCode::UNAUTHORIZED,
            message: "bad token".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
        };
        let acp_err = map_sampling_err_to_acp(err);
        assert_eq!(acp_err.code, acp::Error::auth_required().code);
    }

    /// Regression test: 403 Forbidden must not map to auth_required. The cli-chat-proxy returns 403 for policy denials unrelated to the caller's credentials.
    /// Examples: content-safety blocks like SAFETY_CHECK_TYPE_DATA_LEAKAGE, ZDR-gated operations, remote settings blocks.
    /// Mapping these to auth_required makes the desktop app tear down the session and start silent re-auth on -32000. That can race with invalid_grant_threshold to wipe auth.json.
    #[test]
    fn forbidden_does_not_map_to_auth_required() {
        let err = SamplingError::Api {
            status: StatusCode::FORBIDDEN,
            message:
                "Content violates usage guidelines. Failed check: SAFETY_CHECK_TYPE_DATA_LEAKAGE"
                    .into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
        };
        let acp_err = map_sampling_err_to_acp(err);
        assert_ne!(
            acp_err.code,
            acp::Error::auth_required().code,
            "403 Forbidden must not be surfaced as auth_required"
        );
        assert_eq!(
            acp_err.data,
            Some(serde_json::Value::String(
                "Content violates usage guidelines. Failed check: SAFETY_CHECK_TYPE_DATA_LEAKAGE"
                    .into()
            ))
        );
    }

    /// Helper: run a closure with XAI_API_KEY temporarily set (or cleared).
    /// Cleans up even if the closure panics.
    fn with_api_key_env<F: FnOnce()>(key: Option<&str>, f: F) {
        let prev = std::env::var("XAI_API_KEY").ok();
        let prev_legacy = std::env::var("GROK_CODE_XAI_API_KEY").ok();
        // SAFETY: serial_test ensures no concurrent env mutation.
        unsafe {
            std::env::remove_var("XAI_API_KEY");
            std::env::remove_var("GROK_CODE_XAI_API_KEY");
            if let Some(k) = key {
                std::env::set_var("XAI_API_KEY", k);
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        // Restore original state.
        unsafe {
            std::env::remove_var("XAI_API_KEY");
            std::env::remove_var("GROK_CODE_XAI_API_KEY");
            if let Some(v) = prev {
                std::env::set_var("XAI_API_KEY", v);
            }
            if let Some(v) = prev_legacy {
                std::env::set_var("GROK_CODE_XAI_API_KEY", v);
            }
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    #[serial_test::serial]
    fn forbidden_subscription_error_includes_api_key_hint_when_env_set() {
        with_api_key_env(Some("xai-test"), || {
            let err = SamplingError::Api {
                status: StatusCode::FORBIDDEN,
                message: "The model 'Distill' requires a Grok subscription.".into(),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
                error_code: None,
            };
            let acp_err = map_sampling_err_to_acp(err);
            let data = acp_err.data.unwrap();
            let msg = data.as_str().unwrap();
            assert!(
                msg.contains("grok logout"),
                "should suggest grok logout when API key is available: {msg}"
            );
            assert!(
                msg.contains("/logout"),
                "should mention /logout TUI command: {msg}"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn forbidden_subscription_error_no_hint_without_api_key() {
        with_api_key_env(None, || {
            let err = SamplingError::Api {
                status: StatusCode::FORBIDDEN,
                message: "The model 'Distill' requires a Grok subscription.".into(),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
                error_code: None,
            };
            let acp_err = map_sampling_err_to_acp(err);
            let data = acp_err.data.unwrap();
            let msg = data.as_str().unwrap();
            assert!(
                !msg.contains("grok logout"),
                "should NOT suggest logout when no API key is available: {msg}"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn forbidden_non_subscription_error_no_hint() {
        with_api_key_env(Some("xai-test"), || {
            let err = SamplingError::Api {
                status: StatusCode::FORBIDDEN,
                message: "Content violates usage guidelines.".into(),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
                error_code: None,
            };
            let acp_err = map_sampling_err_to_acp(err);
            let data = acp_err.data.unwrap();
            let msg = data.as_str().unwrap();
            assert!(
                !msg.contains("grok logout"),
                "should NOT suggest logout for non-subscription 403: {msg}"
            );
        });
    }

    #[test]
    fn prompt_complete_fields_ok_passes_through_stop_reason() {
        let result: std::result::Result<acp::StopReason, acp::Error> = Ok(acp::StopReason::EndTurn);
        let (stop, agent_result, error_kind) = prompt_complete_fields(&result);
        assert_eq!(stop, serde_json::json!("end_turn"));
        assert_eq!(agent_result, serde_json::Value::Null);
        assert_eq!(error_kind, None);
    }

    #[test]
    fn prompt_complete_fields_rate_limit_omits_detail() {
        let err = acp::Error::new(RATE_LIMITED_ERROR_CODE, "Rate limited".to_string())
            .data("Rate limit exceeded");
        let result = Err(err);
        let (stop, agent_result, error_kind) = prompt_complete_fields(&result);
        assert_eq!(stop, serde_json::json!("rate_limit"));
        assert_eq!(agent_result, serde_json::Value::Null);
        assert_eq!(error_kind, None);
    }

    #[test]
    fn prompt_complete_fields_generic_error_includes_detail() {
        let err = acp::Error::internal_error().data("connection reset");
        let result = Err(err);
        let (stop, agent_result, error_kind) = prompt_complete_fields(&result);
        assert_eq!(stop, serde_json::json!("error"));
        assert_eq!(
            agent_result,
            serde_json::Value::String("connection reset".into())
        );
        assert_eq!(
            error_kind, None,
            "errors without a kind marker carry no errorKind"
        );
    }

    #[test]
    fn prompt_complete_fields_error_without_data_falls_back_to_message() {
        let err = acp::Error::new(-32000, "something broke".to_string());
        assert!(err.data.is_none());
        let result = Err(err);
        let (stop, agent_result, error_kind) = prompt_complete_fields(&result);
        assert_eq!(stop, serde_json::json!("error"));
        assert_eq!(
            agent_result,
            serde_json::Value::String("something broke".into())
        );
        assert_eq!(error_kind, None);
    }

    #[test]
    fn error_kind_from_error_reads_typed_marker_only() {
        let truncation = map_sampling_err_to_acp(SamplingError::MaxTokensTruncation);
        assert_eq!(
            error_kind_from_error(&truncation),
            Some(SamplingErrorKind::MaxTokensTruncation)
        );
        // No data, string data, and object data without the marker all yield None.
        assert_eq!(error_kind_from_error(&acp::Error::internal_error()), None);
        assert_eq!(
            error_kind_from_error(&acp::Error::internal_error().data("boom")),
            None
        );
        let with_status = acp::Error::internal_error()
            .data(error_data_with_status("bad gateway".into(), Some(502)));
        assert_eq!(error_kind_from_error(&with_status), None);
    }

    #[test]
    fn http_status_from_error_extracts_status() {
        let err = acp::Error::internal_error()
            .data(error_data_with_status("bad token".into(), Some(401)));
        assert_eq!(http_status_from_error(&err), Some(401));
    }

    /// The typed max-tokens kind round-trips through `acp::Error.data` to the uploaded stop_reason.
    #[test]
    fn stop_reason_for_turn_error_distinguishes_max_tokens() {
        let err = map_sampling_err_to_acp(SamplingError::MaxTokensTruncation);
        assert_eq!(stop_reason_for_turn_error(&err), "MaxTokens");
        assert_eq!(
            stop_reason_for_turn_error(&acp::Error::internal_error()),
            "Error"
        );
    }

    #[test]
    fn prompt_complete_fields_extracts_message_from_status_data() {
        let err = acp::Error::internal_error()
            .data(error_data_with_status("model not found".into(), Some(404)));
        let result = Err(err);
        let (stop, agent_result, error_kind) = prompt_complete_fields(&result);
        assert_eq!(stop, serde_json::json!("error"));
        assert_eq!(
            agent_result,
            serde_json::Value::String("model not found".into())
        );
        assert_eq!(error_kind, None);
    }
}
