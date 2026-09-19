// Modified for Distill by Samuel Fajreldines, 2026.
//! What a test scripts for the model, one [`Conversation`] at a time: each turn's tool calls in order,
//! then its reply, with the failures that answer in their place while their counts last. Call ids carry
//! the conversation and call number ([`mock_call_id`]) so inherited history does not advance a child's script.

use std::fmt;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::conversation::ConversationId;
use crate::failure::{Failure, StatusFailure, StreamError};
use crate::tools::Tool;

const MOCK_CALL_ID_PREFIX: &str = "call_mock_";

/// `call_mock_<conversation>_<call_number>`, both counted from 1.
#[must_use]
pub fn mock_call_id(conversation: usize, call_number: usize) -> String {
    format!("{MOCK_CALL_ID_PREFIX}{conversation}_{call_number}")
}

pub(crate) fn mock_call_id_prefix(conversation: ConversationId) -> String {
    format!("{MOCK_CALL_ID_PREFIX}{}_", conversation.number())
}

/// A substring the result must contain, or a pattern it must match. The label doubles as the text
/// of the violation report.
#[derive(Debug, Clone)]
pub(crate) enum TextCheck {
    Contains(String),
    Matches { pattern: String, regex: Regex },
}

impl TextCheck {
    pub(crate) fn contains(expected: impl Into<String>) -> Self {
        TextCheck::Contains(expected.into())
    }

    pub(crate) fn matches(pattern: impl Into<String>) -> Result<Self, regex::Error> {
        let pattern = pattern.into();
        let regex = Regex::new(&pattern)?;
        Ok(TextCheck::Matches { pattern, regex })
    }

    /// The substring or pattern this checks for.
    pub(crate) fn label(&self) -> &str {
        match self {
            TextCheck::Contains(expected) => expected,
            TextCheck::Matches { pattern, .. } => pattern,
        }
    }

    pub(crate) fn is_met_by(&self, text: &str) -> bool {
        match self {
            TextCheck::Contains(expected) => text.contains(expected),
            TextCheck::Matches { regex, .. } => regex.is_match(text),
        }
    }
}

/// Arguments are written in Distill's shape; the next request's result for the call is checked.
#[derive(Debug, Clone)]
#[must_use]
pub struct MockToolCall {
    pub(crate) tool: Tool,
    pub(crate) arguments: Value,
    pub(crate) result_check: Option<TextCheck>,
}

impl MockToolCall {
    pub fn new(tool: Tool, arguments: Value) -> Self {
        MockToolCall {
            tool,
            arguments,
            result_check: None,
        }
    }

    /// Pin what the model is shown for this call after hooks or permissions acted on it.
    pub fn result_contains(mut self, expected: impl Into<String>) -> Self {
        self.result_check = Some(TextCheck::contains(expected));
        self
    }

