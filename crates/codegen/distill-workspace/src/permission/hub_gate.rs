// Modified for Distill by Samuel Fajreldines, 2026.
//! The approval gate on hub tool calls: the session owner's answer stands between the model and every
//! mutating tool on the user's device. A call the toolset cannot decode never runs; only the read-only
//! allowlist in [`AccessKind::from`] skips the prompt; no transport and no answer both deny; the folder's
//! persisted grants are the TUI's `permission.toml`, shared on purpose so a grant given in either
//! surface holds in the other.

use std::sync::LazyLock;

use prometheus::{IntCounterVec, register_int_counter_vec};
use serde_json::Value;
use distill_paths::AbsPathBuf;
use distill_tool_runtime::{ToolApprovalPolicy, ToolError, ToolErrorKind};

use crate::handle::WorkspaceHandle;
use crate::host_kind::WorkspaceHostKind;
use crate::permission::grants::{
    evaluate_bash_with_ambient, protected_target, record_prompt_outcome, session_grant_pre_decision,
};
use crate::permission::hub_permission::{
    PermissionHookTransport, ToolServerPermissionTransport, hitl_permission_live_enabled,
    prompt_outcome_allows, request_permission_via_hub,
};
use crate::permission::prompter::PromptOutcome;
use crate::permission::state::{CachedStateStore, PermissionState, persist_state};
use crate::permission::types::{AccessKind, Decision};
use crate::session::WorkspaceSession;

static DECISION_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "grok_workspace_permission_decision_total",
        "Hub tool calls settled by the approval gate, by how they were settled",
        &["decision"]
    )
    .expect("grok_workspace_permission_decision_total must register once")
});

const DECISIONS: [&str; 7] = [
    "undecodable",
    "grant_allow",
    "grant_deny",
    "no_transport",
    "prompt_allow",
    "prompt_deny",
    "prompt_redirect",
];

pub(crate) fn init_metrics() {
    for decision in DECISIONS {
        let _ = DECISION_TOTAL.with_label_values(&[decision]);
    }
}

fn count(decision: &str) {
    DECISION_TOTAL.with_label_values(&[decision]).inc();
}

/// Whether hub tool calls on a workspace wait for the session owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolApprovalGate {
    /// Mutating calls go through the folder's grants and the owner's prompt; reads run unasked.
    Enforced,
    /// Every call runs unasked.
    Off,
}

/// On by construction where the device is the user's own: there is no off switch on the device, only
/// the tenant's `tool_approval_policy` delivered with the bind. The sandbox guest keeps the opt-in its
/// plane already uses (`GROK_HITL_PERMISSION_LIVE`).
pub fn approval_gate_for(host_kind: WorkspaceHostKind) -> ToolApprovalGate {
    resolve_gate(host_kind, hitl_permission_live_enabled())
}

fn resolve_gate(host_kind: WorkspaceHostKind, hitl_opt_in: bool) -> ToolApprovalGate {
    let enforced = match host_kind {
        WorkspaceHostKind::Daemon => true,
        WorkspaceHostKind::Sandbox => hitl_opt_in,
    };
    if enforced {
        ToolApprovalGate::Enforced
    } else {
        ToolApprovalGate::Off
    }
}

/// Only a tool that reads state, or touches nothing but the session's own bookkeeping, runs unasked.
/// `web_fetch` prompts: an outbound fetch is an exfiltration channel.
fn requires_approval(access: &AccessKind) -> bool {
    match access {
        AccessKind::Read(_) | AccessKind::Grep { .. } | AccessKind::WebSearch(_) => false,
        AccessKind::Bash(_)
        | AccessKind::Edit(_)
        | AccessKind::MCPTool { .. }
        | AccessKind::WebFetch(_)
        | AccessKind::AgentMessage { .. }
        | AccessKind::Tool(_) => true,
    }
}

/// The session's side of the gate: the hub-set ceiling and, once a guarded call arrives, the folder's
/// grants. Released with the [`WorkspaceSession`] that owns it.
#[derive(Default)]
pub(crate) struct SessionApproval {
    policy: parking_lot::Mutex<ToolApprovalPolicy>,
    /// Held across the prompt, so a session answers one card at a time.
    grants: tokio::sync::Mutex<Option<FolderGrants>>,
}

impl SessionApproval {
    pub(crate) fn set_policy(&self, policy: ToolApprovalPolicy) {
        *self.policy.lock() = policy;
    }

    fn policy(&self) -> ToolApprovalPolicy {
        *self.policy.lock()
    }
}

struct FolderGrants {
    cwd: AbsPathBuf,
    store: CachedStateStore,
    state: PermissionState,
    /// The owner's "allow all edits" answer: session-scoped by design, never written to disk.
    allow_edits_for_session: bool,
}

impl FolderGrants {
    async fn load(cwd: AbsPathBuf) -> Self {
        let (store, state) = CachedStateStore::resolve_and_load(&cwd, None).await;
        FolderGrants {
            cwd,
            store,
            state,
            allow_edits_for_session: false,
        }
    }

