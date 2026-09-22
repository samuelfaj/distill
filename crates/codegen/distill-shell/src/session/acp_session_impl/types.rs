// Modified for Distill by Samuel Fajreldines, 2026.
//! Top-level enum definitions for `acp_session`; their `impl` blocks and helpers stay in `acp_session.rs`.

use super::*;

/// Identity captured at the sampler submission boundary. It is separate from
/// the response so failed, cancelled, and retried requests can use the same
/// ledger shape without inventing provider response fields.
#[derive(Debug, Clone)]
pub(crate) struct UsageAttemptContext {
    pub(crate) attempt_id: String,
    pub(crate) task_id: Option<String>,
    pub(crate) turn_id: Option<String>,
    pub(crate) request_id: Option<String>,
    pub(crate) role: String,
    pub(crate) model_id: String,
    pub(crate) endpoint: Option<String>,
    pub(crate) requested_effort: Option<String>,
    pub(crate) applied_effort: Option<String>,
}

impl UsageAttemptContext {
    pub(crate) fn update_from_response(
        &mut self,
        response: &distill_sampling_types::ConversationResponse,
    ) {
        if let Some(message_id) = response.message_id.clone() {
            self.request_id = Some(message_id);
        }
        if let Some(model_id) = response
            .assistant()
            .and_then(|assistant| assistant.model_id.clone())
            .filter(|model| !model.is_empty())
        {
            self.model_id = model_id;
        }
    }

    pub(crate) fn into_attribution(
        self,
        status: distill_chat_state::UsageCallStatus,
        usage: Option<distill_sampling_types::TokenUsage>,
        usage_complete: bool,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    ) -> distill_chat_state::UsageAttribution {
        let cost_usd_ticks = distill_sampling_types::reported_cost_ticks(cost_usd_ticks);
        distill_chat_state::UsageAttribution {
            attempt_id: self.attempt_id,
            task_id: self.task_id,
            turn_id: self.turn_id,
            request_id: self.request_id,
            role: self.role,
            model_id: self.model_id,
            endpoint: self.endpoint,
            requested_effort: self.requested_effort,
            applied_effort: self.applied_effort,
            status,
            usage,
            usage_complete,
            api_duration_ms,
            cost_usd_ticks,
            cost_basis: if cost_usd_ticks.is_some() {
                distill_chat_state::UsageCostBasis::Reported
            } else {
                distill_chat_state::UsageCostBasis::Unknown
            },
        }
    }
}

/// Controls how MCP server system-reminders are injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpReminderMode {
    /// Only inject what changed (added/removed/updated servers).
    Delta,
    /// Inject the full server list when anything changes.
    Full,
}

/// Which credential store the resubmit should wait on. Waiting on the session token for a
/// provider-key 401 would hold the full pace for a refresh irrelevant to the rejected credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveredStore {
    /// `AuthManager` session token (devbox re-mint, OIDC refresh); `wait_for_token_refresh` is meaningful.
    SessionToken,
    /// Auth-provider key minted into chat-state credentials; nothing to wait on in the `AuthManager`, so floor-pace instead.
    AuthProvider,
}

/// Whether a turn iteration is a parked uncharged-401 resubmit — see the re-park arm in
/// `SessionActor::handle_sampling_failure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnParkState {
    /// No park in effect: full prepare/preflight cadence.
    Fresh,
    /// Parked on the uncharged path since the last successful response.
    Parked,
}

impl TurnParkState {
    pub(crate) fn is_parked(self) -> bool {
        matches!(self, Self::Parked)
    }
}

/// Recovery decision returned by `SessionActor::handle_sampling_failure` for the sampler-based turn loop.
pub(crate) enum SamplerFailureRecovery {
    /// Compaction ran.
    /// The turn loop should rebuild the request from the compacted conversation and resubmit.
    CompactAndResubmit,
    /// Resubmit through the auth-retry schedule: recovery succeeded, or the parked
    /// credential-less case. `credential` is the rejected request's wire provenance;
    /// credential-less sends are never charged against the per-incident budget.
    RefreshAuthAndResubmit {
        credential: distill_sampling_types::SentCredential,
        store: RecoveredStore,
    },
    /// Transient failure: back off and resubmit instead of killing the turn.
    /// Retries are bounded.
    RetryTransient {
        kind: distill_sampler::SamplingErrorKind,
        status_code: Option<u16>,
    },
}

