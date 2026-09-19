//! Plays each scripted [`Conversation`] forward one request at a time. Only a reply advances the
//! script, so a resend repeats it and a pin takes over and supersedes an in-progress turn; a failure
//! answers in place without advancing, so a retry meets the same position. A departure is a [`ScriptViolation`].

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::btree_map::Entry;
use std::sync::Arc;
use std::time::Duration;

use crate::conversation::ConversationId;
use crate::conversation_script::{
    Conversation, ScriptEntries, ScriptTarget, ScriptViolation, TextCheck, mock_call_id_prefix,
};
use crate::failure::{DOOM_LOOP_CHECK_HEADER, Failure, ObservedFailure};
use crate::inference_request::{
    BodyHash, InferenceRequest, OfferedTools, first_system_message, offered_tools, tool_results,
};
use crate::model_reply::ModelReply;
use crate::tools::PickedToolCall;

/// Only the results for this conversation's calls count.
struct ScriptRequest {
    conversation: ConversationId,
    body_hash: BodyHash,
    first_system_message: Option<String>,
    results: BTreeMap<String, String>,
    offered: OfferedTools,
    asks_for_loop_report: bool,
}

impl ScriptRequest {
    fn new(conversation: ConversationId, request: &InferenceRequest<'_>) -> Self {
        let body = request.body();
        let prefix = mock_call_id_prefix(conversation);
        ScriptRequest {
            conversation,
            body_hash: request.body_hash(),
            first_system_message: first_system_message(body),
            results: tool_results(body)
                .into_iter()
                .filter(|(call_id, _)| call_id.starts_with(&prefix))
                .collect(),
            offered: offered_tools(body),
            asks_for_loop_report: request.headers().contains_key(DOOM_LOOP_CHECK_HEADER),
        }
    }
}

/// A scripted conversation's answer to one request, recorded on its log entry.
pub(crate) struct ServedReply {
    pub(crate) reply: ModelReply,
    pub(crate) hold: Option<Duration>,
    pub(crate) observed: Option<ObservedFailure>,
}

struct ServedRequest {
    served: ServedReply,
    violations: Vec<ScriptViolation>,
}

/// A call keeps its check until the request carrying its result; a reply keeps the request that
/// took it.
enum Progress {
    Unserved,
    Pinned {
        entry_index: usize,
    },
    Calling {
        entry_index: usize,
        call_id: String,
        /// `None` once the check ran, or for a call without one.
        pending_check: Option<TextCheck>,
        taken_by: BodyHash,
    },
    Failing {
        entry_index: usize,
    },
    Replied {
        entry_index: usize,
        taken_by: BodyHash,
        /// The unpinned entry after it; `None` when its reply is the one that repeats.
        next: Option<usize>,
    },
}

struct FailureBudget {
    failure: Failure,
    remaining: usize,
}

impl FailureBudget {
    fn new(failure: &Failure) -> Self {
        FailureBudget {
            failure: failure.clone(),
            remaining: failure.count(),
        }
    }
}

/// One of an entry's failures chosen for a request; `repetition` counts from 1 and numbers a doom
/// loop's replay.
struct ChosenFailure {
    index: usize,
    failure: Failure,
    repetition: usize,
}

/// The failures of one entry in the order the case declared them; the first with budget left answers.
struct EntryFailures(Vec<FailureBudget>);

impl EntryFailures {
    fn next_failure(&self) -> Option<ChosenFailure> {
        let (index, budget) = self
            .0
            .iter()
            .enumerate()
            .find(|(_, budget)| budget.remaining > 0)?;
        Some(ChosenFailure {
            index,
            failure: budget.failure.clone(),
            repetition: budget.failure.count() - budget.remaining + 1,
        })
    }

    fn spend(&mut self, index: usize) {
        let Some(budget) = self.0.get_mut(index) else {
            unreachable!("failure index came from next_failure");
        };
        budget.remaining -= 1;
    }
}

/// What a request gets, decided without touching `Progress`; `record` then applies it.
enum NextStep {
    RepeatReply {
        entry_index: usize,
        text: String,
    },
    Failure {
        entry_index: usize,
        failure_index: usize,
        failure: Failure,
        /// The call a doom loop repeats, when the entry has one the request offers.
        looping_call: Option<(String, PickedToolCall)>,
        violation: Option<ScriptViolation>,
    },
    Call {
        entry_index: usize,
        call_id: String,
        pending_check: Option<TextCheck>,
        picked: PickedToolCall,
    },
    Reply {
        entry_index: usize,
        text: String,
    },
    NotOffered {
        entry_index: usize,
        violation: ScriptViolation,
        reply: String,
    },
}

