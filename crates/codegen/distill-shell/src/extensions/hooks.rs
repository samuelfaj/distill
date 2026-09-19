// Modified for Distill by Samuel Fajreldines, 2026.
use std::collections::{HashMap, HashSet};

use agent_client_protocol as acp;
use distill_hooks::event::{HookEventEnvelope, HookEventName};
use distill_hooks::matcher::HookMatcher;
use distill_hooks_plugins_types::{HookEvent, HookHandlerType, HookInfo};
use serde::Deserialize;

use crate::agent::MvpAgent;

type ExtResult = Result<acp::ExtResponse, acp::Error>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListRequest {
    session_id: String,
}

/// Current hooks as wire DTOs for `HooksListResponse` / `HooksChanged`
/// payloads. Reads the disabled set and registered dirs fresh from disk so
/// every producer converts with the same inputs.
pub(crate) fn current_hook_infos(
    registry: Option<&distill_hooks::discovery::HookRegistry>,
) -> Vec<HookInfo> {
    let Some(registry) = registry else {
        return Vec::new();
    };
    let disabled = distill_hooks::trust::DisabledHooks::load();
    let registered = crate::config::registered_hook_paths();
    hook_specs_to_infos(&registry.all_hooks(), &disabled, &registered)
}

/// Convert a whole spec list to wire DTOs. `removable` is source-level: `HooksAction::Remove` unregisters the entire `source_dir`, and a managed-policy member pins the directory (removal is refused), so no hook in such a source is removable.
fn hook_specs_to_infos(
    specs: &[&distill_hooks::config::HookSpec],
    disabled: &distill_hooks::trust::DisabledHooks,
    registered_dirs: &HashSet<String>,
) -> Vec<HookInfo> {
    let pinned_dirs: HashSet<String> = specs
        .iter()
        .filter(|s| s.is_managed_policy())
        .map(|s| s.source_dir.display().to_string())
        .collect();
    specs
        .iter()
        .map(|spec| hook_spec_to_info_with(spec, disabled, registered_dirs, &pinned_dirs))
        .collect()
}

fn hook_spec_to_info_with(
    spec: &distill_hooks::config::HookSpec,
    disabled: &distill_hooks::trust::DisabledHooks,
    registered_dirs: &HashSet<String>,
    pinned_source_dirs: &HashSet<String>,
) -> HookInfo {
    use distill_hooks::event::HookEventName;

    let event = match spec.event {
        HookEventName::SessionStart => HookEvent::SessionStart,
        HookEventName::SessionEnd => HookEvent::SessionEnd,
        HookEventName::Stop => HookEvent::Stop,
        HookEventName::StopFailure => HookEvent::StopFailure,
        HookEventName::StopCancelled => HookEvent::StopCancelled,
        HookEventName::PreToolUse => HookEvent::PreToolUse,
        HookEventName::PostToolUse => HookEvent::PostToolUse,
        HookEventName::PostToolUseFailure => HookEvent::PostToolUseFailure,
        HookEventName::PermissionDenied => HookEvent::PermissionDenied,
        HookEventName::UserPromptSubmit => HookEvent::UserPromptSubmit,
        HookEventName::Notification => HookEvent::Notification,
        HookEventName::SubagentStart => HookEvent::SubagentStart,
        HookEventName::SubagentStop | HookEventName::SubagentEnd => HookEvent::SubagentStop,
        HookEventName::PreCompact => HookEvent::PreCompact,
        HookEventName::PostCompact => HookEvent::PostCompact,
    };

    let handler_type = if spec.url.is_some() {
        HookHandlerType::Http
    } else {
        HookHandlerType::Command
    };

    let command_display = spec
        .command_raw
        .clone()
        .or_else(|| spec.command.as_ref().map(|p| p.display().to_string()));
    let url_display = spec.url_raw.clone().or_else(|| spec.url.clone());

    let source_dir = spec.source_dir.display().to_string();
    let removable =
        registered_dirs.contains(&source_dir) && !pinned_source_dirs.contains(&source_dir);
    HookInfo {
        name: spec.name.clone(),
        event,
        handler_type,
        matcher: spec.configured_matcher.clone(),
        command: command_display,
        url: url_display,
        timeout_ms: spec.timeout_ms,
        source_dir,
        disabled: distill_hooks::trust::hook_disabled_for_display_with(spec, disabled),
        pinned: spec.is_managed_policy(),
        removable,
    }
}

