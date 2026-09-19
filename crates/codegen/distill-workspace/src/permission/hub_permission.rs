// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::LazyLock;

use async_trait::async_trait;
use prometheus::{HistogramVec, IntCounter, register_histogram_vec, register_int_counter};
use serde_json::Value;
use distill_computer_hub_sdk::harness::PERMISSION_REQUEST_KIND;
use distill_computer_hub_sdk::{ToolServer, WeakToolServer};
use distill_tool_protocol::SessionId;
use distill_tool_runtime::ToolApprovalPolicy;

use crate::permission::prompter::{PromptOutcome, tool_name_for_access};
use crate::permission::types::{AccessKind, HookAsk};

static PERMISSION_REPLY_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "grok_workspace_permission_reply_seconds",
        "Wall-clock time awaiting chat's reply to a permission_request hook",
        &["outcome"],
        vec![0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0]
    )
    .expect("grok_workspace_permission_reply_seconds must register once")
});

static PERMISSION_TIMEOUT_TOTAL: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "grok_workspace_permission_timeout_total",
        "permission_request hooks whose reply timed out (backstop deadline fired)"
    )
    .expect("grok_workspace_permission_timeout_total must register once")
});

pub(crate) fn init_metrics() {
    for outcome in ["ok", "error"] {
        let _ = PERMISSION_REPLY_DURATION.with_label_values(&[outcome]);
    }
    PERMISSION_TIMEOUT_TOTAL.inc_by(0);
}

fn is_timeout_err(msg: &str) -> bool {
    msg.contains("timed out")
}

/// Opt-in for the hub prompt path where it is not on by construction: the sandbox guest. Daemon hosts
/// ignore it (see `approval_gate_for`).
pub const HITL_PERMISSION_LIVE_ENV: &str = "GROK_HITL_PERMISSION_LIVE";

pub fn hitl_permission_live_enabled() -> bool {
    distill_config::env_bool(HITL_PERMISSION_LIVE_ENV) == Some(true)
}

#[async_trait]
pub trait PermissionHookTransport: Send + Sync {
    async fn request_permission(&self, payload: Value) -> Result<Value, String>;
}

pub struct ToolServerPermissionTransport {
    server: WeakToolServer,
    session_id: SessionId,
}

impl ToolServerPermissionTransport {
    pub fn new(server: ToolServer, session_id: SessionId) -> Self {
        Self {
            server: server.downgrade(),
            session_id,
        }
    }

    pub fn from_session_id(server: ToolServer, session_id: &str) -> Option<Self> {
        SessionId::new(session_id)
            .ok()
            .map(|sid| Self::new(server, sid))
    }
}

#[async_trait]
impl PermissionHookTransport for ToolServerPermissionTransport {
    async fn request_permission(&self, payload: Value) -> Result<Value, String> {
        let start = std::time::Instant::now();
        let Some(server) = self.server.upgrade() else {
            PERMISSION_REPLY_DURATION
                .with_label_values(&["error"])
                .observe(start.elapsed().as_secs_f64());
            return Err("tool server gone (weak upgrade failed)".to_owned());
        };
        let reply_result = server
            .request_hook(
                self.session_id.clone(),
                PERMISSION_REQUEST_KIND.to_owned(),
                payload,
            )
            .await;
        let outcome = match &reply_result {
            Ok(_) => "ok",
            Err(e) => {
                if is_timeout_err(&e.to_string()) {
                    PERMISSION_TIMEOUT_TOTAL.inc();
                }
                "error"
            }
        };
        PERMISSION_REPLY_DURATION
            .with_label_values(&[outcome])
            .observe(start.elapsed().as_secs_f64());
        reply_result.map_err(|e| e.to_string())
    }
}

fn scope_for_access(access: &AccessKind) -> &'static str {
    match access {
        AccessKind::Bash(_)
        | AccessKind::Edit(_)
        | AccessKind::MCPTool { .. }
        | AccessKind::AgentMessage { .. }
        | AccessKind::Tool(_) => "write",
        AccessKind::Read(_)
        | AccessKind::Grep { .. }
        | AccessKind::WebFetch(_)
        | AccessKind::WebSearch(_) => "read",
    }
}

