// Modified for Distill by Samuel Fajreldines, 2026.
use std::net::IpAddr;
use std::time::{Duration, Instant};

use url::Url;

use crate::config::HookSpec;
use crate::event::HookEventEnvelope;
use crate::result::{HttpInfo, StopHookOutcome};

use super::command::MAX_OUTPUT_BYTES;
use super::{
    GateKind, GateOutcome, HookHealth, HookRunOutput, HookRunnerResult, PostToolUseHookJson,
    PromptHookJson, RunContext, StopHookJson, extract_system_message,
    post_tool_use_json_to_outcome, prompt_json_to_block, stop_json_to_outcome,
};

const RESPONSE_PREVIEW_MAX: usize = 200;

async fn read_body_capped(mut response: reqwest::Response) -> reqwest::Result<String> {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < MAX_OUTPUT_BYTES {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let take = chunk.len().min(MAX_OUTPUT_BYTES - buf.len());
        if let Some(part) = chunk.get(..take) {
            buf.extend_from_slice(part);
        }
        if take < chunk.len() {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if octets[0] == 127 {
                return false;
            }
            if octets[0] == 10 {
                return true;
            }
            if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                return true;
            }
            if octets[0] == 192 && octets[1] == 168 {
                return true;
            }
            if octets[0] == 169 && octets[1] == 254 {
                return true;
            }
            if octets[0] == 100 && (64..=127).contains(&octets[1]) {
                return true;
            }
            if v4.is_unspecified() {
                return true;
            }
            false
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return false;
            }
            if v6.is_unspecified() {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ip(&IpAddr::V4(v4));
            }
            let segments = v6.segments();
            if segments[0] & 0xffc0 == 0xfe80 {
                return true;
            }
            if segments[0] & 0xfe00 == 0xfc00 {
                return true;
            }
            false
        }
    }
}

async fn validate_hook_url(url: &str) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;

    if parsed.scheme() != "https" {
        return Err(format!(
            "only https:// URLs are allowed for HTTP hooks, got {}://",
            parsed.scheme()
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(format!("URL resolves to blocked private/internal IP: {ip}"));
        }
        return Ok(());
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&addr_str)
        .await
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("DNS resolved no addresses for {host}"));
    }

    // SECURITY: resolution-time check only; DNS rebinding can still swap in a blocked IP at send time.
    for addr in &addrs {
        if is_blocked_ip(&addr.ip()) {
            return Err(format!(
                "URL host {host} resolves to blocked private/internal IP: {}",
                addr.ip()
            ));
        }
    }

    Ok(())
}

fn build_hook_client(timeout_ms: u64) -> reqwest::Client {
    distill_extra_ca::build_reqwest_client(|builder| {
        builder
            .timeout(Duration::from_millis(timeout_ms))
            // SECURITY: only the initial URL is SSRF-validated; do not follow redirects.
            .redirect(reqwest::redirect::Policy::none())
    })
    .expect("hook HTTP client config is valid")
}