/// Outcome of a single turn attempt via the sampler-based path.
/// `CompactAndResubmit` short-circuits the outer turn loop with `continue` (the turn driver re-builds the request from the latest chat state).
pub(crate) enum SamplerTurnOutcome {
    /// Model responded, with per-call latency stats for `shell.turn.inference_done`.
    Response(
        Box<ConversationResponse>,
        Box<distill_sampler::InferenceLatencyStats>,
        UsageAttemptContext,
    ),
    CompactAndResubmit,
    /// Retry through the auth-retry schedule. Mirrors
    /// [`SamplerFailureRecovery::RefreshAuthAndResubmit`].
    RefreshAuthAndResubmit {
        credential: distill_sampling_types::SentCredential,
        store: RecoveredStore,
    },
    /// Mirrors [`SamplerFailureRecovery::RetryTransient`].
    RetryTransient {
        kind: distill_sampler::SamplingErrorKind,
        status_code: Option<u16>,
    },
}

/// How a completed turn stopped, mapped 1:1 to `acp::StopReason`.
/// A refusal that also exhausted the salvage budget reports `Refusal`.
pub(crate) enum CompletedStop {
    EndTurn,
    /// The salvage continue budget ran out, so the committed content is known to be cut off.
    MaxTokens,
    /// Content-filter refusal with the provider explanation (may be empty).
    Refusal(String),
}

/// Outcome of `process_conversation_turn`, distinguishing normal completion from cancellation.
pub(crate) enum TurnOutcome {
    /// The model finished responding (no more tool calls).
    /// `tools_called` lists the tools invoked this turn, for completion-requirement tracking.
    /// `structured_output` is the schema-validated `--json-schema` output (`None` without a schema; `Some(Err)` on parse or validation failure).
    Completed {
        tools_called: Vec<String>,
        structured_output: Option<Result<serde_json::Value, String>>,
        /// How the completed turn stopped; decided once at the return site.
        stop: CompletedStop,
    },
    /// The turn was cancelled (user rejection, hook denial, doom loop, etc.).
    /// The category distinguishes the cause for analytics.
    Cancelled {
        category: Option<crate::session::events::CancellationCategory>,
        context: Option<crate::session::commands::CancellationContext>,
    },
    /// The `--max-turns` limit was reached after a tool-execution cycle.
    MaxTurnsReached { limit: usize },
    /// Silent EndTurn after stationarity or true-noop thrash.
    /// Distinct from Completed so the recovery, goal, and stop-hook paths cannot re-open the sampling loop.
    StationarityEnded,
}

#[derive(Debug)]
pub(crate) enum ToolLoop {
    Continue,
    NonExistingTool,
    ToolParsingError,
    /// User clicked "No" on a permission prompt.
    /// Carries the rejection reason and tool name so callers (e.g. subagent result) can report what was rejected.
    PermissionReject {
        tool_name: String,
        reason: String,
    },
    /// The user cancelled the turn (e.g. Cmd+C during a permission prompt).
    Cancelled,
    /// User provided a followup message instead of approving the tool execution.
    /// The string contains the followup message to be added as a user turn.
    FollowupMessage(String),
    /// A user-configured `pre_tool_use` hook blocked this tool call.
    /// Non-terminal: the deny reason is fed back and the turn continues (see `execute_tool_calls`).
    /// Only `hook_name` is retained, for the per-tool telemetry and annotation.
    HookDenied {
        hook_name: String,
    },
}

/// Why the TodoGate fired.
/// Single variant today; kept as an enum so any new reason has to go through the same `as_str` to `TODO_GATE_*` const mapping.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoGateReason {
    /// The turn ended with pending or unbacked in-progress todos.
    InFlight,
}

/// Outcome of `evaluate_todo_gate`.
/// `Nudge` carries the rendered reminder text and the typed reason so the producer can emit telemetry without re-deriving the reason from the input.
/// Exposed as `pub` solely so the replay-trace integration test in `tests/trace_replay.rs` can match against the decision.
#[doc(hidden)]
pub enum TodoGateDecision {
    /// Gate is satisfied; the turn may end.
    Continue,
    /// Inject `reminder` as a `<system-reminder>` and force another turn.
    Nudge {
        reminder: String,
        reason: TodoGateReason,
    },
}

/// Why `maybe_fire_laziness_check` aborted before producing a verdict.
/// Maps 1:1 to the `LAZINESS_ABORT_*` telemetry consts via `as_const_str()`.
/// Mirrors the `TodoGateReason` pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LazinessAbortReason {
    UserInput,
    ModelSwitch,
    Timeout,
    ClassifierError,
}