fn describe_access(access: &AccessKind) -> String {
    match access {
        AccessKind::Bash(_) => "Run a terminal command".to_owned(),
        AccessKind::Edit(path) => format!("Edit {path}"),
        AccessKind::MCPTool { name, .. } => format!("Run MCP tool {name}"),
        AccessKind::WebFetch(url) => format!("Fetch {url}"),
        AccessKind::WebSearch(query) => format!("Search the web for {query}"),
        AccessKind::Read(_) => "Read a file".to_owned(),
        AccessKind::Grep { .. } => "Search file contents".to_owned(),
        AccessKind::AgentMessage { subagent_id } => {
            format!("Send a message to subagent {subagent_id}")
        }
        AccessKind::Tool(name) => format!("Run {name}"),
    }
}

/// `tool_approval_policy` tells the renderer which answers will be honoured: under `always_prompt`
/// an "always" answer is recorded nowhere, so the card must not offer it.
pub(crate) fn build_permission_payload(
    access: &AccessKind,
    tool_call_id: &str,
    hook_ask: Option<&HookAsk>,
    policy: ToolApprovalPolicy,
) -> Value {
    let description = match hook_ask {
        Some(ask) => ask.prompt_header(&describe_access(access)),
        None => describe_access(access),
    };
    let mut payload = serde_json::json!({
        "tool_call_id": tool_call_id,
        "tool_name": tool_name_for_access(access),
        "description": description,
        "scope": scope_for_access(access),
        "tool_approval_policy": policy,
    });
    if let Some(map) = payload.as_object_mut() {
        match access {
            AccessKind::Bash(command) => {
                map.insert("bash_command".to_owned(), Value::from(command.clone()));
            }
            AccessKind::AgentMessage { subagent_id } => {
                map.insert("subagent_id".to_owned(), Value::from(subagent_id.clone()));
            }
            AccessKind::Edit(path) => {
                map.insert(
                    "edit_file_paths".to_owned(),
                    Value::from(vec![path.clone()]),
                );
            }
            _ => {}
        }
    }
    payload
}

/// The reply's outcome and scope as a [`PromptOutcome`]. `access` resolves the one scope that
/// names no value on the wire: `tool_scope` ("this specific tool") is the access's own MCP tool.
pub(crate) fn reply_to_outcome(reply: &Value, access: &AccessKind) -> PromptOutcome {
    let outcome = match reply.get("outcome") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(1) => "approve",
            Some(2) => "reject",
            Some(3) => "always_approve",
            Some(4) => "always_reject",
            _ => "",
        },
        _ => "",
    };
    let followup = reply
        .get("followup_message")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    match outcome {
        "approve" => PromptOutcome::AllowOnce,
        "always_approve" => match scope_kind_value(reply) {
            Some(("bash_command", Some(value))) => PromptOutcome::AllowAlwaysBashCommand(value),
            Some(("bash_glob", Some(value))) => PromptOutcome::AllowAlwaysBashGlob(value),
            Some(("server_prefix", Some(value))) => PromptOutcome::AllowAlwaysMcpServer(value),
            Some(("domain", Some(value))) => PromptOutcome::AllowAlwaysDomain(value),
            _ => PromptOutcome::AllowAlways,
        },
        "reject" => match followup {
            Some(message) => PromptOutcome::FollowupMessage(message.to_owned()),
            None => PromptOutcome::RejectOnce,
        },
        "always_reject" => match (scope_kind_value(reply), access) {
            (Some(("bash_command", Some(value))), _) => {
                PromptOutcome::RejectAlwaysBashCommand(value)
            }
            (Some(("domain", Some(value))), _) => PromptOutcome::RejectAlwaysDomain(value),
            (Some(("tool_scope", _)), AccessKind::MCPTool { name, .. }) => {
                PromptOutcome::RejectAlwaysMcpTool(name.clone())
            }
            _ => PromptOutcome::RejectOnce,
        },
        "cancelled" => PromptOutcome::Cancelled,
        _ => PromptOutcome::RejectOnce,
    }
}

fn scope_kind_value(reply: &Value) -> Option<(&str, Option<String>)> {
    let scope = reply.get("scope")?;
    let kind = scope.get("kind").and_then(Value::as_str)?;
    let value = scope
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((kind, value))
}

pub fn prompt_outcome_allows(outcome: &PromptOutcome) -> bool {
    matches!(
        outcome,
        PromptOutcome::AllowOnce
            | PromptOutcome::AllowAlways
            | PromptOutcome::AllowEditsForSession
            | PromptOutcome::AllowAlwaysBashCommand(_)
            | PromptOutcome::AllowAlwaysBashGlob(_)
            | PromptOutcome::AllowAlwaysDomain(_)
            | PromptOutcome::AllowAlwaysMcpTool(_)
            | PromptOutcome::AllowAlwaysMcpServer(_)
    )
}