pub async fn run_http_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
    mode: GateKind,
) -> HookRunOutput {
    let start = Instant::now();

    let Some(ref raw_url) = spec.url else {
        return (
            HookRunnerResult::Failed("http hook has no 'url' field".into()),
            start.elapsed(),
            None,
            None,
        );
    };

    let mut url_env = spec.extra_env.clone();
    for (k, v) in [
        ("GROK_HOOK_EVENT", envelope.hook_event_name.to_string()),
        ("GROK_HOOK_NAME", spec.name.clone()),
        ("GROK_SESSION_ID", ctx.session_id.to_string()),
        ("GROK_WORKSPACE_ROOT", ctx.workspace_root.to_string()),
        ("CLAUDE_PROJECT_DIR", ctx.workspace_root.to_string()),
    ] {
        url_env.insert(k.to_string(), v);
    }
    let expanded_url = crate::env_expand::expand_env_vars_with_extra(raw_url, &url_env);
    let url: &str = &expanded_url;
    let log_url: &str = spec.url_raw.as_deref().unwrap_or(url);

    let make_info = |status: Option<u16>, preview: Option<String>| -> HttpInfo {
        HttpInfo {
            expanded_url: url.to_owned(),
            source_url: spec.url_raw.clone(),
            status,
            response_preview: preview,
        }
    };

    let validation = tokio::time::timeout(
        Duration::from_millis(spec.timeout_ms),
        validate_hook_url(url),
    )
    .await
    .unwrap_or_else(|_| {
        Err(format!(
            "URL validation timed out after {}ms",
            spec.timeout_ms
        ))
    });
    if let Err(reason) = validation {
        tracing::warn!(
            hook_name = %spec.name,
            url = %log_url,
            %reason,
            "SSRF protection: blocked HTTP hook URL"
        );
        return (
            HookRunnerResult::Failed(format!("blocked by SSRF protection: {reason}")),
            start.elapsed(),
            Some(make_info(None, None)),
            None,
        );
    }

    let body = match serde_json::to_string(&envelope.to_hook_json()) {
        Ok(j) => j,
        Err(e) => {
            return (
                HookRunnerResult::Failed(format!("failed to serialize envelope: {e}")),
                start.elapsed(),
                Some(make_info(None, None)),
                None,
            );
        }
    };

    let client = build_hook_client(spec.timeout_ms);

    let response = match client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let elapsed = start.elapsed();
            let error = if e.is_timeout() {
                format!("timed out after {}ms", spec.timeout_ms)
            } else {
                // SECURITY: without_url() strips the secret-bearing expanded URL from the error.
                format!("HTTP request failed for {}: {}", log_url, e.without_url())
            };
            return (
                HookRunnerResult::Failed(error),
                elapsed,
                Some(make_info(None, None)),
                None,
            );
        }
    };

    let status = response.status();
    let status_code = status.as_u16();
    let elapsed = start.elapsed();

    tracing::debug!(
        hook_name = %spec.name,
        url = %log_url,
        status = status_code,
        elapsed_ms = elapsed.as_millis() as u64,
        "http hook completed"
    );

    let response_text = match read_body_capped(response).await {
        Ok(t) => t,
        Err(e) => {
            return (
                HookRunnerResult::Failed(format!(
                    "failed to read response body for {}: {}",
                    log_url,
                    e.without_url()
                )),
                elapsed,
                Some(make_info(Some(status_code), None)),
                None,
            );
        }
    };

    let response_preview = if response_text.trim().is_empty() {
        None
    } else {
        Some(truncate_preview(&response_text))
    };

    let http_info = Some(make_info(Some(status_code), response_preview.clone()));
    let system_message = extract_system_message(&response_text);

    let result = match mode {
        GateKind::Tool => parse_http_blocking_result(&response_text, status, &spec.name),
        GateKind::Stop => parse_http_stop_result(&response_text, status, &spec.name),
        GateKind::PostTool => parse_http_post_tool_use_result(&response_text, status, &spec.name),
        GateKind::Prompt => parse_http_prompt_result(&response_text, status, &spec.name),
        GateKind::Observe if status.is_success() => HookRunnerResult::Success,
        GateKind::Observe => HookRunnerResult::Failed(format!("HTTP status {status}")),
    };
    (result, elapsed, http_info, system_message)
}

fn parse_http_stop_result(
    response_text: &str,
    status: reqwest::StatusCode,
    hook_name: &str,
) -> HookRunnerResult {
    if !status.is_success() {
        return HookRunnerResult::Failed(format!("HTTP status {status}"));
    }
    let trimmed = response_text.trim();
    if trimmed.is_empty() {
        return HookRunnerResult::Stop(StopHookOutcome::default());
    }
    match serde_json::from_str::<StopHookJson>(trimmed) {
        Ok(json) => match stop_json_to_outcome(json, hook_name) {
            Ok(outcome) => HookRunnerResult::Stop(outcome),
            Err(err) => HookRunnerResult::Failed(err),
        },
        Err(e) => {
            tracing::warn!(
                hook_name = %hook_name,
                error = %e,
                "could not parse HTTP stop hook response JSON, treating as allow-stop"
            );
            HookRunnerResult::Stop(StopHookOutcome::default())
        }
    }
}