impl NextStep {
    fn entry_index(&self) -> usize {
        match self {
            NextStep::RepeatReply { entry_index, .. }
            | NextStep::Failure { entry_index, .. }
            | NextStep::Call { entry_index, .. }
            | NextStep::Reply { entry_index, .. }
            | NextStep::NotOffered { entry_index, .. } => *entry_index,
        }
    }
}

/// The reply a decided step gives, the failure the log records, and any violation the step carries;
/// `held` is the failure a stalled answer records.
fn render(
    step: NextStep,
    request: &ScriptRequest,
    held: Option<ObservedFailure>,
    reasoning: Option<String>,
) -> (ModelReply, Option<ObservedFailure>, Vec<ScriptViolation>) {
    match step {
        NextStep::RepeatReply { text, .. } | NextStep::Reply { text, .. } => {
            let reply = match reasoning {
                Some(reasoning) => ModelReply::ReasoningReply { reasoning, text },
                None => ModelReply::Text(text),
            };
            (reply, held, Vec::new())
        }
        NextStep::Failure {
            failure,
            looping_call,
            violation,
            ..
        } => {
            let observed = failure.observed();
            let reply = match failure {
                Failure::Status(status) => ModelReply::Refusal(status),
                Failure::StreamError(stream_error) => ModelReply::StreamError(stream_error),
                Failure::Cut { .. } => ModelReply::CutReply,
                Failure::Dropped { .. } => ModelReply::Dropped,
                Failure::MalformedBody { .. } => ModelReply::Malformed,
                Failure::Hang { .. } => ModelReply::Hang,
                Failure::DoomLoop { .. } => match looping_call {
                    Some((call_id, call)) => ModelReply::ToolCall { call_id, call },
                    None => ModelReply::LoopingReply {
                        reported: request.asks_for_loop_report,
                    },
                },
            };
            (reply, Some(observed), violation.into_iter().collect())
        }
        NextStep::Call {
            call_id, picked, ..
        } => (
            ModelReply::ToolCall {
                call_id,
                call: picked,
            },
            held,
            Vec::new(),
        ),
        NextStep::NotOffered {
            violation, reply, ..
        } => (ModelReply::Text(reply), held, vec![violation]),
    }
}

struct ReplayingScript {
    entries: ScriptEntries,
    /// Requests that reached the script, the count a pinned entry names.
    requests: usize,
    /// Entries whose reply was served or whose turn a pin took over; each counts once, so a
    /// resumed turn a pin already superseded cannot hide a later turn that never answered.
    accounted: BTreeSet<usize>,
    /// The subset of `accounted` a pin took over mid tool call; the pinned turn stands in for
    /// their reply.
    superseded: BTreeSet<usize>,
    progress: Progress,
    failures: Vec<EntryFailures>,
}

impl ReplayingScript {
    fn new(entries: ScriptEntries) -> Self {
        let failures = entries
            .iter()
            .map(|entry| EntryFailures(entry.failures.iter().map(FailureBudget::new).collect()))
            .collect();
        ReplayingScript {
            entries,
            requests: 0,
            accounted: BTreeSet::new(),
            superseded: BTreeSet::new(),
            progress: Progress::Unserved,
            failures,
        }
    }

    /// `None` while every entry is pinned and none has been reached.
    fn serve(&mut self, request: &ScriptRequest) -> Option<ServedRequest> {
        let mut violations = Vec::new();
        violations.extend(self.run_pending_check(request));
        if !self.repeats_last_reply(request) {
            self.admit();
        }
        let step = self.next_step(request)?;
        self.record(&step, request);
        let entry = self.entries.at(step.entry_index());
        let hold = entry.stall;
        let reasoning = entry.reasoning.clone();
        let held = hold.map(|_| ObservedFailure::Stalled);
        let (reply, observed, rendered) = render(step, request, held, reasoning);
        violations.extend(rendered);
        Some(ServedRequest {
            served: ServedReply {
                reply,
                hold,
                observed,
            },
            violations,
        })
    }

    fn run_pending_check(&mut self, request: &ScriptRequest) -> Option<ScriptViolation> {
        let Progress::Calling {
            call_id,
            pending_check,
            ..
        } = &mut self.progress
        else {
            return None;
        };
        let result = request.results.get(call_id.as_str())?;
        let check = pending_check.take()?;
        if check.is_met_by(result) {
            return None;
        }
        Some(ScriptViolation::ResultMismatch {
            conversation: request.conversation.number(),
            call_id: call_id.clone(),
            expected: check.label().to_owned(),
            result: result.clone(),
        })
    }

    /// A resend of the request that already took the last answer, a reply or a tool call, which
    /// repeats it without counting as a new request or firing a pin.
    fn repeats_last_reply(&self, request: &ScriptRequest) -> bool {
        matches!(
            &self.progress,
            Progress::Replied { taken_by, .. } | Progress::Calling { taken_by, .. }
                if *taken_by == request.body_hash
        )
    }