pub async fn request_permission_via_hub(
    transport: &dyn PermissionHookTransport,
    access: &AccessKind,
    tool_call_id: &str,
    hook_ask: Option<&HookAsk>,
    policy: ToolApprovalPolicy,
) -> PromptOutcome {
    let payload = build_permission_payload(access, tool_call_id, hook_ask, policy);
    match transport.request_permission(payload).await {
        Ok(reply) => match reply_to_outcome(&reply, access) {
            PromptOutcome::AllowAlways if matches!(access, AccessKind::Edit(_)) => {
                PromptOutcome::AllowEditsForSession
            }
            PromptOutcome::AllowAlways
                if matches!(
                    access,
                    AccessKind::AgentMessage { .. } | AccessKind::Tool(_)
                ) =>
            {
                PromptOutcome::AllowOnce
            }
            other => other,
        },
        Err(e) => {
            tracing::error!(error = %e, "hub permission request failed; rejecting");
            PromptOutcome::Error(format!("hub permission request failed: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn any_access() -> AccessKind {
        AccessKind::Bash("ls".into())
    }

    #[test]
    fn is_timeout_err_matches_backstop_wording_only() {
        assert!(is_timeout_err("request timed out after 600s"));
        assert!(is_timeout_err("request timed out after 600.0s"));
        assert!(!is_timeout_err("connection lost"));
        assert!(!is_timeout_err("tool server gone (weak upgrade failed)"));
    }

    #[test]
    fn payload_for_bash_carries_command_and_write_scope() {
        let payload = build_permission_payload(
            &AccessKind::Bash("rm -rf /tmp/x".into()),
            "tc-1",
            /*hook_ask=*/ None,
            ToolApprovalPolicy::GrantsAllowed,
        );
        assert_eq!(
            payload
                .get("tool_call_id")
                .unwrap_or(&serde_json::Value::Null),
            "tc-1"
        );
        assert_eq!(
            payload.get("tool_name").unwrap_or(&serde_json::Value::Null),
            "run_terminal_command"
        );
        assert_eq!(
            payload
                .get("description")
                .unwrap_or(&serde_json::Value::Null),
            "Run a terminal command"
        );
        assert_eq!(
            payload.get("scope").unwrap_or(&serde_json::Value::Null),
            "write"
        );
        assert_eq!(
            payload
                .get("bash_command")
                .unwrap_or(&serde_json::Value::Null),
            "rm -rf /tmp/x"
        );
        assert!(payload.get("edit_file_paths").is_none());
    }

    #[test]
    fn payload_description_carries_the_hook_ask() {
        let payload = build_permission_payload(
            &AccessKind::Bash("deploy".into()),
            "tc-ask",
            Some(&HookAsk {
                hook_name: "guard".to_owned(),
                reason: Some("confirm this".to_owned()),
            }),
            ToolApprovalPolicy::GrantsAllowed,
        );
        assert_eq!(
            payload
                .get("description")
                .unwrap_or(&serde_json::Value::Null),
            "Run a terminal command — hook 'guard' asks: confirm this"
        );
    }

    #[test]
    fn payload_for_agent_message_has_dedicated_content_free_identity() {
        let payload = build_permission_payload(
            &AccessKind::AgentMessage {
                subagent_id: "sub-1".into(),
            },
            "tc-message",
            /*hook_ask=*/ None,
            ToolApprovalPolicy::GrantsAllowed,
        );
        assert_eq!(
            payload.get("tool_name").unwrap_or(&serde_json::Value::Null),
            "send_subagent_message"
        );
        assert_eq!(
            payload
                .get("description")
                .unwrap_or(&serde_json::Value::Null),
            "Send a message to subagent sub-1"
        );
        assert_eq!(
            payload.get("scope").unwrap_or(&serde_json::Value::Null),
            "write"
        );
        assert_eq!(
            payload
                .get("subagent_id")
                .unwrap_or(&serde_json::Value::Null),
            "sub-1"
        );
        assert!(payload.get("edit_file_paths").is_none());
        assert!(payload.get("bash_command").is_none());
    }

    #[test]
    fn payload_for_edit_carries_file_paths() {
        let payload = build_permission_payload(
            &AccessKind::Edit("src/main.rs".into()),
            "tc-2",
            /*hook_ask=*/ None,
            ToolApprovalPolicy::GrantsAllowed,
        );
        assert_eq!(
            payload.get("tool_name").unwrap_or(&serde_json::Value::Null),
            "search_replace"
        );
        assert_eq!(
            payload
                .get("description")
                .unwrap_or(&serde_json::Value::Null),
            "Edit src/main.rs"
        );
        assert_eq!(
            payload.get("scope").unwrap_or(&serde_json::Value::Null),
            "write"
        );
        assert_eq!(
            payload
                .get("edit_file_paths")
                .unwrap_or(&serde_json::Value::Null),
            &serde_json::json!(["src/main.rs"])
        );
        assert!(payload.get("bash_command").is_none());
        assert!(payload.get("edit_kind").is_none());
    }

    #[test]
    fn payload_for_mcp_has_no_tool_context() {
        let payload = build_permission_payload(
            &AccessKind::MCPTool {
                name: "linear__list".into(),
                input: serde_json::Value::Null,
            },
            "tc-3",
            /*hook_ask=*/ None,
            ToolApprovalPolicy::GrantsAllowed,
        );
        assert_eq!(
            payload.get("tool_name").unwrap_or(&serde_json::Value::Null),
            "mcp:linear__list"
        );
        assert_eq!(
            payload
                .get("description")
                .unwrap_or(&serde_json::Value::Null),
            "Run MCP tool linear__list"
        );
        assert_eq!(
            payload.get("scope").unwrap_or(&serde_json::Value::Null),
            "write"
        );
        assert!(payload.get("bash_command").is_none());
        assert!(payload.get("edit_file_paths").is_none());
    }

    #[test]
    fn reply_outcomes_map_to_prompt_outcomes() {
        assert!(matches!(
            reply_to_outcome(&serde_json::json!({ "outcome": "approve" }), &any_access()),
            PromptOutcome::AllowOnce
        ));
        assert!(matches!(
            reply_to_outcome(&serde_json::json!({ "outcome": "reject" }), &any_access()),
            PromptOutcome::RejectOnce
        ));
        assert!(matches!(
            reply_to_outcome(
                &serde_json::json!({ "outcome": "cancelled" }),
                &any_access()
            ),
            PromptOutcome::Cancelled
        ));
        assert!(matches!(
            reply_to_outcome(
                &serde_json::json!({ "outcome": "unspecified" }),
                &any_access()
            ),
            PromptOutcome::RejectOnce
        ));
        assert!(matches!(
            reply_to_outcome(&serde_json::json!({}), &any_access()),
            PromptOutcome::RejectOnce
        ));
    }

    #[test]
    fn reject_with_followup_routes_message_to_model() {
        let reply =
            serde_json::json!({ "outcome": "reject", "followup_message": "use cargo instead" });
        match reply_to_outcome(&reply, &any_access()) {
            PromptOutcome::FollowupMessage(m) => assert_eq!(m, "use cargo instead"),
            other => panic!("expected FollowupMessage, got {other:?}"),
        }
    }

    #[test]
    fn always_approve_maps_scope_to_persistent_outcome() {
        let bash = serde_json::json!({
            "outcome": "always_approve",
            "scope": { "kind": "bash_command", "value": "cargo build" },
        });
        match reply_to_outcome(&bash, &any_access()) {
            PromptOutcome::AllowAlwaysBashCommand(v) => assert_eq!(v, "cargo build"),
            other => panic!("expected AllowAlwaysBashCommand, got {other:?}"),
        }
        let server = serde_json::json!({
            "outcome": "always_approve",
            "scope": { "kind": "server_prefix", "value": "linear" },
        });
        match reply_to_outcome(&server, &any_access()) {
            PromptOutcome::AllowAlwaysMcpServer(v) => assert_eq!(v, "linear"),
            other => panic!("expected AllowAlwaysMcpServer, got {other:?}"),
        }
        assert!(matches!(
            reply_to_outcome(
                &serde_json::json!({ "outcome": "always_approve" }),
                &any_access()
            ),
            PromptOutcome::AllowAlways
        ));
    }

    #[test]
    fn always_reject_with_bash_scope_persists_the_denied_prefix() {
        let reply = serde_json::json!({
            "outcome": "always_reject",
            "scope": { "kind": "bash_command", "value": "curl" },
        });
        match reply_to_outcome(&reply, &any_access()) {
            PromptOutcome::RejectAlwaysBashCommand(v) => assert_eq!(v, "curl"),
            other => panic!("expected RejectAlwaysBashCommand, got {other:?}"),
        }
    }

    /// The wire's `tool_scope` carries no value; on an MCP call it is that tool. A domain reject
    /// carries its domain. Either scope on an access it cannot apply to is a reject-once.
    #[test]
    fn always_reject_with_tool_or_domain_scope_persists_the_denial() {
        let tool_scope = serde_json::json!({
            "outcome": "always_reject",
            "scope": { "kind": "tool_scope" },
        });
        let mcp = AccessKind::MCPTool {
            name: "linear__save_issue".into(),
            input: serde_json::Value::Null,
        };
        match reply_to_outcome(&tool_scope, &mcp) {
            PromptOutcome::RejectAlwaysMcpTool(v) => assert_eq!(v, "linear__save_issue"),
            other => panic!("expected RejectAlwaysMcpTool, got {other:?}"),
        }
        assert!(matches!(
            reply_to_outcome(&tool_scope, &any_access()),
            PromptOutcome::RejectOnce
        ));
        let domain = serde_json::json!({
            "outcome": "always_reject",
            "scope": { "kind": "domain", "value": "example.com" },
        });
        match reply_to_outcome(
            &domain,
            &AccessKind::WebFetch("https://example.com/x".into()),
        ) {
            PromptOutcome::RejectAlwaysDomain(v) => assert_eq!(v, "example.com"),
            other => panic!("expected RejectAlwaysDomain, got {other:?}"),
        }
    }

    struct StubTransport {
        reply: Result<Value, String>,
        seen: Mutex<Option<Value>>,
    }

    #[async_trait]
    impl PermissionHookTransport for StubTransport {
        async fn request_permission(&self, payload: Value) -> Result<Value, String> {
            *self.seen.lock().unwrap() = Some(payload);
            self.reply.clone()
        }
    }

    #[tokio::test]
    async fn request_sends_payload_and_decodes_reply() {
        let transport = StubTransport {
            reply: Ok(serde_json::json!({ "outcome": "approve" })),
            seen: Mutex::new(None),
        };
        let outcome = request_permission_via_hub(
            &transport,
            &AccessKind::Bash("ls -la".into()),
            "tc-7",
            /*hook_ask=*/ None,
            ToolApprovalPolicy::GrantsAllowed,
        )
        .await;
        assert!(matches!(outcome, PromptOutcome::AllowOnce));
        let seen = transport
            .seen
            .lock()
            .unwrap()
            .clone()
            .expect("payload sent");
        assert_eq!(
            seen.get("tool_call_id").unwrap_or(&serde_json::Value::Null),
            "tc-7"
        );
        assert_eq!(
            seen.get("bash_command").unwrap_or(&serde_json::Value::Null),
            "ls -la"
        );
    }

    #[tokio::test]
    async fn transport_error_fails_closed() {
        let transport = StubTransport {
            reply: Err("connection lost".to_owned()),
            seen: Mutex::new(None),
        };
        let outcome = request_permission_via_hub(
            &transport,
            &AccessKind::Edit("a.rs".into()),
            "tc-8",
            /*hook_ask=*/ None,
            ToolApprovalPolicy::GrantsAllowed,
        )
        .await;
        assert!(matches!(outcome, PromptOutcome::Error(_)));
    }

    #[tokio::test]
    async fn edit_always_approve_maps_to_session_scope() {
        let transport = StubTransport {
            reply: Ok(serde_json::json!({ "outcome": "always_approve" })),
            seen: Mutex::new(None),
        };
        let outcome = request_permission_via_hub(
            &transport,
            &AccessKind::Edit("a.rs".into()),
            "tc-9",
            /*hook_ask=*/ None,
            ToolApprovalPolicy::GrantsAllowed,
        )
        .await;
        assert!(matches!(outcome, PromptOutcome::AllowEditsForSession));
        let transport = StubTransport {
            reply: Ok(serde_json::json!({ "outcome": "always_approve" })),
            seen: Mutex::new(None),
        };
        let outcome = request_permission_via_hub(
            &transport,
            &AccessKind::AgentMessage {
                subagent_id: "sub-1".into(),
            },
            "tc-message",
            /*hook_ask=*/ None,
            ToolApprovalPolicy::GrantsAllowed,
        )
        .await;
        assert!(matches!(outcome, PromptOutcome::AllowOnce));
        let transport = StubTransport {
            reply: Ok(serde_json::json!({ "outcome": "always_approve" })),
            seen: Mutex::new(None),
        };
        let outcome = request_permission_via_hub(
            &transport,
            &AccessKind::MCPTool {
                name: "x".into(),
                input: serde_json::Value::Null,
            },
            "tc-10",
            /*hook_ask=*/ None,
            ToolApprovalPolicy::GrantsAllowed,
        )
        .await;
        assert!(matches!(outcome, PromptOutcome::AllowAlways));
    }
}