    /// Pin the result against a regular expression, for a result whose exact text a case cannot
    /// spell (an absolute path, a rendered timestamp).
    pub fn result_matches(mut self, pattern: impl Into<String>) -> Result<Self, regex::Error> {
        self.result_check = Some(TextCheck::matches(pattern)?);
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ScriptEntry {
    pub(crate) tool_calls: Vec<MockToolCall>,
    pub(crate) reply: String,
    pub(crate) at_request: Option<usize>,
    pub(crate) stall: Option<Duration>,
    /// A reasoning stream served before the reply, so the client sees a thought first.
    pub(crate) reasoning: Option<String>,
    /// In the order the case declared them; each answers until its count is spent.
    pub(crate) failures: Vec<Failure>,
}

/// The last entry repeats once the script is served through, so there is always one to answer from.
#[derive(Debug, Clone)]
pub(crate) struct ScriptEntries {
    earlier: Vec<ScriptEntry>,
    last: ScriptEntry,
}

impl ScriptEntries {
    pub(crate) fn len(&self) -> usize {
        self.earlier.len() + 1
    }

    pub(crate) fn at(&self, index: usize) -> &ScriptEntry {
        self.earlier.get(index).unwrap_or(&self.last)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &ScriptEntry> {
        self.earlier.iter().chain(std::iter::once(&self.last))
    }

    pub(crate) fn pinned_to(&self, request: usize) -> Option<usize> {
        self.iter()
            .position(|entry| entry.at_request == Some(request))
    }

    pub(crate) fn next_unpinned(&self, after: Option<usize>) -> Option<usize> {
        self.iter()
            .enumerate()
            .skip(after.map_or(0, |after| after + 1))
            .find(|(_, entry)| entry.at_request.is_none())
            .map(|(index, _)| index)
    }

    pub(crate) fn request_pinned_twice(&self) -> Option<usize> {
        let mut pinned: Vec<usize> = self.iter().filter_map(|entry| entry.at_request).collect();
        pinned.sort_unstable();
        pinned.windows(2).find_map(|pair| {
            let [earlier, later] = pair else {
                return None;
            };
            (earlier == later).then_some(*earlier)
        })
    }

    /// Call ids continue the count of the calls the entries before it made.
    pub(crate) fn calls(
        &self,
        index: usize,
        conversation: ConversationId,
    ) -> impl Iterator<Item = (String, &MockToolCall)> {
        let calls_before: usize = self
            .earlier
            .iter()
            .take(index)
            .map(|entry| entry.tool_calls.len())
            .sum();
        self.at(index)
            .tool_calls
            .iter()
            .enumerate()
            .map(move |(position, call)| {
                (
                    mock_call_id(conversation.number(), calls_before + position + 1),
                    call,
                )
            })
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ScriptTarget {
    Conversation(ConversationId),
    Opening(TextCheck),
}

impl fmt::Display for ScriptTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptTarget::Conversation(conversation) => write!(f, "{conversation}"),
            ScriptTarget::Opening(opening) => {
                write!(f, "the conversation opened with {:?}", opening.label())
            }
        }
    }
}

/// The turn being built before `.reply` closes it; open once any setter has run.
#[derive(Debug, Clone, Default)]
struct PendingTurn {
    calls: Vec<MockToolCall>,
    at_request: Option<usize>,
    stall: Option<Duration>,
    reasoning: Option<String>,
    failures: Vec<Failure>,
}

impl PendingTurn {
    fn is_open(&self) -> bool {
        !self.calls.is_empty()
            || self.at_request.is_some()
            || self.stall.is_some()
            || self.reasoning.is_some()
            || !self.failures.is_empty()
    }

    fn close(self, reply: String) -> ScriptEntry {
        ScriptEntry {
            tool_calls: self.calls,
            reply,
            at_request: self.at_request,
            stall: self.stall,
            reasoning: self.reasoning,
            failures: self.failures,
        }
    }
}

/// `.reply` closes the turn being built: `.calls` after it starts the next turn, a second `.reply`
/// in a row is a turn with no calls, and every conversation ends with a `.reply`.
#[derive(Debug, Clone)]
#[must_use]
pub struct Conversation {
    target: ScriptTarget,
    turns: Vec<ScriptEntry>,
    pending: PendingTurn,
}

impl Conversation {
    /// Conversations are numbered from 1 in the order the mock opens them.
    pub fn nth(number: usize) -> Self {
        Conversation::new(ScriptTarget::Conversation(ConversationId::nth(number)))
    }

    /// Subagents started together arrive in either order, so each is found by its prompt.
    pub fn for_system_prompt(contains: impl Into<String>) -> Self {
        Conversation::new(ScriptTarget::Opening(TextCheck::contains(contains)))
    }

    fn new(target: ScriptTarget) -> Self {
        Conversation {
            target,
            turns: Vec::new(),
            pending: PendingTurn::default(),
        }
    }

    /// Served in order: each call, then its result, before the next call and the reply.
    pub fn calls(mut self, calls: impl IntoIterator<Item = MockToolCall>) -> Self {
        self.pending.calls.extend(calls);
        self
    }

    /// Serve this turn at the `nth` request of the conversation rather than in list order; the
    /// unpinned turns keep their order around it. Two turns pinned to one request panic.
    pub fn at_request(mut self, nth: usize) -> Self {
        assert!(nth >= 1, "requests are counted from 1");
        self.pending.at_request = Some(nth);
        self
    }

    /// Hold every answer for this turn, a failure's included, for `hold` before opening the stream.
    pub fn stall(mut self, hold: Duration) -> Self {
        self.pending.stall = Some(hold);
        self
    }

    /// Stream `reasoning` ahead of this turn's reply, so the client receives a thought before the answer.
    pub fn reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.pending.reasoning = Some(reasoning.into());
        self
    }

    pub fn refuse(mut self, failure: StatusFailure) -> Self {
        self.pending.failures.push(Failure::Status(failure));
        self
    }

    /// Answer the next `count` requests with [`crate::CUT_REPLY`] stopped at the output token limit.
    pub fn cut(mut self, count: usize) -> Self {
        self.pending.failures.push(Failure::Cut { count });
        self
    }

    pub fn fail_stream(mut self, stream_error: StreamError) -> Self {
        self.pending
            .failures
            .push(Failure::StreamError(stream_error));
        self
    }

    /// Answer the next `count` requests with this turn's first tool call again, each under a fresh
    /// call id the way a looping model issues them, or with [`crate::LOOPING_REPLY`] when the turn
    /// has no tool calls.
    pub fn doom_loop(mut self, count: usize) -> Self {
        self.pending.failures.push(Failure::DoomLoop { count });
        self
    }

    /// Close the connection with no body on the next `count` requests, before the response head.
    pub fn drop_connection(mut self, count: usize) -> Self {
        self.pending.failures.push(Failure::Dropped { count });
        self
    }

    /// Answer the next `count` requests with a body the client's stream decoder cannot parse.
    pub fn malformed_body(mut self, count: usize) -> Self {
        self.pending.failures.push(Failure::MalformedBody { count });
        self
    }

    /// Open the stream on the next `count` requests then never send a chunk, so the client's
    /// inference idle timeout fires.
    pub fn hang(mut self, count: usize) -> Self {
        self.pending.failures.push(Failure::Hang { count });
        self
    }

    pub fn reply(mut self, text: impl Into<String>) -> Self {
        let turn = std::mem::take(&mut self.pending).close(text.into());
        self.turns.push(turn);
        self
    }

    /// Panics for a conversation with no turns or an open turn no `.reply` closed.
    pub(crate) fn into_script(mut self) -> (ScriptTarget, ScriptEntries) {
        assert!(
            !self.pending.is_open(),
            "{}: an open turn after the last reply; close it with .reply()",
            self.target
        );
        let Some(last) = self.turns.pop() else {
            panic!("{}: no turns; add a .reply()", self.target);
        };
        (
            self.target,
            ScriptEntries {
                earlier: self.turns,
                last,
            },
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptViolation {
    ResultMismatch {
        conversation: usize,
        call_id: String,
        expected: String,
        result: String,
    },
    ToolNotOffered {
        conversation: usize,
        call_id: String,
        tool: Tool,
        offered: Vec<String>,
    },
    Unfinished {
        conversation: usize,
        replies_served: usize,
        /// Turns a pin took over mid tool call; counted toward the entries accounted for.
        superseded: usize,
        entry_count: usize,
    },
    Unopened {
        expected: String,
    },
}

impl fmt::Display for ScriptViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptViolation::ResultMismatch {
                conversation,
                call_id,
                expected,
                result,
            } => write!(
                f,
                "conversation {conversation} call {call_id}: result did not meet {expected:?}; got {result:?}"
            ),
            ScriptViolation::ToolNotOffered {
                conversation,
                call_id,
                tool,
                offered,
            } => write!(
                f,
                "conversation {conversation} call {call_id}: the request offered no name {tool} is known by; offered {offered:?}"
            ),
            ScriptViolation::Unfinished {
                conversation,
                replies_served,
                superseded,
                entry_count,
            } => {
                write!(
                    f,
                    "conversation {conversation}: {replies_served} of {entry_count} entries served"
                )?;
                if *superseded > 0 {
                    write!(f, ", {superseded} superseded by a pin")?;
                }
                Ok(())
            }
            ScriptViolation::Unopened { expected } => {
                write!(f, "no conversation opened with {expected:?}")
            }
        }
    }
}