fn parse_http_post_tool_use_result(
    response_text: &str,
    status: reqwest::StatusCode,
    hook_name: &str,
) -> HookRunnerResult {
    if !status.is_success() {
        return HookRunnerResult::Failed(format!("HTTP status {status}"));
    }
    let no_signal = HookRunnerResult::PostToolUse {
        outcome: Default::default(),
        failure: None,
    };
    let trimmed = response_text.trim();
    if trimmed.is_empty() {
        return no_signal;
    }
    match serde_json::from_str::<PostToolUseHookJson>(trimmed) {
        Ok(json) => {
            let parsed = post_tool_use_json_to_outcome(json, hook_name, HookHealth::Healthy);
            HookRunnerResult::PostToolUse {
                outcome: parsed.outcome,
                failure: parsed.failure,
            }
        }
        Err(e) => {
            tracing::warn!(
                hook_name,
                error = %e,
                "could not parse HTTP post_tool_use hook response JSON; carrying no signal"
            );
            no_signal
        }
    }
}

fn parse_http_prompt_result(
    response_text: &str,
    status: reqwest::StatusCode,
    hook_name: &str,
) -> HookRunnerResult {
    if !status.is_success() {
        return HookRunnerResult::Failed(format!("HTTP status {status}"));
    }
    let trimmed = response_text.trim();
    if trimmed.is_empty() {
        return HookRunnerResult::Success;
    }
    match serde_json::from_str::<PromptHookJson>(trimmed) {
        Ok(json) => match prompt_json_to_block(&json, hook_name, None) {
            Ok(Some(reason)) => HookRunnerResult::Block {
                reason,
                hook_name: hook_name.to_string(),
            },
            Ok(None) => HookRunnerResult::Success,
            Err(err) => HookRunnerResult::Failed(err),
        },
        Err(e) => {
            tracing::warn!(
                hook_name = %hook_name,
                error = %e,
                "could not parse HTTP prompt hook response JSON, treating as allow"
            );
            HookRunnerResult::Success
        }
    }
}

fn parse_http_blocking_result(
    response_text: &str,
    status: reqwest::StatusCode,
    hook_name: &str,
) -> HookRunnerResult {
    if response_text.trim().is_empty() {
        if status.is_success() {
            return HookRunnerResult::Allow {
                updated_input: None,
                additional_context: None,
            };
        }
        return HookRunnerResult::Failed(format!("HTTP status {status} with empty body"));
    }

    match serde_json::from_str::<super::GateHookJson>(response_text) {
        Ok(json) if json.is_gate_document() => {
            let health = HookHealth::from_success(status.is_success());
            match super::gate_outcome(json, hook_name, /* fallback_reason */ None, health) {
                GateOutcome::Deny(reason) => HookRunnerResult::Deny {
                    reason,
                    hook_name: hook_name.to_string(),
                },
                GateOutcome::Ask {
                    reason,
                    updated_input,
                    additional_context,
                } => HookRunnerResult::Ask {
                    reason,
                    updated_input,
                    additional_context,
                },
                GateOutcome::Defer => HookRunnerResult::Defer,
                GateOutcome::Allow {
                    updated_input,
                    additional_context,
                } => HookRunnerResult::Allow {
                    updated_input,
                    additional_context,
                },
                GateOutcome::Failed(err) => {
                    HookRunnerResult::Failed(format!("{err} (HTTP status {status})"))
                }
            }
        }
        Ok(_) if status.is_success() => HookRunnerResult::Allow {
            updated_input: None,
            additional_context: None,
        },
        Err(e) if status.is_success() => {
            tracing::warn!(
                hook_name,
                error = %e,
                "could not parse HTTP hook response JSON, treating as allow"
            );
            HookRunnerResult::Allow {
                updated_input: None,
                additional_context: None,
            }
        }
        Ok(_) => HookRunnerResult::Failed(format!("HTTP status {status} with non-decision body")),
        Err(e) => HookRunnerResult::Failed(format!(
            "HTTP status {status} and failed to parse response: {e}"
        )),
    }
}