#[derive(Debug, Clone)]
pub struct ClientHookGroup {
    pub matcher: Option<HookMatcher>,
    pub callback_ids: Vec<String>,
    pub timeout: Option<std::time::Duration>,
}

pub(crate) type ClientHooks = HashMap<HookEventName, Vec<ClientHookGroup>>;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientHookDispatch<'a> {
    pub hook_callback_id: &'a str,
    #[serde(flatten)]
    pub envelope: &'a HookEventEnvelope,
}

pub(crate) const ADVERTISED_BLOCKING_EVENTS: &[distill_hooks::event::HookEventName] = &[
    distill_hooks::event::HookEventName::PreToolUse,
    distill_hooks::event::HookEventName::Stop,
    distill_hooks::event::HookEventName::SubagentStop,
];

pub(crate) const ADVERTISED_DECISIONS: &[&str] = &["deny", "block"];

pub(crate) const ADVERTISED_STOP_SIGNALS: &[&str] =
    &["continue", "stopReason", "additionalContext"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClientHookDecision {
    #[default]
    Continue,
    #[serde(alias = "block")]
    Deny,
    Ask,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientHookResponse {
    #[serde(default)]
    pub decision: ClientHookDecision,
    #[serde(default, alias = "reason")]
    pub system_message: Option<String>,
    #[serde(default, rename = "continue")]
    pub continue_: Option<bool>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub additional_context: Option<String>,
}

pub(crate) fn parse_client_hooks(meta: Option<&acp::Meta>) -> ClientHooks {
    let mut hooks = ClientHooks::new();
    let Some(map) = meta
        .and_then(|m| m.get("x.ai/hooks"))
        .and_then(|h| h.as_object())
    else {
        return hooks;
    };
    for (event_name, value) in map {
        let de = serde::de::value::StrDeserializer::<serde::de::value::Error>::new(event_name);
        let Ok(event) = HookEventName::deserialize(de) else {
            tracing::warn!(event = %event_name, "ignoring unknown x.ai/hooks event");
            continue;
        };
        let Some(array) = value.as_array() else {
            tracing::warn!(event = %event_name, "x.ai/hooks event value is not an array; skipping");
            continue;
        };
        let groups: Vec<ClientHookGroup> = array
            .iter()
            .filter_map(|group| parse_hook_group(event, group))
            .collect();
        if !groups.is_empty() {
            hooks.entry(event.canonical()).or_default().extend(groups);
        }
    }
    hooks
}

pub(crate) fn reconnect_client_hooks(meta: Option<&acp::Meta>) -> Option<ClientHooks> {
    meta.and_then(|m| m.get("x.ai/hooks"))
        .map(|_| parse_client_hooks(meta))
}

fn parse_hook_group(event: HookEventName, value: &serde_json::Value) -> Option<ClientHookGroup> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WireGroup {
        #[serde(default)]
        matcher: Option<String>,
        #[serde(default)]
        hook_callback_ids: Vec<String>,
        #[serde(default)]
        timeout: Option<f64>,
    }

    let group = WireGroup::deserialize(value)
        .inspect_err(|err| tracing::warn!(%event, %err, "ignoring malformed x.ai/hooks group"))
        .ok()?;
    if group.hook_callback_ids.is_empty() {
        tracing::warn!(%event, "ignoring x.ai/hooks group with no hookCallbackIds");
        return None;
    }
    const MAX_HOOK_TIMEOUT_SECS: f64 = 600.0;
    let timeout = group
        .timeout
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| std::time::Duration::from_secs_f64(s.min(MAX_HOOK_TIMEOUT_SECS)));
    let matcher = match group.matcher.as_deref() {
        None | Some("") | Some("*") => None,
        Some(pattern) if event.traits().matcher == distill_hooks::event::MatcherPolicy::Ignored => {
            tracing::warn!(%event, pattern, "matcher on a {event} hook group is ignored (this event always fires)");
            None
        }
        Some(pattern) => match HookMatcher::new(pattern) {
            Ok(matcher) => Some(matcher),
            Err(err) => {
                tracing::warn!(%event, pattern, %err, "ignoring x.ai/hooks group with invalid matcher");
                return None;
            }
        },
    };
    Some(ClientHookGroup {
        matcher,
        callback_ids: group.hook_callback_ids,
        timeout,
    })
}

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/hooks/list" => {
            let req: ListRequest = super::parse_params(args)?;
            let sid = acp::SessionId::new(req.session_id);

            let result = agent
                .list_hooks(&sid)
                .await
                .ok_or_else(|| anyhow::anyhow!("session not found"));
            super::to_ext_response(result)
        }
        "x.ai/hooks/action" => {
            let req: distill_hooks_plugins_types::HooksActionRequest = super::parse_params(args)?;
            let sid = acp::SessionId::new(req.session_id);

            let result = agent
                .execute_hooks_action(&sid, req.action)
                .await
                .ok_or_else(|| anyhow::anyhow!("session not found"));
            super::to_ext_response(result)
        }
        _ => Err(acp::Error::method_not_found()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_hooks::config::HookSpec;
    use distill_hooks::event::HookEventName;
    use std::path::PathBuf;

    fn make_spec(
        command_raw: Option<&str>,
        command: Option<&str>,
        url_raw: Option<&str>,
        url: Option<&str>,
    ) -> HookSpec {
        HookSpec {
            name: "test:pre_tool_use[0].hooks[0]".to_string(),
            event: HookEventName::PreToolUse,
            handler_type: distill_hooks::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: command.map(PathBuf::from),
            command_raw: command_raw.map(str::to_string),
            url: url.map(str::to_string),
            url_raw: url_raw.map(str::to_string),
            timeout_ms: 5000,
            source_dir: PathBuf::from("/tmp"),
            extra_env: HashMap::new(),
            layer: distill_hooks::config::HookProvenance::File,
        }
    }

    #[test]
    fn hook_spec_to_info_raw_display_wins_so_secrets_never_reach_dto() {
        let no_disabled = distill_hooks::trust::DisabledHooks::from_names([]);
        let no_dirs = HashSet::new();
        let command = |raw, resolved| {
            hook_specs_to_infos(
                &[&make_spec(raw, resolved, None, None)],
                &no_disabled,
                &no_dirs,
            )
            .remove(0)
            .command
        };
        assert_eq!(
            command(Some("${VAR}/x"), Some("/resolved/x")).as_deref(),
            Some("${VAR}/x")
        );
        assert_eq!(
            command(None, Some("/legacy/x")).as_deref(),
            Some("/legacy/x")
        );
        assert!(command(None, None).is_none());

        let url = |raw, resolved| {
            hook_specs_to_infos(
                &[&make_spec(None, None, raw, resolved)],
                &no_disabled,
                &no_dirs,
            )
            .remove(0)
            .url
        };
        assert_eq!(
            url(
                Some("https://${HOST}/p?token=${TOKEN}"),
                Some("https://api/p?token=ghp_X")
            )
            .as_deref(),
            Some("https://${HOST}/p?token=${TOKEN}"),
        );
        assert_eq!(
            url(None, Some("https://h/c")).as_deref(),
            Some("https://h/c")
        );
        assert!(url(None, None).is_none());
    }

    /// `removable` must mean "Remove can succeed": a managed-policy member pins the whole source directory even when it is user-registered, and an unpinned sibling in that directory is pinned along with it.
    /// Both managed tiers (`SystemManaged` and `Requirements`) pin identically.
    #[test]
    fn hook_specs_to_infos_pins_removable_at_source_level() {
        let no_disabled = distill_hooks::trust::DisabledHooks::from_names([]);
        let registered: HashSet<String> = [
            "/reg/policy".to_string(),
            "/reg/req".to_string(),
            "/reg/user".to_string(),
        ]
        .into();

        let mut policy = make_spec(Some("a"), None, None, None);
        policy.source_dir = PathBuf::from("/reg/policy");
        policy.layer = distill_hooks::config::HookProvenance::SystemManaged;
        let mut sibling = make_spec(Some("b"), None, None, None);
        sibling.source_dir = PathBuf::from("/reg/policy");
        let mut req = make_spec(Some("r"), None, None, None);
        req.source_dir = PathBuf::from("/reg/req");
        req.layer = distill_hooks::config::HookProvenance::Requirements;
        let mut req_sibling = make_spec(Some("s"), None, None, None);
        req_sibling.source_dir = PathBuf::from("/reg/req");
        let mut user = make_spec(Some("c"), None, None, None);
        user.source_dir = PathBuf::from("/reg/user");
        let unregistered = make_spec(Some("d"), None, None, None); // stays at /tmp

        let infos = hook_specs_to_infos(
            &[&policy, &sibling, &req, &req_sibling, &user, &unregistered],
            &no_disabled,
            &registered,
        );
        let [
            policy_info,
            sibling_info,
            req_info,
            req_sib,
            user_info,
            unreg,
            ..,
        ] = infos.as_slice()
        else {
            panic!("expected 6 hook infos: {infos:?}");
        };
        assert!(policy_info.pinned && !policy_info.removable);
        assert!(
            !sibling_info.pinned && !sibling_info.removable,
            "unpinned sibling of a managed-policy hook must not be removable"
        );
        assert!(
            req_info.pinned && !req_info.removable,
            "Requirements tier must pin its source like SystemManaged"
        );
        assert!(
            !req_sib.pinned && !req_sib.removable,
            "unpinned sibling of a Requirements hook must not be removable"
        );
        assert!(user_info.removable);
        assert!(!unreg.removable, "unregistered dirs are never removable");
    }

    #[test]
    fn parse_client_hooks_parses_valid_groups() {
        let meta = serde_json::json!({
            "x.ai/hooks": {
                "PreToolUse": [
                    { "matcher": "run_terminal_command", "hookCallbackIds": ["cb_0"] },
                    { "matcher": null, "hookCallbackIds": ["cb_1"] },
                    { "matcher": "*", "hookCallbackIds": ["cb_2"] }
                ],
                "post_tool_use": [{ "hookCallbackIds": ["cb_3"] }]
            }
        });
        let hooks = parse_client_hooks(meta.as_object());

        let Some(pre) = hooks.get(&HookEventName::PreToolUse) else {
            panic!("expected PreToolUse hooks: {hooks:?}");
        };
        assert_eq!(pre.len(), 3);
        let Some(first) = pre.first() else {
            panic!("expected PreToolUse group: {pre:?}");
        };
        assert_eq!(first.callback_ids, ["cb_0"]);
        let matcher = first.matcher.as_ref().unwrap();
        assert!(matcher.is_match("run_terminal_command"));
        assert!(!matcher.is_match("read_file"));
        assert!(pre.get(1).is_some_and(|g| g.matcher.is_none()));
        assert!(pre.get(2).is_some_and(|g| g.matcher.is_none()));
        assert!(hooks.contains_key(&HookEventName::PostToolUse));
    }

    #[test]
    fn parse_client_hooks_drops_invalid_and_absent() {
        assert!(parse_client_hooks(None).is_empty());
        assert!(
            parse_client_hooks(serde_json::json!({ "askUserQuestion": true }).as_object())
                .is_empty()
        );

        let meta = serde_json::json!({
            "NotARealEvent": [{ "hookCallbackIds": ["x"] }],
            "x.ai/hooks": {
                "PreToolUse": [
                    { "matcher": "[invalid", "hookCallbackIds": ["bad_regex"] },
                    { "matcher": "run_terminal_command", "hookCallbackIds": [] },
                    { "matcher": "read_file", "hookCallbackIds": ["good"] }
                ]
            }
        });
        let parsed = parse_client_hooks(meta.as_object());
        let Some(groups) = parsed.get(&HookEventName::PreToolUse) else {
            panic!("expected PreToolUse hooks: {parsed:?}");
        };
        assert_eq!(groups.len(), 1);
        let Some(first) = groups.first() else {
            panic!("expected one group: {groups:?}");
        };
        assert_eq!(first.callback_ids, ["good"]);
    }

    #[test]
    fn parse_client_hooks_reads_group_timeout() {
        let meta = serde_json::json!({
            "x.ai/hooks": {
                "PreToolUse": [
                    { "hookCallbackIds": ["a"], "timeout": 5.0 },
                    { "hookCallbackIds": ["b"], "timeout": 0 },
                    { "hookCallbackIds": ["c"] },
                    { "hookCallbackIds": ["d"], "timeout": 100000 }
                ]
            }
        });
        let parsed = parse_client_hooks(meta.as_object());
        let Some(groups) = parsed.get(&HookEventName::PreToolUse) else {
            panic!("expected PreToolUse hooks: {parsed:?}");
        };
        let [g0, g1, g2, g3] = groups.as_slice() else {
            panic!("expected 4 groups: {groups:?}");
        };
        assert_eq!(g0.timeout, Some(std::time::Duration::from_secs(5)));
        assert_eq!(g1.timeout, None);
        assert_eq!(g2.timeout, None);
        assert_eq!(g3.timeout, Some(std::time::Duration::from_secs(600)));
    }

    #[test]
    fn parse_client_hooks_canonicalizes_subagent_alias() {
        let meta = serde_json::json!({
            "x.ai/hooks": { "SubagentEnd": [{ "hookCallbackIds": ["cb"] }] }
        });
        let hooks = parse_client_hooks(meta.as_object());
        assert!(hooks.contains_key(&HookEventName::SubagentStop));
        assert!(!hooks.contains_key(&HookEventName::SubagentEnd));
    }

    #[test]
    fn reconnect_client_hooks_only_when_key_present() {
        assert!(reconnect_client_hooks(None).is_none());
        assert!(reconnect_client_hooks(serde_json::json!({ "other": true }).as_object()).is_none());

        let cleared = reconnect_client_hooks(serde_json::json!({ "x.ai/hooks": {} }).as_object());
        assert!(cleared.is_some_and(|h| h.is_empty()));

        let set = reconnect_client_hooks(
            serde_json::json!({
                "x.ai/hooks": { "PreToolUse": [{ "hookCallbackIds": ["cb"] }] }
            })
            .as_object(),
        );
        assert!(set.is_some_and(|h| h.contains_key(&HookEventName::PreToolUse)));
    }

    #[test]
    fn client_hook_response_deserialization() {
        let deny: ClientHookResponse =
            serde_json::from_str(r#"{"decision":"deny","systemMessage":"blocked"}"#).unwrap();
        assert_eq!(deny.decision, ClientHookDecision::Deny);
        assert_eq!(deny.system_message.as_deref(), Some("blocked"));

        let unknown: ClientHookResponse =
            serde_json::from_str(r#"{"decision":"maybe_later"}"#).unwrap();
        assert_eq!(unknown.decision, ClientHookDecision::Other);

        let empty: ClientHookResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.decision, ClientHookDecision::Continue);
        assert!(empty.system_message.is_none());
        assert_eq!(
            ClientHookResponse::default().decision,
            ClientHookDecision::Continue
        );

        let stop: ClientHookResponse = serde_json::from_str(
            r#"{"continue":false,"stopReason":"budget","additionalContext":"ctx"}"#,
        )
        .unwrap();
        assert_eq!(stop.decision, ClientHookDecision::Continue);
        assert_eq!(stop.continue_, Some(false));
        assert_eq!(stop.stop_reason.as_deref(), Some("budget"));
        assert_eq!(stop.additional_context.as_deref(), Some("ctx"));

        let blocked: ClientHookResponse =
            serde_json::from_str(r#"{"decision":"block","reason":"run the tests"}"#).unwrap();
        assert_eq!(blocked.decision, ClientHookDecision::Deny);
        assert_eq!(blocked.system_message.as_deref(), Some("run the tests"));
    }

    #[test]
    fn advertised_blocking_events_are_gates() {
        use distill_hooks::event::GateKind;
        for event in ADVERTISED_BLOCKING_EVENTS {
            assert_ne!(
                event.traits().gate,
                GateKind::Observe,
                "advertised blocking event {event:?} has no decision gate"
            );
        }
    }

    #[test]
    fn advertised_capabilities_match_response_parser() {
        for decision in ADVERTISED_DECISIONS {
            let parsed: ClientHookDecision =
                serde_json::from_value(serde_json::json!(decision)).unwrap();
            assert_eq!(
                parsed,
                ClientHookDecision::Deny,
                "advertised decision {decision:?} must parse as a blocking decision"
            );
        }

        let signal_values = serde_json::json!({
            "continue": false,
            "stopReason": "r",
            "additionalContext": "c",
        });
        for signal in ADVERTISED_STOP_SIGNALS {
            let response: ClientHookResponse = serde_json::from_value(
                serde_json::json!({ *signal: signal_values.get(*signal).cloned().unwrap_or(serde_json::Value::Null) }),
            )
            .unwrap();
            let captured = match *signal {
                "continue" => response.continue_ == Some(false),
                "stopReason" => response.stop_reason.as_deref() == Some("r"),
                "additionalContext" => response.additional_context.as_deref() == Some("c"),
                other => panic!("unknown advertised stop signal {other:?}"),
            };
            assert!(captured, "advertised stop signal {signal:?} was not parsed");
        }
    }

    #[test]
    fn client_hook_dispatch_serializes_envelope() {
        use distill_hooks::event::{HookEventEnvelope, HookPayload};

        let envelope = HookEventEnvelope {
            hook_event_name: HookEventName::PreToolUse,
            session_id: "s1".into(),
            cwd: "/work".into(),
            workspace_root: "/work".into(),
            timestamp: "t".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: Some("default".into()),
            payload: HookPayload::PreToolUse {
                tool_name: "run_terminal_command".into(),
                tool_use_id: "call_1".into(),
                tool_input: serde_json::json!({ "command": "ls" }),
                tool_input_truncated: true,
                subagent_type: None,
            },
        };
        let dispatch = ClientHookDispatch {
            hook_callback_id: "cb_0",
            envelope: &envelope,
        };
        let value = serde_json::to_value(&dispatch).unwrap();
        assert_eq!(
            value.get("hookCallbackId").and_then(|v| v.as_str()),
            Some("cb_0")
        );
        assert_eq!(
            value.get("hookEventName").and_then(|v| v.as_str()),
            Some("pre_tool_use")
        );
        assert_eq!(value.get("sessionId").and_then(|v| v.as_str()), Some("s1"));
        assert_eq!(value.get("cwd").and_then(|v| v.as_str()), Some("/work"));
        assert_eq!(
            value.get("toolUseId").and_then(|v| v.as_str()),
            Some("call_1")
        );
        assert_eq!(
            value.get("toolName").and_then(|v| v.as_str()),
            Some("run_terminal_command")
        );
        assert_eq!(
            value
                .get("toolInput")
                .and_then(|t| t.get("command"))
                .and_then(|v| v.as_str()),
            Some("ls")
        );
        assert_eq!(
            value.get("toolInputTruncated").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            value.get("permissionMode").and_then(|v| v.as_str()),
            Some("default")
        );
    }
}