    /// `Some(Ok)` runs, `Some(Err(reason))` denies, `None` prompts.
    async fn pre_decision(
        &mut self,
        access: &AccessKind,
        policy: ToolApprovalPolicy,
    ) -> Option<Result<(), String>> {
        if let Some(fresh) = self.store.reload_if_changed().await {
            self.state = fresh;
        }
        let bash = match access {
            AccessKind::Bash(cmd) => {
                // The ambient git scan reads `.git/config` under the cwd; off the runtime thread, and a
                // scan that did not finish is a prompt
                let (cmd, state, cwd) = (cmd.clone(), self.state.clone(), self.cwd.clone());
                let evaluation = tokio::task::spawn_blocking(move || {
                    evaluate_bash_with_ambient(&cmd, &state, cwd.as_path())
                })
                .await
                .ok()?;
                Some(evaluation)
            }
            _ => None,
        };
        // The same floor the TUI applies: a hook root, `.git/hooks`, `.ssh`, a shell rc, the grant store
        // is prompted for whatever grants say. Hub sessions have no display path, so no path context
        if protected_target(access, bash.as_ref(), self.cwd.as_path(), None).is_some() {
            return None;
        }
        // A folder's blanket `allow_bash_execute` is unattended mode for bash; it is honoured only where the
        // tenant allows unattended hosts. Explicit command and glob grants are unaffected by the pin
        let yolo_pin = (policy != ToolApprovalPolicy::UnattendedAllowed)
            .then_some(BLANKET_BASH_NEEDS_UNATTENDED);
        let (decision, _) = session_grant_pre_decision(
            access,
            bash.as_ref(),
            &self.state,
            self.allow_edits_for_session,
            None,
            yolo_pin,
        )?;
        match decision {
            Decision::Allow => Some(Ok(())),
            Decision::Reject(reason) => Some(Err(reason)),
            Decision::Ask
            | Decision::FollowupMessage(_)
            | Decision::PolicyDeny(_)
            | Decision::Cancelled => None,
        }
    }

    async fn record(&mut self, access: &AccessKind, outcome: &PromptOutcome) {
        if matches!(outcome, PromptOutcome::AllowEditsForSession) {
            self.allow_edits_for_session = true;
        } else if record_prompt_outcome(&mut self.state, access, outcome).is_some() {
            persist_state(&self.cwd, &self.state, None).await;
        }
    }
}

const BLANKET_BASH_NEEDS_UNATTENDED: &str =
    "a folder's allow_bash_execute needs the tenant's unattended_allowed ceiling";

fn permission_denied(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorKind::PermissionDenied, message)
}

/// Settle one hub tool call: `Ok` lets it run, `Err` is the denial the model sees.
///
/// # Errors
/// The toolset's own decode error when the call cannot be parsed (it could not have run);
/// `PermissionDenied` when a persisted deny matches, no hub transport can carry the prompt, or the
/// owner rejects, redirects, or does not answer.
pub(crate) async fn approve_hub_call(
    workspace: &WorkspaceHandle,
    session: &WorkspaceSession,
    tool_name: &str,
    call_id: &str,
    args: &Value,
) -> Result<(), ToolError> {
    let transport = workspace.hub_server_blocking().await.and_then(|server| {
        ToolServerPermissionTransport::from_session_id(server, session.session_id())
    });
    settle(
        session,
        tool_name,
        call_id,
        args,
        transport
            .as_ref()
            .map(|t| t as &dyn PermissionHookTransport),
    )
    .await
}

async fn settle(
    session: &WorkspaceSession,
    tool_name: &str,
    call_id: &str,
    args: &Value,
    transport: Option<&dyn PermissionHookTransport>,
) -> Result<(), ToolError> {
    let policy = session.approval.policy();
    // A client's auto-approve is unattended mode; only a tenant that allows unattended hosts may grant it.
    if policy == ToolApprovalPolicy::UnattendedAllowed && session.yolo_mode() {
        return Ok(());
    }
    let input = session
        .toolset()
        .try_parse(tool_name, args)
        .await
        .inspect_err(|_| count("undecodable"))?;
    let access = AccessKind::from(&input);
    if !requires_approval(&access) {
        return Ok(());
    }

    let mut slot = session.approval.grants.lock().await;
    let mut folder = match policy {
        ToolApprovalPolicy::AlwaysPrompt => None,
        ToolApprovalPolicy::GrantsAllowed | ToolApprovalPolicy::UnattendedAllowed => {
            if slot.is_none() {
                match AbsPathBuf::new(session.cwd().to_path_buf()) {
                    Ok(cwd) => *slot = Some(FolderGrants::load(cwd).await),
                    Err(e) => {
                        tracing::warn!(error = %e, "session cwd has no grant store; every mutating call prompts")
                    }
                }
            }
            slot.as_mut()
        }
    };
    if let Some(folder) = &mut folder {
        match folder.pre_decision(&access, policy).await {
            Some(Ok(())) => {
                count("grant_allow");
                return Ok(());
            }
            Some(Err(reason)) => {
                count("grant_deny");
                return Err(permission_denied(reason));
            }
            None => {}
        }
    }

    let Some(transport) = transport else {
        // SECURITY: fail closed; a guarded tool never runs without a channel to the session owner
        count("no_transport");
        tracing::warn!(tool = %tool_name, session = %session.session_id(), "no hub transport for the permission prompt; rejecting guarded tool");
        return Err(permission_denied(
            "tool permission unavailable (no hub transport)",
        ));
    };
    let outcome = request_permission_via_hub(transport, &access, call_id, None, policy).await;
    if let Some(folder) = &mut folder {
        folder.record(&access, &outcome).await;
    }
    if prompt_outcome_allows(&outcome) {
        count("prompt_allow");
        return Ok(());
    }
    tracing::info!(tool = %tool_name, session = %session.session_id(), call_id, ?outcome, "tool permission denied via hub; rejecting tool call");
    Err(match &outcome {
        PromptOutcome::FollowupMessage(msg) => {
            count("prompt_redirect");
            permission_denied(format!("tool permission redirected: {msg}"))
        }
        _ => {
            count("prompt_deny");
            permission_denied(format!("tool permission denied for {tool_name}"))
        }
    })
}

#[cfg(test)]
#[path = "hub_gate_tests.rs"]
mod tests;