fn truncate_preview(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= RESPONSE_PREVIEW_MAX {
        trimmed.to_string()
    } else {
        let boundary = trimmed
            .char_indices()
            .take_while(|&(i, _)| i <= RESPONSE_PREVIEW_MAX)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        let Some(prefix) = trimmed.get(..boundary) else {
            return trimmed.to_string();
        };
        let mut preview = prefix.to_string();
        preview.push_str("...");
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn http_updated_input_applies() {
        let result = parse_http_blocking_result(
            r#"{"hookSpecificOutput":{"updatedInput":{"command":"echo hi"}}}"#,
            StatusCode::OK,
            "test-hook",
        );
        match result {
            HookRunnerResult::Allow {
                updated_input: Some(rewrite),
                ..
            } => assert_eq!(
                rewrite.get("command").and_then(|v| v.as_str()),
                Some("echo hi")
            ),
            other => panic!("expected Allow with updatedInput, got {other:?}"),
        }
    }

    #[test]
    fn http_error_status_drops_a_gate_document_rewrite() {
        let result = parse_http_blocking_result(
            r#"{"hookSpecificOutput":{"updatedInput":{"command":"echo hi"}}}"#,
            StatusCode::INTERNAL_SERVER_ERROR,
            "test-hook",
        );
        assert!(matches!(
            result,
            HookRunnerResult::Allow {
                updated_input: None,
                ..
            }
        ));
    }

    #[test]
    fn http_non_object_updated_input_drops_rewrite_keeps_decision() {
        let allow = parse_http_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":"nope"}}"#,
            StatusCode::OK,
            "test-hook",
        );
        assert!(
            matches!(
                allow,
                HookRunnerResult::Allow {
                    updated_input: None,
                    ..
                }
            ),
            "a non-object updatedInput on an allow must drop the rewrite and keep allowing"
        );

        let ask = parse_http_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"confirm","updatedInput":"nope"}}"#,
            StatusCode::OK,
            "test-hook",
        );
        match ask {
            HookRunnerResult::Ask {
                reason,
                updated_input: None,
                ..
            } => assert_eq!(reason.as_deref(), Some("confirm")),
            other => panic!("a non-object updatedInput on an ask must keep asking, got {other:?}"),
        }
    }

    #[test]
    fn http_ask_json_carries_reason_and_rewrite() {
        let result = parse_http_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"confirm","updatedInput":{"command":"echo hi"}}}"#,
            StatusCode::OK,
            "test-hook",
        );
        match result {
            HookRunnerResult::Ask {
                reason,
                updated_input: Some(rewrite),
                ..
            } => {
                assert_eq!(reason.as_deref(), Some("confirm"));
                assert_eq!(
                    rewrite.get("command").and_then(|v| v.as_str()),
                    Some("echo hi")
                );
            }
            other => panic!("expected Ask with rewrite, got {other:?}"),
        }
    }

    #[test]
    fn http_defer_and_additional_context() {
        assert!(matches!(
            parse_http_blocking_result(
                r#"{"hookSpecificOutput":{"permissionDecision":"defer"}}"#,
                StatusCode::OK,
                "test-hook",
            ),
            HookRunnerResult::Defer
        ));
        match parse_http_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"allow","additionalContext":"note"}}"#,
            StatusCode::OK,
            "test-hook",
        ) {
            HookRunnerResult::Allow {
                additional_context, ..
            } => assert_eq!(additional_context.as_deref(), Some("note")),
            other => panic!("expected Allow with additionalContext, got {other:?}"),
        }
    }

    #[test]
    fn http_broken_status_keeps_allow_but_drops_rewrite() {
        let result = parse_http_blocking_result(
            r#"{"hookSpecificOutput":{"updatedInput":{"command":"echo hi"}}}"#,
            StatusCode::INTERNAL_SERVER_ERROR,
            "test-hook",
        );
        assert!(matches!(
            result,
            HookRunnerResult::Allow {
                updated_input: None,
                ..
            }
        ));
    }

    #[test]
    fn http_non_object_updated_input_drops_the_rewrite() {
        let result = parse_http_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":"nope"}}"#,
            StatusCode::OK,
            "test-hook",
        );
        assert!(
            matches!(
                result,
                HookRunnerResult::Allow {
                    updated_input: None,
                    ..
                }
            ),
            "got: {result:?}"
        );

        let ask = parse_http_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"confirm","updatedInput":"nope"}}"#,
            StatusCode::OK,
            "test-hook",
        );
        match ask {
            HookRunnerResult::Ask {
                reason,
                updated_input: None,
                ..
            } => assert_eq!(reason.as_deref(), Some("confirm")),
            other => panic!("a non-object updatedInput on an ask must keep asking, got {other:?}"),
        }
    }

    #[test]
    fn http_deny_carries_reason_or_generic() {
        match parse_http_blocking_result(
            r#"{"decision":"deny","reason":"dangerous command"}"#,
            StatusCode::OK,
            "test-hook",
        ) {
            HookRunnerResult::Deny { reason, hook_name } => {
                assert_eq!(reason, "dangerous command");
                assert_eq!(hook_name, "test-hook");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
        match parse_http_blocking_result(r#"{"decision":"deny"}"#, StatusCode::OK, "my-hook") {
            HookRunnerResult::Deny { reason, .. } => {
                assert!(reason.contains("my-hook"), "got: {reason}")
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn http_deny_honored_on_non_2xx() {
        match parse_http_blocking_result(
            r#"{"decision":"deny","reason":"forbidden"}"#,
            StatusCode::FORBIDDEN,
            "test-hook",
        ) {
            HookRunnerResult::Deny { reason, .. } => assert_eq!(reason, "forbidden"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn http_prompt_block_reason_or_generic() {
        match parse_http_prompt_result(
            r#"{"decision":"block","reason":"policy says no"}"#,
            StatusCode::OK,
            "prompt-hook",
        ) {
            HookRunnerResult::Block { reason, hook_name } => {
                assert_eq!(reason, "policy says no");
                assert_eq!(hook_name, "prompt-hook");
            }
            other => panic!("expected Block (prompt block), got {other:?}"),
        }
        match parse_http_prompt_result(r#"{"decision":"block"}"#, StatusCode::OK, "prompt-hook") {
            HookRunnerResult::Block { reason, .. } => assert!(reason.contains("prompt-hook")),
            other => panic!("expected Block (prompt block), got {other:?}"),
        }
    }

    #[test]
    fn http_prompt_allows_on_success_without_verdict() {
        for body in [
            "",
            "plain text",
            "{}",
            r#"{"hookSpecificOutput":{"additionalContext":"ctx"}}"#,
            "not json at all",
        ] {
            let result = parse_http_prompt_result(body, StatusCode::OK, "prompt-hook");
            assert!(
                matches!(result, HookRunnerResult::Success),
                "body {body:?} must allow"
            );
        }
    }

    #[test]
    fn http_prompt_unknown_decision_fails() {
        let result =
            parse_http_prompt_result(r#"{"decision":"deny"}"#, StatusCode::OK, "prompt-hook");
        assert!(matches!(result, HookRunnerResult::Failed(_)));
    }

    #[test]
    fn http_prompt_error_status_fails_regardless_of_body() {
        let result = parse_http_prompt_result(
            r#"{"decision":"block","reason":"x"}"#,
            StatusCode::INTERNAL_SERVER_ERROR,
            "prompt-hook",
        );
        assert!(matches!(result, HookRunnerResult::Failed(_)));
    }

    /// The error names the bad value only; `HookRunResult::Failed.hook_name` carries the attribution.
    #[test]
    fn http_unknown_decision_is_failed() {
        match parse_http_blocking_result(r#"{"decision":"maybe"}"#, StatusCode::OK, "test-hook") {
            HookRunnerResult::Failed(msg) => {
                assert!(
                    msg.contains("maybe") && !msg.contains("test-hook"),
                    "got: {msg}"
                )
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_2xx_non_decision_body_fails_open() {
        for body in [
            "",
            "   \n  ",
            "not json at all",
            r#"{"decision":"deny""#,
            r#"{"detail":"ok"}"#,
        ] {
            let result = parse_http_blocking_result(body, StatusCode::OK, "test-hook");
            assert!(
                matches!(result, HookRunnerResult::Allow { .. }),
                "for {body:?}"
            );
        }
    }

    #[test]
    fn http_non_2xx_non_decision_body_fails() {
        for body in ["", "not json", r#"{"detail":"Not Found"}"#] {
            let result =
                parse_http_blocking_result(body, StatusCode::INTERNAL_SERVER_ERROR, "test-hook");
            match result {
                HookRunnerResult::Failed(msg) => {
                    assert!(msg.contains("500"), "for {body:?}: {msg}")
                }
                other => panic!("expected Failed for {body:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn http_stop_status_and_body_handling() {
        match parse_http_stop_result(
            r#"{"decision":"block","reason":"tests failing"}"#,
            StatusCode::OK,
            "s",
        ) {
            HookRunnerResult::Stop(o) => {
                assert_eq!(o.block_reason.as_deref(), Some("tests failing"));
            }
            other => panic!("expected Stop, got {other:?}"),
        }
        match parse_http_stop_result("", StatusCode::OK, "s") {
            HookRunnerResult::Stop(o) => assert!(o.is_empty()),
            other => panic!("expected Stop, got {other:?}"),
        }
        match parse_http_stop_result("not json", StatusCode::OK, "s") {
            HookRunnerResult::Stop(o) => assert!(o.is_empty()),
            other => panic!("expected Stop, got {other:?}"),
        }
        assert!(matches!(
            parse_http_stop_result(r#"{"decision":"deny"}"#, StatusCode::OK, "s"),
            HookRunnerResult::Failed(_)
        ));
        assert!(matches!(
            parse_http_stop_result(
                r#"{"decision":"block"}"#,
                StatusCode::INTERNAL_SERVER_ERROR,
                "s"
            ),
            HookRunnerResult::Failed(_)
        ));
    }

    #[test]
    fn http_post_tool_use_status_and_body_handling() {
        match parse_http_post_tool_use_result(
            r#"{"decision":"block","reason":"needs review","hookSpecificOutput":{"updatedToolOutput":{"stdout":"clean"}}}"#,
            StatusCode::OK,
            "p",
        ) {
            HookRunnerResult::PostToolUse { outcome, failure } => {
                assert!(failure.is_none());
                assert_eq!(outcome.block_reason.as_deref(), Some("needs review"));
                match outcome.output_replacement {
                    Some(crate::result::OutputReplacement {
                        kind: crate::result::ReplacementKind::Builtin,
                        value,
                        ..
                    }) => {
                        assert_eq!(value, serde_json::json!({"stdout":"clean"}));
                    }
                    other => panic!("expected a builtin replacement, got {other:?}"),
                }
            }
            other => panic!("expected PostToolUse, got {other:?}"),
        }

        assert!(
            matches!(
                parse_http_post_tool_use_result(
                    r#"{"decision":"block","reason":"x"}"#,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "p",
                ),
                HookRunnerResult::Failed(_)
            ),
            "a non-2xx must fail"
        );

        for body in ["", "   \n  ", "not json at all"] {
            match parse_http_post_tool_use_result(body, StatusCode::OK, "p") {
                HookRunnerResult::PostToolUse { outcome, failure } => {
                    assert!(failure.is_none(), "for {body:?}");
                    assert!(outcome.is_empty(), "for {body:?}: {outcome:?}");
                }
                other => panic!("expected no-signal PostToolUse for {body:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn http_empty_body_success_allows() {
        for body in ["", "   \n  "] {
            let result = parse_http_blocking_result(body, StatusCode::OK, "test-hook");
            assert!(matches!(result, HookRunnerResult::Allow { .. }));
        }
    }

    #[test]
    fn http_empty_body_error_status_fails() {
        let result = parse_http_blocking_result("", StatusCode::INTERNAL_SERVER_ERROR, "test-hook");
        match result {
            HookRunnerResult::Failed(msg) => {
                assert!(msg.contains("500"));
                assert!(msg.contains("empty body"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_invalid_json_success_status_fail_open() {
        for body in ["not json at all", r#"{"decision":"deny""#] {
            let result = parse_http_blocking_result(body, StatusCode::OK, "test-hook");
            assert!(matches!(result, HookRunnerResult::Allow { .. }));
        }
    }

    #[test]
    fn http_invalid_json_error_status_fails() {
        let result =
            parse_http_blocking_result("not json", StatusCode::INTERNAL_SERVER_ERROR, "test-hook");
        match result {
            HookRunnerResult::Failed(msg) => {
                assert!(msg.contains("500"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_deny_with_non_success_status() {
        let result = parse_http_blocking_result(
            r#"{"decision":"deny","reason":"forbidden"}"#,
            StatusCode::FORBIDDEN,
            "test-hook",
        );
        match result {
            HookRunnerResult::Deny { reason, .. } => {
                assert_eq!(reason, "forbidden");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn is_blocked_ip_classifies_ranges() {
        let blocked = [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "169.254.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "::",
            "fe80::1",
            "fc00::1",
            "fd00::1",
            "::ffff:10.0.0.1",
            "::ffff:192.168.1.1",
        ];
        let allowed = [
            "172.15.0.1",
            "172.32.0.1",
            "100.63.0.1",
            "127.0.0.1",
            "::1",
            "1.1.1.1",
            "8.8.8.8",
        ];
        for ip in blocked {
            assert!(
                is_blocked_ip(&ip.parse::<IpAddr>().unwrap()),
                "{ip} must be blocked"
            );
        }
        for ip in allowed {
            assert!(
                !is_blocked_ip(&ip.parse::<IpAddr>().unwrap()),
                "{ip} must be allowed"
            );
        }
    }

    #[tokio::test]
    async fn validate_hook_url_accepts_and_rejects() {
        validate_hook_url("https://1.1.1.1/hook")
            .await
            .expect("public https IP must pass");
        for (url, needle) in [
            ("http://example.com/hook", "https://"),
            ("ftp://example.com/hook", "https://"),
            ("https://10.0.0.1/hook", "blocked"),
            ("https://169.254.169.254/latest/meta-data/", "blocked"),
            ("not a url", "invalid URL"),
            ("https://nonexistent.invalid/hook", "DNS resolution failed"),
        ] {
            let err = validate_hook_url(url).await.expect_err("must reject");
            assert!(err.contains(needle), "for {url}, got: {err}");
        }
    }

    use crate::config::HookSpec;
    use crate::event::{HookEventEnvelope, HookEventName, HookPayload};
    use crate::test_support::with_env_var;

    fn http_pre_tool_use_spec(
        raw_url: &str,
        timeout_ms: u64,
        extra_env: std::collections::HashMap<String, String>,
    ) -> HookSpec {
        HookSpec {
            name: "test-http-hook".into(),
            event: HookEventName::PreToolUse,
            handler_type: crate::config::HandlerType::Http,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: None,
            command_raw: None,
            url: Some(raw_url.to_string()),
            url_raw: Some(raw_url.to_string()),
            timeout_ms,
            source_dir: std::env::temp_dir(),
            extra_env,
            layer: crate::config::HookProvenance::File,
        }
    }

    fn http_pre_tool_use_envelope() -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: HookEventName::PreToolUse,
            session_id: "test".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::PreToolUse {
                tool_name: "test".into(),
                tool_use_id: "id-1".into(),
                tool_input: serde_json::json!({}),
                tool_input_truncated: false,
                subagent_type: None,
            },
        }
    }

    fn http_test_ctx() -> crate::runner::RunContext<'static> {
        crate::runner::RunContext {
            session_id: "test",
            workspace_root: "/tmp",
            process_scope: None,
            disabled: Default::default(),
        }
    }

    #[tokio::test]
    async fn run_http_hook_uses_post_expansion_url_for_ssrf() {
        let mut extra_env = std::collections::HashMap::new();
        extra_env.insert("INTERNAL_HOST_SSRF".to_string(), "10.0.0.1".to_string());

        let url_template = "https://${INTERNAL_HOST_SSRF}/hook";
        let spec = http_pre_tool_use_spec(url_template, 1000, extra_env);
        let envelope = http_pre_tool_use_envelope();
        let ctx = http_test_ctx();
        let (result, _, info, _) = run_http_hook(&spec, &envelope, &ctx, GateKind::Tool).await;

        match result {
            crate::runner::HookRunnerResult::Failed(reason) => {
                assert!(
                    reason.contains("blocked") || reason.contains("SSRF"),
                    "expected SSRF block message, got: {reason}"
                );
            }
            other => panic!("expected SSRF Failed, got {other:?}"),
        }

        let info = info.expect("HttpInfo should be present for SSRF block path");
        assert_eq!(
            info.expanded_url, "https://10.0.0.1/hook",
            "HttpInfo.expanded_url must reflect the post-expansion URL (the actual target SSRF blocked)"
        );
        assert_eq!(
            info.source_url.as_deref(),
            Some("https://${INTERNAL_HOST_SSRF}/hook"),
            "HttpInfo.source_url must mirror HookSpec::url_raw"
        );
    }

    #[tokio::test]
    async fn run_http_hook_scrubs_url_from_reqwest_error() {
        let secret = "ghp_VERY_REAL_SECRET_TOKEN_42";
        let mut extra_env = std::collections::HashMap::new();
        extra_env.insert("RUNTIME_HOST".to_string(), "192.0.2.1".to_string());
        extra_env.insert("MY_TOKEN".to_string(), secret.to_string());

        let url_template = "https://${RUNTIME_HOST}/check?token=${MY_TOKEN}";
        let spec = http_pre_tool_use_spec(url_template, 500, extra_env);
        let envelope = http_pre_tool_use_envelope();
        let ctx = http_test_ctx();

        let (result, _, info, _) = run_http_hook(&spec, &envelope, &ctx, GateKind::Tool).await;

        let error_text = match result {
            crate::runner::HookRunnerResult::Failed(reason) => reason,
            other => panic!("expected Failed, got {other:?}"),
        };

        assert!(
            !error_text.contains(secret),
            "secret leaked into error text: {error_text}"
        );

        if !error_text.contains("timed out") {
            assert!(
                error_text.contains("${RUNTIME_HOST}") || error_text.contains("${MY_TOKEN}"),
                "expected error to reference the raw URL form, got: {error_text}"
            );
        }

        let info = info.expect("HttpInfo should be present for connection failures too");
        assert_eq!(
            info.expanded_url,
            "https://192.0.2.1/check?token=ghp_VERY_REAL_SECRET_TOKEN_42"
        );
        assert_eq!(info.source_url.as_deref(), Some(url_template));
    }

    #[tokio::test]
    async fn hook_client_does_not_follow_redirects() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                server_requests.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let response = "HTTP/1.1 302 Found\r\n\
                     Location: http://169.254.169.254/latest/meta-data/\r\n\
                     Content-Length: 0\r\n\r\n";
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        let client = build_hook_client(5000);
        let resp = client
            .post(format!("http://{addr}/hook"))
            .body("{}")
            .send()
            .await
            .expect("request should succeed without following the redirect");

        assert_eq!(
            resp.status().as_u16(),
            302,
            "redirect must be surfaced, not followed"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "client must not issue a second request to the redirect target"
        );
    }

    #[tokio::test]
    async fn url_unresolved_var_fails_validation() {
        let key = "GROK_HOOKS_HTTP_TEST_UNRESOLVED";
        let expanded = with_env_var(key, None, || {
            let extra = std::collections::HashMap::new();
            crate::env_expand::expand_env_vars_with_extra(
                &format!("https://${{{key}}}/check"),
                &extra,
            )
        });
        assert!(expanded.contains(&format!("${{{key}}}")));
        let result = validate_hook_url(&expanded).await;
        assert!(result.is_err(), "expected invalid URL error, got Ok");
    }
}
