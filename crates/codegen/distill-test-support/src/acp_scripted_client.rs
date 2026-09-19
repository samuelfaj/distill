//! The connection side of `GrokStdioClient` and `LeaderStdioClient`: the `agent_client_protocol::Client`
//! handler that answers each request from the agent by a [`ClientPolicy`] and records every message.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_client_protocol as acp;
use serde::Deserialize;
use serde_json::Value;

use crate::acp_ask_user_question::{ASK_USER_QUESTION_METHOD, AskUserQuestionRequest};
use crate::acp_hold_registry::HoldRegistry;
use crate::acp_policy::{
    ClientPolicy, Interactivity, PermissionDecision, QuestionDecision, Reply, RequestPolicy,
    TrustDecision,
};
use crate::acp_transcript::{Transcript, TranscriptEntry};

/// The reverse-request method an agent sends to prompt a capable client for a folder-trust decision.
const FOLDER_TRUST_REQUEST_METHOD: &str = "x.ai/folder_trust/request";

/// The reverse method an MCP server's `elicitation/create` reaches the client through.
const MCP_ELICIT_METHOD: &str = "x.ai/mcp/elicit";

/// A policy for one kind of request plus the count of arrivals so far, numbered from 1 to match
/// `RequestPolicy::with_nth`.
struct NumberedPolicy<D> {
    policy: RequestPolicy<D>,
    arrivals: AtomicUsize,
}

impl<D> NumberedPolicy<D> {
    fn new(policy: RequestPolicy<D>) -> Self {
        NumberedPolicy {
            policy,
            arrivals: AtomicUsize::new(0),
        }
    }
}

impl<D: Clone> NumberedPolicy<D> {
    fn next_decision(&self) -> D {
        let request_number = self.arrivals.fetch_add(1, Ordering::Relaxed) + 1;
        self.policy.decision_for(request_number)
    }
}

/// The handler the connection drives, shared with the client that reads what it recorded.
#[derive(Clone)]
pub(crate) struct ScriptedClient {
    state: Arc<ClientState>,
}

struct ClientState {
    permissions: NumberedPolicy<PermissionDecision>,
    questions: NumberedPolicy<QuestionDecision>,
    trust: Option<TrustDecision>,
    interactivity: Interactivity,
    transcript: Transcript,
    holds: HoldRegistry,
}

impl ScriptedClient {
    pub(crate) fn new(policy: ClientPolicy) -> Self {
        ScriptedClient {
            state: Arc::new(ClientState {
                permissions: NumberedPolicy::new(policy.permissions),
                questions: NumberedPolicy::new(policy.questions),
                trust: policy.trust,
                interactivity: policy.interactivity,
                transcript: Transcript::default(),
                holds: HoldRegistry::default(),
            }),
        }
    }

    pub(crate) fn transcript(&self) -> &Transcript {
        &self.state.transcript
    }

    pub(crate) fn holds(&self) -> &HoldRegistry {
        &self.state.holds
    }

    /// Whether this client advertises `x.ai/folderTrust.interactive`, the capability the agent
    /// requires before it sends a folder-trust prompt.
    pub(crate) fn advertises_interactive_trust(&self) -> bool {
        self.state.trust.is_some()
    }

    pub(crate) fn interactivity(&self) -> &Interactivity {
        &self.state.interactivity
    }
}

impl ClientState {
    /// The value of `reply`, recorded as the entry `record` builds from it. A held reply first waits for its
    /// session's cancel and stays registered until it is recorded.
    async fn answer<T>(&self, reply: Reply<T>, record: impl FnOnce(&T) -> TranscriptEntry) -> T {
        let (value, registered_hold) = match reply {
            Reply::Now(value) => (value, None),
            Reply::AfterCancel { value, session_id } => (
                value,
                Some(self.holds.hold_until_released(&session_id).await),
            ),
        };
        self.transcript.record(record(&value));
        drop(registered_hold);
        value
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for ScriptedClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let reply = self.state.permissions.next_decision().reply(&args);
        let outcome = self
            .state
            .answer(reply, |outcome| TranscriptEntry::PermissionRequest {
                request: args,
                outcome: outcome.clone(),
            })
            .await;
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        self.state
            .transcript
            .record(TranscriptEntry::SessionUpdate(args));
        Ok(())
    }

    /// Question requests get the policy reply. Any other extension request is answered `null`, the trait
    /// default, which the shell reads as a declined plan approval rather than as a client that went away.
    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        let method = args.method.as_ref().to_owned();
        let params: Value = serde_json::from_str(args.params.get())?;
        let reply = match method.as_str() {
            ASK_USER_QUESTION_METHOD => {
                let question = AskUserQuestionRequest::deserialize(&params)?;
                self.state.questions.next_decision().reply(&question)
            }
            FOLDER_TRUST_REQUEST_METHOD => match self.state.trust {
                Some(decision) => decision.reply(),
                None => Reply::Now(Value::Null),
            },
            MCP_ELICIT_METHOD => match &self.state.interactivity {
                Interactivity::Interactive { elicitation } => Reply::Now(elicitation.reply()),
                Interactivity::Headless => Reply::Now(Value::Null),
            },
            _ => Reply::Now(Value::Null),
        };
        let reply = self
            .state
            .answer(reply, |reply| TranscriptEntry::ExtRequest {
                method,
                params,
                reply: reply.clone(),
            })
            .await;
        let raw = serde_json::value::to_raw_value(&reply).expect("serialize ext reply");
        Ok(acp::ExtResponse::new(Arc::from(raw)))
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        let params: Value = serde_json::from_str(args.params.get())?;
        self.state
            .transcript
            .record(TranscriptEntry::ExtNotification {
                method: args.method.as_ref().to_owned(),
                params,
            });
        Ok(())
    }
}

#[cfg(test)]
#[path = "acp_scripted_client_tests.rs"]
mod tests;