    /// Count the request; an entry pinned to it takes over, whatever was serving. A turn interrupted
    /// mid call or mid failure is superseded, so the drop check does not require the reply it never reached.
    fn admit(&mut self) {
        self.requests += 1;
        let Some(pinned) = self.entries.pinned_to(self.requests) else {
            return;
        };
        match &self.progress {
            Progress::Calling { entry_index, .. }
            | Progress::Failing { entry_index }
            | Progress::Pinned { entry_index } => {
                let taken_over = *entry_index;
                self.accounted.insert(taken_over);
                self.superseded.insert(taken_over);
            }
            Progress::Unserved | Progress::Replied { .. } => {}
        }
        self.progress = Progress::Pinned {
            entry_index: pinned,
        };
    }

    fn entry_to_serve(&self) -> Option<usize> {
        match &self.progress {
            Progress::Unserved => self.entries.next_unpinned(None),
            Progress::Pinned { entry_index }
            | Progress::Calling { entry_index, .. }
            | Progress::Failing { entry_index } => Some(*entry_index),
            Progress::Replied {
                entry_index, next, ..
            } => Some(next.unwrap_or(*entry_index)),
        }
    }

    fn next_step(&self, request: &ScriptRequest) -> Option<NextStep> {
        if let Progress::Replied {
            entry_index,
            taken_by,
            ..
        } = &self.progress
            && *taken_by == request.body_hash
        {
            return Some(NextStep::RepeatReply {
                entry_index: *entry_index,
                text: self.entries.at(*entry_index).reply.clone(),
            });
        }
        let index = self.entry_to_serve()?;
        let entry = self.entries.at(index);
        if let Some(chosen) = self
            .failures
            .get(index)
            .and_then(EntryFailures::next_failure)
        {
            return Some(self.failure_step(index, chosen, request));
        }
        if let Progress::Replied { next: None, .. } = &self.progress {
            return Some(NextStep::Reply {
                entry_index: index,
                text: entry.reply.clone(),
            });
        }
        let next_call = self
            .entries
            .calls(index, request.conversation)
            .find(|(call_id, _)| !request.results.contains_key(call_id));
        let Some((call_id, call)) = next_call else {
            return Some(NextStep::Reply {
                entry_index: index,
                text: entry.reply.clone(),
            });
        };
        Some(match call.tool.pick(&request.offered, &call.arguments) {
            Some(picked) => NextStep::Call {
                entry_index: index,
                call_id,
                pending_check: call.result_check.clone(),
                picked,
            },
            None => NextStep::NotOffered {
                entry_index: index,
                violation: ScriptViolation::ToolNotOffered {
                    conversation: request.conversation.number(),
                    call_id,
                    tool: call.tool.clone(),
                    offered: request.offered.names(),
                },
                reply: entry.reply.clone(),
            },
        })
    }

    /// Decides which failure answers and, for a doom loop, the call to repeat under a fresh call id
    /// per repetition; `serve` renders the reply. The looping reply stands in when the entry has no
    /// call or the request does not offer that tool.
    fn failure_step(
        &self,
        entry_index: usize,
        chosen: ChosenFailure,
        request: &ScriptRequest,
    ) -> NextStep {
        let ChosenFailure {
            index: failure_index,
            failure,
            repetition,
        } = chosen;
        let (looping_call, violation) = match &failure {
            Failure::Status(_)
            | Failure::StreamError(_)
            | Failure::Cut { .. }
            | Failure::Dropped { .. }
            | Failure::MalformedBody { .. }
            | Failure::Hang { .. } => (None, None),
            Failure::DoomLoop { .. } => {
                match self.entries.calls(entry_index, request.conversation).next() {
                    None => (None, None),
                    Some((id, call)) => {
                        let call_id = format!("{id}_loop{repetition}");
                        match call.tool.pick(&request.offered, &call.arguments) {
                            Some(picked) => (Some((call_id, picked)), None),
                            None => (
                                None,
                                Some(ScriptViolation::ToolNotOffered {
                                    conversation: request.conversation.number(),
                                    call_id,
                                    tool: call.tool.clone(),
                                    offered: request.offered.names(),
                                }),
                            ),
                        }
                    }
                }
            }
        };
        NextStep::Failure {
            entry_index,
            failure_index,
            failure,
            looping_call,
            violation,
        }
    }

