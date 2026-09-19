use super::*;

/// Max follow-up chips kept from a single (server-controlled) notification.
pub(super) const MAX_FOLLOW_UPS: usize = 6;

/// Max chars kept per (server-controlled) suggestion label.
pub(super) const MAX_FOLLOW_UP_LABEL: usize = 256;

/// Max `response_id` length accepted, so an oversized server id can't bloat the retained `follow_up_seen` ring.
/// Longer ids are rejected rather than truncated, because truncation could collide ids.
pub(super) const MAX_RESPONSE_ID_LEN: usize = 128;

/// Deserialize shape of the `x.ai/follow_ups` params emitted by the shell translator: `{ response_id, suggestions: [{ label, .. }] }`.
/// The keys are prost-derived snake_case, not camelCase like most other pager notification payloads, so this struct must match snake_case verbatim.
/// Every field defaults so a malformed or partial payload degrades to "no chips" instead of erroring.
#[derive(serde::Deserialize)]
pub(super) struct FollowUpsParams {
    #[serde(default)]
    response_id: String,
    #[serde(default)]
    suggestions: Vec<FollowUpSuggestionParam>,
    /// Turn identity stamped by the shell, the same `promptId` it puts on every `session/update`, hence the camelCase rename.
    /// Older shells omit it; when present it makes dedup deterministic when a viewer adopts a turn (see [`AgentView::apply_follow_ups_with_prompt`]).
    #[serde(default, rename = "promptId")]
    prompt_id: Option<String>,
    /// Carries the reserved `"x.ai/replayed"` marker, which the shell never sets in v1.
    /// Honoring it from day one means future replay producers need no pager change.
    /// Parsed loosely as a JSON value because the key contains a slash, which prost cannot model as a field.
    #[serde(default, rename = "_meta")]
    meta: Option<serde_json::Value>,
}

/// A single `x.ai/follow_ups` suggestion.
/// Only the human-facing `label` is consumed; `properties` and `tool_overrides` (also in the wire shape) are ignored.
#[derive(serde::Deserialize)]
pub(super) struct FollowUpSuggestionParam {
    #[serde(default)]
    label: String,
}

/// Sanitize a server-supplied suggestion label for safe chip rendering and submission.
/// Drops control, bidi, and format characters ([`is_unsafe_display_char`](crate::render::line_utils::is_unsafe_display_char)).
/// Bounds the length and trims surrounding whitespace.
pub(super) fn sanitize_suggestion(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .filter(|c| !crate::render::line_utils::is_unsafe_display_char(*c))
        .take(MAX_FOLLOW_UP_LABEL)
        .collect();
    cleaned.trim().to_owned()
}

/// Handle `x.ai/follow_ups`: render follow-up suggestion chips for the latest assistant response.
/// The reserved `_meta["x.ai/replayed"] == true` marker suppresses rendering (it is absent today and treated as optional).
/// A background agent's follow-ups would mis-route until the shell adds a session id.
pub(super) fn handle_follow_ups(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(params) = serde_json::from_str::<FollowUpsParams>(notif.params.get()) else {
        return false;
    };
    if params
        .meta
        .as_ref()
        .and_then(|m| m.get("x.ai/replayed"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    // The response id is the key by which the newest response wins, and it is retained in the seen ring
    // Drop a notification with a missing or oversized one
    if params.response_id.is_empty() || params.response_id.len() > MAX_RESPONSE_ID_LEN {
        return false;
    }
    // Bound count and length at ingestion (a stored cap, not just a render-time one)
    let suggestions: Vec<String> = params
        .suggestions
        .into_iter()
        .take(MAX_FOLLOW_UPS)
        .filter_map(|s| {
            let label = sanitize_suggestion(&s.label);
            (!label.is_empty()).then_some(label)
        })
        .collect();

    let ActiveView::Agent(id) = app.active_view else {
        return false;
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return false;
    };
    agent.apply_follow_ups_with_prompt(params.response_id, params.prompt_id.as_deref(), suggestions)
}
