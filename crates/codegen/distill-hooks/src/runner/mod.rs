// Modified for Distill by Samuel Fajreldines, 2026.
pub mod command;
pub mod http;

use std::time::Duration;

use crate::config::HookSpec;
use crate::event::{HookEventEnvelope, MAX_HOOK_FEEDBACK_CHARS, clip_reason, clip_text};
use serde::Deserialize;

use crate::result::{
    HttpInfo, OutputReplacement, PostToolUseHookOutcome, ReplacementKind, StopHookOutcome,
};

pub use crate::event::GateKind;

pub struct RunContext<'a> {
    pub session_id: &'a str,
    pub workspace_root: &'a str,
    pub process_scope: Option<distill_tools::util::ProcessScope>,
    /// Loaded by the caller off the dispatch path, so the announced count and the run loop agree and no dispatch reads the file.
    pub disabled: std::sync::Arc<crate::trust::DisabledHooks>,
}

impl RunContext<'_> {
    pub fn disabled(&self) -> &crate::trust::DisabledHooks {
        &self.disabled
    }
}

#[derive(Debug)]
pub enum HookRunnerResult {
    Allow {
        updated_input: Option<serde_json::Map<String, serde_json::Value>>,
        additional_context: Option<String>,
    },
    Ask {
        reason: Option<String>,
        updated_input: Option<serde_json::Map<String, serde_json::Value>>,
        additional_context: Option<String>,
    },
    Defer,
    Deny {
        reason: String,
        hook_name: String,
    },
    Block {
        reason: String,
        hook_name: String,
    },
    Stop(StopHookOutcome),
    PostToolUse {
        outcome: PostToolUseHookOutcome,
        failure: Option<String>,
    },
    Success,
    Failed(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GateHookJson {
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    hook_specific_output: Option<GateHookSpecificOutputJson>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GateHookSpecificOutputJson {
    #[serde(default)]
    permission_decision: Option<String>,
    #[serde(default)]
    permission_decision_reason: Option<String>,
    #[serde(default)]
    updated_input: Option<serde_json::Value>,
    #[serde(default)]
    additional_context: Option<serde_json::Value>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

struct NonObjectRewrite;

impl GateHookJson {
    fn is_gate_document(&self) -> bool {
        self.decision.is_some()
            || self.hook_specific_output.is_some()
            || self.rest.keys().any(|key| {
                matches!(
                    key.as_str(),
                    "updatedInput"
                        | "permissionDecision"
                        | "permissionDecisionReason"
                        | "continue"
                        | "additionalContext"
                )
            })
    }

    fn permission_decision(&self) -> Option<&str> {
        non_empty(
            self.hook_specific_output
                .as_ref()
                .and_then(|output| output.permission_decision.as_deref()),
        )
    }

    fn permission_decision_reason(&self) -> Option<&str> {
        self.hook_specific_output
            .as_ref()
            .and_then(|output| output.permission_decision_reason.as_deref())
    }

    fn resolve_decision(&self) -> GateDecision {
        let (field, decision_str) = match (
            self.permission_decision(),
            non_empty(self.decision.as_deref()),
        ) {
            (Some(token), _) => (DecisionField::HookSpecific, Some(token)),
            (None, legacy) => (DecisionField::TopLevel, legacy),
        };
        let token = decision_str.map_or(DecisionToken::Allow, |token| {
            DecisionToken::classify(field, token)
        });
        let reason = non_empty(self.permission_decision_reason())
            .or(non_empty(self.reason.as_deref()))
            .map(str::to_string);
        GateDecision { token, reason }
    }

    fn take_updated_input(
        self,
    ) -> Result<Option<serde_json::Map<String, serde_json::Value>>, NonObjectRewrite> {
        match self
            .hook_specific_output
            .and_then(|output| output.updated_input)
        {
            None => Ok(None),
            Some(serde_json::Value::Object(input)) => Ok(Some(input)),
            Some(_) => Err(NonObjectRewrite),
        }
    }

    fn ignored_parts(&self, decision: &DecisionToken) -> Vec<String> {
        let top = self
            .rest
            .keys()
            .filter(|key| !SILENTLY_IGNORED_TOP_LEVEL_KEYS.contains(&key.as_str()))
            .cloned();
        let nested = self
            .hook_specific_output
            .iter()
            .flat_map(|output| output.rest.keys())
            .filter(|key| !SILENTLY_IGNORED_NESTED_KEYS.contains(&key.as_str()))
            .map(|key| format!("hookSpecificOutput.{key}"));
        let stop_request = (self.rest.get("continue") == Some(&serde_json::Value::Bool(false)))
            .then(|| "continue: false".to_string());
        let unread_reason = (matches!(decision, DecisionToken::Allow | DecisionToken::Defer)
            && (non_empty(self.reason.as_deref()).is_some()
                || non_empty(self.permission_decision_reason()).is_some()))
        .then(|| "reason (only a deny or an ask has one)".to_string());
        let nested_output = self.hook_specific_output.as_ref();
        let deferred_rewrite = (matches!(decision, DecisionToken::Defer)
            && nested_output.is_some_and(|output| output.updated_input.is_some()))
        .then(|| "hookSpecificOutput.updatedInput".to_string());
        let bad_context = self
            .raw_additional_context()
            .is_some_and(|value| !value.is_string());
        let unread_valid_context = matches!(decision, DecisionToken::Defer | DecisionToken::Deny)
            && self.additional_context().is_some();
        let unread_context = (bad_context || unread_valid_context)
            .then(|| "hookSpecificOutput.additionalContext".to_string());
        top.chain(nested)
            .chain(stop_request)
            .chain(unread_reason)
            .chain(deferred_rewrite)
            .chain(unread_context)
            .collect()
    }

    fn raw_additional_context(&self) -> Option<&serde_json::Value> {
        self.hook_specific_output
            .as_ref()
            .and_then(|output| output.additional_context.as_ref())
    }

    fn additional_context(&self) -> Option<String> {
        let text = self.raw_additional_context()?.as_str()?;
        (!text.trim().is_empty()).then(|| clip_text(text, MAX_HOOK_FEEDBACK_CHARS))
    }
}

const SILENTLY_IGNORED_TOP_LEVEL_KEYS: &[&str] = &["hookEventName", "continue", "systemMessage"];
const SILENTLY_IGNORED_NESTED_KEYS: &[&str] = &["hookEventName"];

fn non_empty(text: Option<&str>) -> Option<&str> {
    let trimmed = text?.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Extract a hook's top-level `systemMessage` warning from its stdout JSON.
pub(crate) fn extract_system_message(stdout: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    non_empty(
        value
            .get("systemMessage")
            .and_then(serde_json::Value::as_str),
    )
    .map(crate::event::clip_reason)
}

#[derive(Debug, PartialEq, Eq)]
enum DecisionToken {
    Allow,
    Deny,
    Ask,
    Defer,
    Unknown { field: DecisionField, token: String },
}

impl DecisionToken {
    fn classify(field: DecisionField, token: &str) -> Self {
        match token {
            "allow" | "approve" => Self::Allow,
            "deny" | "block" => Self::Deny,
            "ask" => Self::Ask,
            "defer" => Self::Defer,
            other => Self::Unknown {
                field,
                token: other.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecisionField {
    TopLevel,
    HookSpecific,
}

impl DecisionField {
    const fn wire_name(self) -> &'static str {
        match self {
            Self::TopLevel => "decision",
            Self::HookSpecific => "hookSpecificOutput.permissionDecision",
        }
    }
}

struct GateDecision {
    token: DecisionToken,
    reason: Option<String>,
}

pub(crate) enum GateOutcome {
    Allow {
        updated_input: Option<serde_json::Map<String, serde_json::Value>>,
        additional_context: Option<String>,
    },
    Ask {
        reason: Option<String>,
        updated_input: Option<serde_json::Map<String, serde_json::Value>>,
        additional_context: Option<String>,
    },
    Defer,
    Deny(String),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookHealth {
    Healthy,
    Broken,
}

impl HookHealth {
    pub(crate) fn from_success(success: bool) -> Self {
        if success { Self::Healthy } else { Self::Broken }
    }
}

pub(crate) fn gate_outcome(
    json: GateHookJson,
    hook_name: &str,
    fallback_reason: Option<&str>,
    health: HookHealth,
) -> GateOutcome {
    let GateDecision { token, reason } = json.resolve_decision();
    let mut ignored = json.ignored_parts(&token);
    let additional_context = json.additional_context();
    let updated_input = resolve_rewrite(json, hook_name, health, &mut ignored);

    if !ignored.is_empty() {
        tracing::warn!(
            hook_name,
            ignored = %ignored.join(", "),
            "ignoring parts of a gate hook document"
        );
    }

    match token {
        DecisionToken::Deny => GateOutcome::Deny(clip_reason(
            &reason
                .or_else(|| fallback_reason.map(str::to_string))
                .unwrap_or_else(|| format!("denied by hook '{hook_name}'")),
        )),
        DecisionToken::Unknown { field, token } => GateOutcome::Failed(format!(
            "unknown decision value '{}' in '{}'",
            clip_reason(&token),
            field.wire_name()
        )),
        DecisionToken::Ask => GateOutcome::Ask {
            reason: reason.as_deref().map(clip_reason),
            updated_input,
            additional_context: drop_if_broken(
                additional_context,
                hook_name,
                "additionalContext",
                health,
            ),
        },
        DecisionToken::Defer => GateOutcome::Defer,
        DecisionToken::Allow => GateOutcome::Allow {
            updated_input,
            additional_context: drop_if_broken(
                additional_context,
                hook_name,
                "additionalContext",
                health,
            ),
        },
    }
}

fn resolve_rewrite(
    json: GateHookJson,
    hook_name: &str,
    health: HookHealth,
    ignored: &mut Vec<String>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let updated_input = match json.take_updated_input() {
        Ok(updated_input) => updated_input,
        Err(NonObjectRewrite) => {
            ignored.push("hookSpecificOutput.updatedInput (not an object)".to_string());
            None
        }
    };
    drop_if_broken(updated_input, hook_name, "updatedInput", health)
}

fn drop_if_broken<T>(
    value: Option<T>,
    hook_name: &str,
    wire_field: &str,
    health: HookHealth,
) -> Option<T> {
    if value.is_some() && health == HookHealth::Broken {
        tracing::warn!(hook_name, wire_field, "dropping a failed hook's field");
        return None;
    }
    value
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PromptHookJson {
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub(crate) fn prompt_json_to_block(
    json: &PromptHookJson,
    hook_name: &str,
    fallback_reason: Option<&str>,
) -> Result<Option<String>, String> {
    match json.decision.as_deref() {
        Some("block") => Ok(Some(
            json.reason
                .clone()
                .filter(|r| !r.trim().is_empty())
                .or_else(|| fallback_reason.map(str::to_string))
                .unwrap_or_else(|| format!("Prompt blocked by hook '{hook_name}'")),
        )),
        None | Some("approve") => Ok(None),
        Some(other) => Err(format!("unknown decision value '{other}'")),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StopHookJson {
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default, rename = "continue")]
    pub continue_: Option<bool>,
    #[serde(default, rename = "stopReason")]
    pub stop_reason: Option<String>,
    #[serde(default, rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<StopHookSpecificOutputJson>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StopHookSpecificOutputJson {
    #[serde(default, rename = "additionalContext")]
    pub additional_context: Option<String>,
}

pub(crate) fn stop_json_to_outcome(
    json: StopHookJson,
    hook_name: &str,
) -> Result<StopHookOutcome, String> {
    let block_reason = match json.decision.as_deref() {
        Some("block") => Some(
            json.reason
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| format!("Blocked by stop hook '{hook_name}'")),
        ),
        Some("approve") | None => None,
        Some(other) => {
            return Err(format!("unknown decision value '{other}'"));
        }
    };
    Ok(StopHookOutcome {
        block_reason,
        additional_context: json
            .hook_specific_output
            .and_then(|output| output.additional_context)
            .filter(|context| !context.trim().is_empty())
            .map(|context| clip_text(&context, MAX_HOOK_FEEDBACK_CHARS)),
        force_stop: (json.continue_ == Some(false)).then_some(crate::result::StopOverride {
            reason: json.stop_reason,
        }),
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PostToolUseHookJson {
    pub decision: Option<String>,
    pub reason: Option<String>,
    pub hook_specific_output: Option<PostToolUseHookSpecificOutputJson>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PostToolUseHookSpecificOutputJson {
    pub additional_context: Option<String>,
    pub updated_tool_output: Option<serde_json::Value>,
    #[serde(rename = "updatedMCPToolOutput")]
    pub updated_mcp_tool_output: Option<serde_json::Value>,
}

#[derive(Debug, Default)]
pub(crate) struct PostToolUseParse {
    pub outcome: PostToolUseHookOutcome,
    pub failure: Option<String>,
}

pub(crate) fn post_tool_use_json_to_outcome(
    json: PostToolUseHookJson,
    hook_name: &str,
    health: HookHealth,
) -> PostToolUseParse {
    let mut failure = None;
    let block_reason = match json.decision.as_deref() {
        Some("block") => Some(
            json.reason
                .filter(|reason| !reason.trim().is_empty())
                .map(|reason| clip_text(&reason, MAX_HOOK_FEEDBACK_CHARS))
                .unwrap_or_else(|| format!("Blocked by post_tool_use hook '{hook_name}'")),
        ),
        Some("approve") | None => None,
        Some(other) => {
            tracing::warn!(
                hook_name,
                decision = %other,
                "post_tool_use hook set an unrecognized decision; only \"block\" is honored"
            );
            failure = Some(format!(
                "unrecognized decision value '{}'; only \"block\" is honored",
                clip_reason(other)
            ));
            None
        }
    };
    let output = json.hook_specific_output.unwrap_or_default();
    let additional_context = output
        .additional_context
        .filter(|context| !context.trim().is_empty())
        .map(|context| clip_text(&context, MAX_HOOK_FEEDBACK_CHARS));
    let builtin = output.updated_tool_output;
    let mcp = output.updated_mcp_tool_output;
    let (kind, value) = match (builtin, mcp) {
        (Some(value), Some(_)) => {
            tracing::warn!(
                hook_name,
                "hook set both updatedToolOutput and updatedMCPToolOutput; keeping updatedToolOutput"
            );
            (ReplacementKind::Builtin, Some(value))
        }
        (Some(value), None) => (ReplacementKind::Builtin, Some(value)),
        (None, Some(value)) => (ReplacementKind::Mcp, Some(value)),
        (None, None) => (ReplacementKind::Builtin, None),
    };
    let output_replacement = value.map(|value| OutputReplacement {
        kind,
        hook_name: hook_name.to_string(),
        value,
    });
    let output_replacement = output_replacement.and_then(|replacement| {
        if health == HookHealth::Broken {
            tracing::warn!(
                hook_name,
                wire_field = replacement.wire_field(),
                "dropping a field of a hook that failed"
            );
            None
        } else {
            Some(replacement)
        }
    });
    PostToolUseParse {
        outcome: PostToolUseHookOutcome {
            block_reason,
            additional_context: drop_if_broken(
                additional_context,
                hook_name,
                "additionalContext",
                health,
            ),
            output_replacement,
        },
        failure,
    }
}

pub type HookRunOutput = (HookRunnerResult, Duration, Option<HttpInfo>, Option<String>);

pub async fn run_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
    mode: GateKind,
) -> HookRunOutput {
    match spec.handler_type {
        crate::config::HandlerType::Command => {
            let (result, elapsed, system_message) =
                command::run_command_hook(spec, envelope, ctx, mode).await;
            (result, elapsed, None, system_message)
        }
        crate::config::HandlerType::Http => http::run_http_hook(spec, envelope, ctx, mode).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_system_message_trims_and_ignores_blank_or_absent() {
        assert_eq!(
            extract_system_message(r#"{"systemMessage":"  heads up  ","decision":"allow"}"#),
            Some("heads up".to_string())
        );
        assert_eq!(extract_system_message(r#"{"decision":"allow"}"#), None);
        assert_eq!(extract_system_message(r#"{"systemMessage":"   "}"#), None);
        assert_eq!(extract_system_message(""), None);
        assert_eq!(extract_system_message("not json"), None);
    }

    #[test]
    fn ignored_parts_names_what_changed_nothing() {
        for (json, expected) in [
            (
                r#"{"decision":"allow","hookSpecificOutput":{"updateInput":{}}}"#,
                vec!["hookSpecificOutput.updateInput"],
            ),
            (
                r#"{"permissionDecision":"deny"}"#,
                vec!["permissionDecision"],
            ),
            (
                r#"{"decision":"allow","continue":false}"#,
                vec!["continue: false"],
            ),
            (
                r#"{"decision":"allow","reason":"why"}"#,
                vec!["reason (only a deny or an ask has one)"],
            ),
            (r#"{"decision":"deny","reason":"why"}"#, vec![]),
            (
                r#"{"decision":"deny","reason":"why","hookSpecificOutput":{"additionalContext":"note"}}"#,
                vec!["hookSpecificOutput.additionalContext"],
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"defer","updatedInput":{"command":"x"},"additionalContext":"note"}}"#,
                vec![
                    "hookSpecificOutput.updatedInput",
                    "hookSpecificOutput.additionalContext",
                ],
            ),
        ] {
            let document: GateHookJson = serde_json::from_str(json).expect("valid gate JSON");
            let decision = document.resolve_decision().token;
            assert_eq!(document.ignored_parts(&decision), expected, "for {json}");
        }
    }

    #[test]
    fn blank_permission_decision_falls_back_to_legacy_decision() {
        for (json, expected) in [
            (
                r#"{"decision":"deny","hookSpecificOutput":{"permissionDecision":"  "}}"#,
                DecisionToken::Deny,
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"  "}}"#,
                DecisionToken::Allow,
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":""}}"#,
                DecisionToken::Allow,
            ),
        ] {
            let document: GateHookJson = serde_json::from_str(json).expect("valid gate JSON");
            assert_eq!(document.resolve_decision().token, expected, "for {json}");
        }
    }

    #[test]
    fn ask_with_non_object_updated_input_keeps_asking() {
        let json = r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"confirm","updatedInput":"nope"}}"#;

        let document: GateHookJson = serde_json::from_str(json).expect("valid gate JSON");
        let mut ignored = Vec::new();
        let rewrite = resolve_rewrite(document, "h", HookHealth::Healthy, &mut ignored);
        assert!(
            rewrite.is_none(),
            "a non-object updatedInput must drop the rewrite"
        );
        assert!(
            ignored.iter().any(|part| part.contains("updatedInput")),
            "the dropped rewrite must be named in ignored_parts, got {ignored:?}"
        );

        let document: GateHookJson = serde_json::from_str(json).expect("valid gate JSON");
        let outcome = gate_outcome(document, "h", None, HookHealth::Healthy);
        assert!(
            matches!(
                outcome,
                GateOutcome::Ask {
                    updated_input: None,
                    ..
                }
            ),
            "ask + non-object updatedInput must stay an Ask with no rewrite, not fail open"
        );
    }

    #[test]
    fn deny_with_array_additional_context_still_denies() {
        let json = r#"{"decision":"deny","reason":"blocked","hookSpecificOutput":{"permissionDecision":"deny","additionalContext":["a","b"]}}"#;
        let document: GateHookJson =
            serde_json::from_str(json).expect("a non-string additionalContext must still parse");
        assert!(
            document.is_gate_document(),
            "the document must still be recognized as a gate"
        );
        let outcome = gate_outcome(document, "h", None, HookHealth::Healthy);
        assert!(
            matches!(outcome, GateOutcome::Deny(ref reason) if reason == "blocked"),
            "a deny must land despite an array additionalContext"
        );
    }

    #[test]
    fn bad_additional_context_is_dropped_and_named_and_the_decision_survives() {
        for (json, expected) in [
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"ask","additionalContext":[1,2]}}"#,
                "ask",
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"defer","additionalContext":1}}"#,
                "defer",
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"block","additionalContext":{}}}"#,
                "deny",
            ),
        ] {
            let document: GateHookJson = serde_json::from_str(json).expect("valid gate JSON");
            let decision = document.resolve_decision().token;
            assert!(
                document
                    .ignored_parts(&decision)
                    .iter()
                    .any(|part| part == "hookSpecificOutput.additionalContext"),
                "a bad additionalContext must be named in ignored_parts, for {json}"
            );

            let document: GateHookJson = serde_json::from_str(json).expect("valid gate JSON");
            let got = match gate_outcome(document, "h", None, HookHealth::Healthy) {
                GateOutcome::Ask {
                    additional_context, ..
                } => {
                    assert!(
                        additional_context.is_none(),
                        "the bad context must be dropped, for {json}"
                    );
                    "ask"
                }
                GateOutcome::Defer => "defer",
                GateOutcome::Deny(_) => "deny",
                GateOutcome::Allow { .. } => "allow",
                GateOutcome::Failed(_) => "failed",
            };
            assert_eq!(got, expected, "the token must survive, for {json}");
        }
    }

    #[test]
    fn top_level_additional_context_is_a_recognized_gate_document_and_is_named() {
        let document: GateHookJson =
            serde_json::from_str(r#"{"additionalContext":"note"}"#).expect("valid JSON");
        assert!(
            document.is_gate_document(),
            "a top-level additionalContext must mark a gate document"
        );
        let decision = document.resolve_decision().token;
        assert_eq!(document.ignored_parts(&decision), vec!["additionalContext"]);
    }

    #[test]
    fn valid_additional_context_flows_through() {
        let json =
            r#"{"hookSpecificOutput":{"permissionDecision":"allow","additionalContext":"note"}}"#;
        let document: GateHookJson = serde_json::from_str(json).expect("valid gate JSON");
        match gate_outcome(document, "h", None, HookHealth::Healthy) {
            GateOutcome::Allow {
                additional_context, ..
            } => assert_eq!(additional_context.as_deref(), Some("note")),
            _ => panic!("a valid additionalContext must flow through an allow"),
        }
    }
}