    fn record(&mut self, step: &NextStep, request: &ScriptRequest) {
        match step {
            NextStep::RepeatReply { .. } => {}
            NextStep::Failure {
                entry_index,
                failure_index,
                ..
            } => {
                let Some(budget) = self.failures.get_mut(*entry_index) else {
                    unreachable!("failure entry index came from next_step");
                };
                budget.spend(*failure_index);
                self.progress = Progress::Failing {
                    entry_index: *entry_index,
                };
            }
            NextStep::Call {
                entry_index,
                call_id,
                pending_check,
                ..
            } => {
                self.progress = Progress::Calling {
                    entry_index: *entry_index,
                    call_id: call_id.clone(),
                    pending_check: pending_check.clone(),
                    taken_by: request.body_hash,
                };
            }
            NextStep::Reply { entry_index, .. } | NextStep::NotOffered { entry_index, .. } => {
                self.accounted.insert(*entry_index);
                self.progress = Progress::Replied {
                    entry_index: *entry_index,
                    taken_by: request.body_hash,
                    next: self.entries.next_unpinned(Some(*entry_index)),
                };
            }
        }
    }

    fn unfinished(&self, conversation: ConversationId) -> Option<ScriptViolation> {
        (self.accounted.len() < self.entries.len()).then_some(ScriptViolation::Unfinished {
            conversation: conversation.number(),
            replies_served: self.accounted.len() - self.superseded.len(),
            superseded: self.superseded.len(),
            entry_count: self.entries.len(),
        })
    }
}

struct WaitingScript {
    opening: TextCheck,
    script: ReplayingScript,
}

impl WaitingScript {
    fn is_claimed_by(&self, request: &ScriptRequest) -> bool {
        request
            .first_system_message
            .as_deref()
            .is_some_and(|text| self.opening.is_met_by(text))
    }
}

#[derive(Default)]
struct ReplayState {
    scripts: BTreeMap<ConversationId, ReplayingScript>,
    waiting: Vec<WaitingScript>,
    recorded: Vec<ScriptViolation>,
}

impl ReplayState {
    fn claim_script(&mut self, request: &ScriptRequest) -> Option<&mut ReplayingScript> {
        match self.scripts.entry(request.conversation) {
            Entry::Occupied(entry) => Some(entry.into_mut()),
            Entry::Vacant(entry) => {
                let position = self
                    .waiting
                    .iter()
                    .position(|waiting| waiting.is_claimed_by(request))?;
                Some(entry.insert(self.waiting.remove(position).script))
            }
        }
    }

    fn violations(&self) -> Vec<ScriptViolation> {
        self.recorded
            .iter()
            .cloned()
            .chain(
                self.scripts
                    .iter()
                    .filter_map(|(conversation, script)| script.unfinished(*conversation)),
            )
            .chain(
                self.waiting
                    .iter()
                    .map(|waiting| ScriptViolation::Unopened {
                        expected: waiting.opening.label().to_owned(),
                    }),
            )
            .collect()
    }
}

#[derive(Clone, Default)]
pub(crate) struct ConversationReplay {
    state: Arc<std::sync::Mutex<ReplayState>>,
}

impl ConversationReplay {
    /// Two scripts for one conversation, or two turns of a script pinned to one request, panic here
    /// rather than at serve time.
    pub(crate) fn set(&self, conversations: impl IntoIterator<Item = Conversation>) {
        let mut named = BTreeMap::new();
        let mut waiting = Vec::new();
        for conversation in conversations {
            let (target, entries) = conversation.into_script();
            if let Some(request) = entries.request_pinned_twice() {
                panic!("two turns of the script for {target} are pinned to request {request}");
            }
            let replaying = ReplayingScript::new(entries);
            match target {
                ScriptTarget::Conversation(conversation) => {
                    if named.insert(conversation, replaying).is_some() {
                        panic!("duplicate script for {conversation}");
                    }
                }
                ScriptTarget::Opening(opening) => waiting.push(WaitingScript {
                    opening,
                    script: replaying,
                }),
            }
        }
        let mut state = self.state.lock().unwrap();
        let recorded = state.violations();
        *state = ReplayState {
            scripts: named,
            waiting,
            recorded,
        };
    }

    #[must_use]
    pub(crate) fn violations(&self) -> Vec<ScriptViolation> {
        self.state.lock().unwrap().violations()
    }

    #[must_use = "assert on the violations; finishing disarms the drop check"]
    pub(crate) fn finish(&self) -> Vec<ScriptViolation> {
        std::mem::take(&mut *self.state.lock().unwrap()).violations()
    }

    /// `None` for an auxiliary request, a conversation without a script, or a script none of whose
    /// turns has started.
    pub(crate) fn respond(&self, request: &InferenceRequest<'_>) -> Option<ServedReply> {
        let conversation = request.conversation()?;
        let script_request = ScriptRequest::new(conversation, request);
        let mut state = self.state.lock().unwrap();
        let served = state
            .claim_script(&script_request)?
            .serve(&script_request)?;
        state.recorded.extend(served.violations);
        Some(served.served)
    }
}

#[cfg(test)]
#[path = "conversation_replay_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "conversation_replay_failure_tests.rs"]
mod failure_tests;
