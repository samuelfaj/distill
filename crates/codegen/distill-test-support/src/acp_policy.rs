//! Scripted answers for the requests the agent sends to the client, declared as data before a turn runs.
//! The connection side of [`GrokStdioClient`](crate::GrokStdioClient) applies a [`ClientPolicy`] to every
//! `session/request_permission` and `x.ai/ask_user_question` request, so no test blocks on a prompt or answers
//! one in test code.

use std::collections::BTreeMap;

use agent_client_protocol as acp;
use serde_json::Value;

use crate::acp_ask_user_question::{self, AskUserQuestionRequest};

/// How the client answers one `session/request_permission`.
/// A decision whose option kind the agent did not offer answers `cancelled`, never a different option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Select the `allow_once` option.
    Allow,
    /// Select the `allow_always` option.
    AllowAlways,
    /// Select the `reject_once` option.
    Deny,
    /// Answer the `cancelled` outcome at once.
    Cancel,
    /// Leave the request open until the client next sends `session/cancel` for its session, then answer `cancelled`.
    HoldUntilCancel,
}

/// How the client answers one `x.ai/ask_user_question`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionDecision {
    /// Accept, selecting the first option of every question; a question without options stays unanswered.
    Accept,
    /// Dismiss the questions with the `cancelled` outcome at once.
    Cancel,
    /// Leave the request open until the client next sends `session/cancel` for its session, then dismiss the questions.
    HoldUntilCancel,
}

/// How the client answers one `x.ai/folder_trust/request`.
/// The agent sends this prompt only to a client that advertised `x.ai/folderTrust.interactive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustDecision {
    /// Answer `trust`, granting folder trust for the workspace.
    Grant,
    /// Answer `reject`, leaving the workspace gated.
    Deny,
}

/// How the client answers one `x.ai/mcp/elicit` reverse request (an MCP server's `elicitation/create`
/// forwarded by the agent). `Accept` returns `fields` as the form content; `Decline` and `Cancel`
/// return those outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitationDecision {
    Accept { fields: BTreeMap<String, String> },
    Decline,
    Cancel,
}

impl ElicitationDecision {
    /// The `McpElicitExtResponse` wire shape (`#[serde(tag = "outcome")]`, snake_case) the agent's
    /// elicitation coordinator parses back into an `ElicitResult`.
    pub(crate) fn reply(&self) -> Value {
        match self {
            ElicitationDecision::Accept { fields } => {
                let content: serde_json::Map<String, Value> = fields
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                    .collect();
                serde_json::json!({ "outcome": "accept", "content": content })
            }
            ElicitationDecision::Decline => serde_json::json!({ "outcome": "decline" }),
            ElicitationDecision::Cancel => serde_json::json!({ "outcome": "cancel" }),
        }
    }
}

/// Whether the client advertises interactivity to the agent. The default, [`Interactivity::Headless`],
/// advertises `nonInteractive: true`, so the agent auto-cancels reverse interactions such as MCP
/// elicitation without prompting; every existing case keeps this behavior. [`Interactivity::Interactive`]
/// opts in, advertising `nonInteractive: false` and scripting the one answer the client returns for an
/// `x.ai/mcp/elicit` reverse request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interactivity {
    Headless,
    Interactive { elicitation: ElicitationDecision },
}

/// One decision for every request of a kind, with exceptions for particular requests counted from 1 in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestPolicy<D> {
    default: D,
    exceptions: BTreeMap<usize, D>,
}

impl<D> RequestPolicy<D> {
    #[must_use]
    pub fn new(default: D) -> Self {
        RequestPolicy {
            default,
            exceptions: BTreeMap::new(),
        }
    }

    /// Answer request number `request_number`, counted from 1, with `decision` instead of the default.
    #[must_use]
    pub fn with_nth(mut self, request_number: usize, decision: D) -> Self {
        assert!(request_number >= 1, "requests are counted from 1");
        self.exceptions.insert(request_number, decision);
        self
    }
}

impl<D: Clone> RequestPolicy<D> {
    pub(crate) fn decision_for(&self, request_number: usize) -> D {
        self.exceptions
            .get(&request_number)
            .unwrap_or(&self.default)
            .clone()
    }
}

/// The scripted answers one client applies. A test passes it in as data; the default allows every permission
/// and dismisses every question, the answers a client without a user gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPolicy {
    pub permissions: RequestPolicy<PermissionDecision>,
    pub questions: RequestPolicy<QuestionDecision>,
    /// Absent leaves the client without the `x.ai/folderTrust.interactive` capability, so the agent
    /// never sends a folder-trust prompt; `Some` advertises the capability and answers every prompt.
    pub trust: Option<TrustDecision>,
    pub interactivity: Interactivity,
}

impl Default for ClientPolicy {
    fn default() -> Self {
        ClientPolicy {
            permissions: RequestPolicy::new(PermissionDecision::Allow),
            questions: RequestPolicy::new(QuestionDecision::Cancel),
            trust: None,
            interactivity: Interactivity::Headless,
        }
    }
}

/// When the client sends its answer to a request from the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply<T> {
    Now(T),
    /// Sent once the client cancels `session_id`.
    AfterCancel {
        value: T,
        session_id: acp::SessionId,
    },
}

impl PermissionDecision {
    pub(crate) fn reply(
        self,
        request: &acp::RequestPermissionRequest,
    ) -> Reply<acp::RequestPermissionOutcome> {
        let select = |kind: acp::PermissionOptionKind| {
            Reply::Now(
                request
                    .options
                    .iter()
                    .find(|option| option.kind == kind)
                    .map_or(acp::RequestPermissionOutcome::Cancelled, |option| {
                        acp::RequestPermissionOutcome::Selected(
                            acp::SelectedPermissionOutcome::new(option.option_id.clone()),
                        )
                    }),
            )
        };
        match self {
            PermissionDecision::Allow => select(acp::PermissionOptionKind::AllowOnce),
            PermissionDecision::AllowAlways => select(acp::PermissionOptionKind::AllowAlways),
            PermissionDecision::Deny => select(acp::PermissionOptionKind::RejectOnce),
            PermissionDecision::Cancel => Reply::Now(acp::RequestPermissionOutcome::Cancelled),
            PermissionDecision::HoldUntilCancel => Reply::AfterCancel {
                value: acp::RequestPermissionOutcome::Cancelled,
                session_id: request.session_id.clone(),
            },
        }
    }
}

impl QuestionDecision {
    pub(crate) fn reply(self, request: &AskUserQuestionRequest) -> Reply<Value> {
        match self {
            QuestionDecision::Accept => Reply::Now(acp_ask_user_question::accepted_reply(request)),
            QuestionDecision::Cancel => Reply::Now(acp_ask_user_question::cancelled_reply()),
            QuestionDecision::HoldUntilCancel => Reply::AfterCancel {
                value: acp_ask_user_question::cancelled_reply(),
                session_id: request.session_id.clone(),
            },
        }
    }
}

impl TrustDecision {
    /// The `{ "outcome": _ }` payload the GUI client returns; only `trust` grants, every other value
    /// (including `reject`) leaves the workspace gated.
    pub(crate) fn reply(self) -> Reply<Value> {
        let outcome = match self {
            TrustDecision::Grant => "trust",
            TrustDecision::Deny => "reject",
        };
        Reply::Now(serde_json::json!({ "outcome": outcome }))
    }
}

#[cfg(test)]
#[path = "acp_policy_tests.rs"]
mod tests;