/// Reasons the tolerant classifier-output parser can fail.
/// Each carries enough context to log without leaking the potentially long raw response; the caller logs the raw text separately, truncated.
#[derive(Debug)]
pub(crate) enum ClassifierParseError {
    /// Strict JSON, fence-stripped JSON, and brace-extracted JSON all failed to parse; the response is not recoverable.
    Unparseable,
    /// JSON parsed but `confidence` was outside `[0.0, 1.0]`.
    ConfidenceOutOfRange(f32),
}

/// Outcome of `evaluate_laziness`.
/// `Nudge` carries the category const, confidence, and evidence.
/// The producer can then emit telemetry and build the nudge text without re-deriving anything from the input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LazinessDecision {
    Nudge {
        category: crate::session::events::LazinessCategory,
        confidence: f32,
        evidence: String,
    },
    NoNudge {
        category: crate::session::events::LazinessCategory,
        confidence: f32,
        reason: NoNudgeReason,
    },
}

/// Closed set of reasons `evaluate_laziness` returned `NoNudge`.
/// Each maps to a distinct dashboard slice; the consistency test below asserts the producer never invents a new reason without extending this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoNudgeReason {
    /// Classifier returned one of the `not_stalled_*` categories.
    NotStalled,
    /// Confidence below the configured (or default) threshold.
    LowConfidence,
    /// Per-session cap was `0` (observation-only) or the cap has been reached.
    /// Both cases mean "fire telemetry but inject no nudge".
    CapExhausted,
    /// Defensive: the caller should not have invoked `evaluate_laziness` when `cfg.enabled = false`, but if it did, return this rather than panic.
    FeatureDisabled,
}

/// Distinguishes turn-end drains from mid-turn drains.
/// Turn-end is the safe boundary to fire the verification stage, since no model inference is running.
/// Mid-turn `update_goal(completed: true)` calls are deferred so the verifier-skeptic subagents never race the parent's sampler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainPurpose {
    /// End-of-turn drain.
    /// Verification-eligible completions fire the verification stage (if enabled, Active, and not already running).
    /// Deferred completions from prior mid-turn drains are processed FIFO ahead of the regular channel.
    TurnEnd,
    /// Mid-turn drain (e.g. on `SubagentSpawned`).
    /// Verification-eligible completions are deferred to `pending_classifier_completions` for processing at the next turn-end.
    MidTurn,
}

/// Origin of a drain entry.
/// `Pending` entries had their acks resolved at defer time; `Channel` entries still carry a live oneshot.
pub(crate) enum DrainSource {
    Pending(distill_tools::implementations::distill::update_goal::UpdateGoalInput),
    Channel(distill_tools::implementations::distill::update_goal::UpdateGoalEnvelope),
}

/// Reason a NotAchieved verdict was synthesized without invoking the sampler.
/// Used by `account_not_achieved_without_sampler` to label the synthetic details file and (in future variants) emit distinct telemetry.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NotAchievedSyntheticReason {
    /// A second `update_goal(completed: true)` arrived while a classifier was already running for this goal.
    ConcurrentInFlight,
}

/// Decision for one in-turn goal round (see [`SessionActor::run_goal_round_end`]).
/// `Continue` keeps the turn alive and runs another round with the carried directive injected.
/// `EndTurn` ends the turn because the goal resolved (achieved, paused, blocked) or this isn't a goal turn.
pub(crate) enum GoalRoundDecision {
    Continue(String),
    EndTurn,
}

/// Decision from the turn-end stop gate: allow the turn to end, or keep the agent working by injecting `feedback` as a synthetic user message.
#[derive(Debug)]
pub(crate) enum StopGateDecision {
    AllowStop,
    KeepWorking { feedback: String },
}

/// Serialized onto `streaming_partial.json` so trace inspection can tell whether the abort interrupted thinking, response text, or a tool call.
/// `Completed` always fires before the turn loop dispatches tools, so a streaming partial can only be tied to one of the model's own emission phases.
/// Interruptions during tool execution are covered by the canonical `record_assistant_response` path instead.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapturePhase {
    /// Stream opened (`StreamStarted`) but no channel token observed yet.
    #[default]
    Pending,
    /// Reasoning ("thinking") tokens were the most recent output.
    Reasoning,
    /// Visible response-text tokens were the most recent output.
    ResponseText,
    /// The model had begun emitting a tool call (`ToolCallDelta`).
    ToolCall,
}
