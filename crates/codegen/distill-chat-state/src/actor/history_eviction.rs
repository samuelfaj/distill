// Modified for Distill by Samuel Fajreldines, 2026.
//! Batched, round-age eviction of old large tool output from the retained history.
//!
//! Every main request resends the whole history, so a 20 KB read made 40
//! rounds ago is paid for, cached or not, on each of those 40 calls. This pass
//! replaces such a result with its first and last lines plus a pointer to a
//! stored copy of the original; a result a later identical call superseded
//! becomes a one-line pointer, and an old `write` body or long shell script in
//! a tool call's arguments becomes a stub naming its stored copy.
//!
//! Rewriting an old item breaks the provider's prompt cache from that item
//! on, so the pass never runs per request. It fires
//! - at a moment the cache is already cold: a model switch, a compaction
//!   (unless it kept a cached prefix with tool rounds, as a fork does), or an
//!   idle gap longer than any provider's default cache lifetime; and
//! - only with `d6_warm_batches` (off by default: it rewrites sent history on
//!   a warm cache, and its payback is unmeasured), warm, at most once per
//!   [`BATCH_INTERVAL_ROUNDS`], when the bytes it removes, replayed over the
//!   next interval, outweigh re-billing the suffix it invalidates.
//!
//! A digest is never rewritten again, so each item breaks the cache once. The
//! original is stored before anything is replaced; a store that refuses (no
//! store, a secret-looking payload, a skill or a user's answer) keeps the
//! bytes as they were.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use distill_sampling_types::{ContentPart, ConversationItem, ToolCall};

use super::ChatStateActor;
use super::mutations::HistoryRewrite;

/// Results and argument values smaller than this stay as they are.
pub const MIN_EVICT_BYTES: usize = 4_096;
/// Tool rounds after its call before a result or argument may be evicted.
pub const AGE_ROUNDS: usize = 20;
/// A result a later identical call superseded may go after this many rounds.
pub const SUPERSEDED_AGE_ROUNDS: usize = 10;
/// Warm batches are at least this many tool rounds apart; it is also the
/// replay horizon the payback gate assumes for the bytes a batch removes.
pub const BATCH_INTERVAL_ROUNDS: usize = 25;
/// A cache break re-bills the suffix at full price instead of ~0.1x cached
/// (0.9x extra, plus a cache-write premium on some providers): 10x covers both.
const BUST_COST_FACTOR: usize = 10;
/// No model output for this long means the provider cache has expired: every
/// provider's default prompt-cache lifetime is shorter.
pub const COLD_IDLE: Duration = Duration::from_secs(60 * 60);

/// Lines and bytes a head/tail digest keeps at each end.
const EDGE_LINES: usize = 8;
const EDGE_BYTES: usize = 300;
/// A kept line longer than this is cut.
const LINE_BYTES: usize = 160;
/// Bytes of a long shell command kept in its call's arguments.
const COMMAND_HEAD_BYTES: usize = 600;
/// Allowance for the pointer line before the stored path is known.
const POINTER_ESTIMATE_BYTES: usize = 320;

/// Opens every head/tail digest; a result that starts with it is never evicted again.
pub const EVICTED_MARKER: &str = "[evicted from history";
/// Opens every superseded-copy pointer.
pub const SUPERSEDED_MARKER: &str = "[superseded in history";
/// Opens a whole-file read that returned the same bytes as an earlier call
/// still in history; the call id follows, then a space. That earlier copy is
/// pinned: evicting it would leave the note pointing at nothing.
pub const READ_REUSE_NOTE_PREFIX: &str = "[unchanged since call ";

/// Why the next request will miss the provider cache anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColdReason {
    ModelSwitch,
    Compaction,
    Idle,
}

impl ColdReason {
    fn label(self) -> &'static str {
        match self {
            Self::ModelSwitch => "batch:cold-model-switch",
            Self::Compaction => "batch:cold-compaction",
            Self::Idle => "batch:cold-idle",
        }
    }
}

/// Per-session bookkeeping for the pass (in memory: a resumed session starts fresh).
#[derive(Debug, Default)]
pub(crate) struct EvictionState {
    /// Tool rounds in the conversation when the last batch was attempted.
    /// Inherited history (a fork, a resume) starts as batched: a fork's first
    /// request rides its parent's cached prefix.
    pub last_batch_rounds: usize,
    /// Set when the next request is known to be cold; cleared by that request.
    pub cold: Option<ColdReason>,
    /// When the model last produced an assistant item in this process.
    pub last_model_output: Option<Instant>,
    /// Paths of evicted reads and evicted commands, to count re-reads.
    pub reread_subjects: BTreeSet<String>,
    /// Stored copies the pass pointed at, to count reads of them.
    pub stored_paths: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Result { index: usize },
    Argument { index: usize, call: usize, field: String },
}

impl Target {
    fn index(&self) -> usize {
        match self {
            Self::Result { index } | Self::Argument { index, .. } => *index,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Form {
    HeadTail,
    Superseded { by: String },
    WriteContent { path: String },
    CommandHead,
}

#[derive(Debug, Clone)]
struct Candidate {
    target: Target,
    form: Form,
    tool_name: String,
    arguments: Arc<str>,
    payload: Arc<str>,
    age: usize,
    subject: Option<String>,
    saving: usize,
}

/// What one look at the history found.
#[derive(Debug)]
pub(crate) struct Plan {
    candidates: Vec<Candidate>,
    /// Tool rounds in the conversation.
    pub rounds: usize,
    /// Estimated bytes the batch would remove.
    pub saving: usize,
    /// Bytes from the first item the batch would change to the end.
    pub suffix_bytes: usize,
}

/// One replacement, with what it was.
#[derive(Debug, Clone)]
pub(crate) struct Replacement {
    target: Target,
    text: String,
    decision: &'static str,
    original_bytes: usize,
    subject: Option<String>,
    stored_path: String,
}

/// The tool's own name: `mcp:server/read_file` and `functions__read_file` are `read_file`.
fn tool_kind(name: &str) -> String {
    let name = name.rsplit([':', '/']).next().unwrap_or(name);
    name.rsplit("__").next().unwrap_or(name).to_ascii_lowercase()
}

fn is_read_tool(kind: &str) -> bool {
    matches!(kind, "read_file" | "read")
}

fn is_shell_tool(kind: &str) -> bool {
    matches!(kind, "run_terminal_command" | "run_terminal_cmd" | "bash" | "shell")
}

fn is_write_tool(kind: &str) -> bool {
    matches!(kind, "write" | "write_file" | "create_file")
}

fn string_field<'a>(args: &'a serde_json::Value, keys: &[&str]) -> Option<(&'a str, String)> {
    keys.iter().find_map(|key| {
        args.get(*key)
            .and_then(serde_json::Value::as_str)
            .map(|value| (value, (*key).to_owned()))
    })
}

const PATH_KEYS: [&str; 4] = ["target_file", "file_path", "path", "filePath"];
const COMMAND_KEYS: [&str; 3] = ["command", "cmd", "script"];
const CONTENT_KEYS: [&str; 3] = ["content", "contents", "file_text"];

/// What a read or a shell call is about: `read:<path>` or `shell:<command>`.
/// Re-reads of an evicted output are counted against this.
fn call_subject(call: &ToolCall) -> Option<String> {
    let kind = tool_kind(&call.name);
    let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    if is_read_tool(&kind) {
        string_field(&args, &PATH_KEYS).map(|(path, _)| format!("read:{path}"))
    } else if is_shell_tool(&kind) {
        string_field(&args, &COMMAND_KEYS).map(|(command, _)| format!("shell:{command}"))
    } else {
        None
    }
}

/// Two calls with the same key return the same thing as of their own time, so
/// the later one supersedes the earlier: a read of the same path and range, or
/// the same shell command in the same directory.
fn supersession_key(call: &ToolCall) -> Option<String> {
    let subject = call_subject(call)?;
    let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    let bound = |key: &str| {
        args.get(key)
            .filter(|value| !value.is_null())
            .map(ToString::to_string)
            .unwrap_or_default()
    };
    if !subject.starts_with("read:") {
        let directory: Vec<String> = ["workdir", "cwd", "directory", "working_directory"]
            .iter()
            .map(|key| bound(key))
            .collect();
        return Some(format!("{subject}\u{0}{}", directory.join("\u{0}")));
    }
    Some(format!("{subject}\u{0}{}\u{0}{}", bound("offset"), bound("limit")))
}

fn item_bytes(item: &ConversationItem) -> usize {
    match item {
        ConversationItem::System(s) => s.content.len(),
        ConversationItem::User(u) => u
            .content
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => text.len(),
                ContentPart::Image { url } => url.len(),
            })
            .sum(),
        ConversationItem::Assistant(a) => {
            a.content.len()
                + a.tool_calls
                    .iter()
                    .map(|call| call.arguments.len())
                    .sum::<usize>()
        }
        ConversationItem::ToolResult(tr) => tr.content.len(),
        ConversationItem::BackendToolCall(b) => b.text_summary().len(),
        ConversationItem::Reasoning(r) => {
            distill_sampling_types::reasoning_item_text(r).len()
                + r.encrypted_content.as_deref().map(str::len).unwrap_or(0)
        }
    }
}

/// Tool rounds: assistant items that made at least one tool call.
pub(crate) fn count_rounds(conversation: &[ConversationItem]) -> usize {
    conversation
        .iter()
        .filter(|item| matches!(item, ConversationItem::Assistant(a) if !a.tool_calls.is_empty()))
        .count()
}

fn already_evicted(text: &str) -> bool {
    text.starts_with(EVICTED_MARKER) || text.starts_with(SUPERSEDED_MARKER)
}

/// Whether a tool call id is used twice (by two calls or by two results). A
/// provider that reuses ids makes "the call of this result" ambiguous, so the
/// passes that map results to calls leave such a history as it is.
pub fn ambiguous_call_ids(conversation: &[ConversationItem]) -> bool {
    let mut calls = BTreeSet::new();
    let mut results = BTreeSet::new();
    conversation.iter().any(|item| match item {
        ConversationItem::Assistant(assistant) => assistant
            .tool_calls
            .iter()
            .any(|call| !calls.insert(call.id.as_ref())),
        ConversationItem::ToolResult(tr) => !results.insert(tr.tool_call_id.as_str()),
        _ => false,
    })
}

/// Ids of the earlier copies whole-read reuse notes name. Such a copy stays
/// whole: evicting or clearing it would leave the note pointing at nothing.
pub(crate) fn reuse_note_targets(conversation: &[ConversationItem]) -> BTreeSet<&str> {
    conversation
        .iter()
        .filter_map(|item| match item {
            ConversationItem::ToolResult(tr) => tr
                .content
                .strip_prefix(READ_REUSE_NOTE_PREFIX)
                .and_then(|rest| rest.split_whitespace().next()),
            _ => None,
        })
        .collect()
}

/// Opens the ingest-time note for a payload already sent this session
/// (`distill_workspace::jev::reduce::reuse_note`).
const SENT_CONTENT_NOTE_PREFIX: &str = "[unchanged content:";

/// Whether `later` holds the output itself, so a pointer to it loses nothing
/// of `earlier`: not a note, digest or utility selection that points
/// elsewhere, and not much shorter than the copy it would replace.
fn holds_full_output(later: &str, earlier: &str) -> bool {
    later.len().saturating_mul(2) >= earlier.len()
        && !already_evicted(later)
        && ![
            READ_REUSE_NOTE_PREFIX,
            SENT_CONTENT_NOTE_PREFIX,
            COMPACTION_DIGEST_MARKER,
            super::request_builder::HARD_CLEAR_PLACEHOLDER,
        ]
        .iter()
        .any(|marker| later.starts_with(marker))
        && !later.contains("by verified utility selection")
}

fn clip_line(line: &str) -> (String, bool) {
    if line.len() <= LINE_BYTES {
        return (line.to_owned(), false);
    }
    let mut end = LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}…", line.get(..end).unwrap_or_default()), true)
}

/// The first and last lines a digest keeps, the total line count and whether
/// any kept line was cut.
fn head_tail(text: &str) -> (Vec<String>, Vec<String>, usize, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let mut clipped = false;
    let mut take = |range: &mut dyn Iterator<Item = &&str>| {
        let mut kept = Vec::new();
        let mut bytes = 0usize;
        for line in range {
            if kept.len() >= EDGE_LINES || bytes >= EDGE_BYTES {
                break;
            }
            let (line, cut) = clip_line(line);
            clipped |= cut;
            bytes += line.len() + 1;
            kept.push(line);
        }
        kept
    };
    let head = take(&mut lines.iter());
    let rest = lines.get(head.len()..).unwrap_or_default();
    let mut tail = take(&mut rest.iter().rev());
    tail.reverse();
    (head, tail, lines.len(), clipped)
}

fn ask_hint(ask_stored_output: bool) -> &'static str {
    if ask_stored_output {
        ", or ask_stored_output with that path for a question about it"
    } else {
        ""
    }
}

fn head_tail_digest(text: &str, age: usize, path: &str, ask_stored_output: bool) -> String {
    let (head, tail, total, clipped) = head_tail(text);
    let omitted = total.saturating_sub(head.len() + tail.len());
    let cut = if clipped {
        format!(", long lines cut at {LINE_BYTES} bytes")
    } else {
        String::new()
    };
    let mut out = format!(
        "{EVICTED_MARKER} {age} rounds after this call: {} bytes in {total} lines; the first {} and last {} lines are kept below{cut}. The full output is stored at {path}; read it with read_file{}.]",
        text.len(),
        head.len(),
        tail.len(),
        ask_hint(ask_stored_output),
    );
    for line in &head {
        out.push('\n');
        out.push_str(line);
    }
    if omitted > 0 {
        out.push_str(&format!("\n[… {omitted} lines omitted …]"));
    }
    for line in &tail {
        out.push('\n');
        out.push_str(line);
    }
    out
}

fn estimated_digest_bytes(text: &str) -> usize {
    let (head, tail, _, _) = head_tail(text);
    head.iter().chain(tail.iter()).map(|line| line.len() + 1).sum::<usize>()
        + POINTER_ESTIMATE_BYTES
}

/// Everything in `conversation` the pass may evict now, oldest first.
pub(crate) fn plan(conversation: &[ConversationItem]) -> Plan {
    let rounds = count_rounds(conversation);
    if ambiguous_call_ids(conversation) {
        return Plan {
            candidates: Vec::new(),
            rounds,
            saving: 0,
            suffix_bytes: 0,
        };
    }
    // Round number of each assistant item that made tool calls, and each call.
    let mut round_of_index = BTreeMap::new();
    let mut calls: BTreeMap<&str, (usize, &ToolCall)> = BTreeMap::new();
    let mut latest_by_key: BTreeMap<String, (usize, String)> = BTreeMap::new();
    for (index, item) in conversation.iter().enumerate() {
        let ConversationItem::Assistant(assistant) = item else {
            continue;
        };
        if assistant.tool_calls.is_empty() {
            continue;
        }
        let round = round_of_index.len();
        round_of_index.insert(index, round);
        for call in &assistant.tool_calls {
            calls.insert(call.id.as_ref(), (round, call));
            if let Some(key) = supersession_key(call) {
                latest_by_key.insert(key, (round, call.id.to_string()));
            }
        }
    }
    let age_of = |round: usize| rounds.saturating_sub(round + 1);
    let pinned = reuse_note_targets(conversation);
    let result_of: BTreeMap<&str, &str> = conversation
        .iter()
        .filter_map(|item| match item {
            ConversationItem::ToolResult(tr) => Some((tr.tool_call_id.as_str(), tr.content.as_ref())),
            _ => None,
        })
        .collect();

    let mut candidates = Vec::new();
    for (index, item) in conversation.iter().enumerate() {
        match item {
            ConversationItem::ToolResult(tr) => {
                if tr.content.len() < MIN_EVICT_BYTES
                    || !tr.images.is_empty()
                    || already_evicted(&tr.content)
                    || pinned.contains(tr.tool_call_id.as_str())
                {
                    continue;
                }
                let Some(&(round, call)) = calls.get(tr.tool_call_id.as_str()) else {
                    continue;
                };
                let age = age_of(round);
                // Only a later result that holds the output itself may stand
                // in for this copy; a reuse note or selection pointing back
                // here would leave no copy at all.
                let superseded_by = supersession_key(call)
                    .and_then(|key| latest_by_key.get(&key))
                    .filter(|(latest, id)| {
                        *latest > round
                            && result_of
                                .get(id.as_str())
                                .is_some_and(|later| holds_full_output(later, &tr.content))
                    })
                    .map(|(_, id)| id.clone());
                let (form, floor, estimate) = match superseded_by {
                    Some(by) => (Form::Superseded { by }, SUPERSEDED_AGE_ROUNDS, POINTER_ESTIMATE_BYTES),
                    None => (Form::HeadTail, AGE_ROUNDS, estimated_digest_bytes(&tr.content)),
                };
                if age < floor {
                    continue;
                }
                candidates.push(Candidate {
                    target: Target::Result { index },
                    form,
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    payload: tr.content.clone(),
                    age,
                    subject: call_subject(call),
                    saving: tr.content.len().saturating_sub(estimate),
                });
            }
            ConversationItem::Assistant(assistant) => {
                let Some(&round) = round_of_index.get(&index) else {
                    continue;
                };
                let age = age_of(round);
                if age < AGE_ROUNDS {
                    continue;
                }
                for (position, call) in assistant.tool_calls.iter().enumerate() {
                    if call.arguments.len() < MIN_EVICT_BYTES {
                        continue;
                    }
                    let kind = tool_kind(&call.name);
                    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments)
                    else {
                        continue;
                    };
                    let (value, field, form, estimate) = if is_write_tool(&kind) {
                        let Some((value, field)) = string_field(&args, &CONTENT_KEYS) else {
                            continue;
                        };
                        let path = string_field(&args, &PATH_KEYS)
                            .map(|(path, _)| path.to_owned())
                            .unwrap_or_default();
                        (value, field, Form::WriteContent { path }, POINTER_ESTIMATE_BYTES)
                    } else if is_shell_tool(&kind) {
                        let Some((value, field)) = string_field(&args, &COMMAND_KEYS) else {
                            continue;
                        };
                        (value, field, Form::CommandHead, COMMAND_HEAD_BYTES + POINTER_ESTIMATE_BYTES)
                    } else {
                        continue;
                    };
                    if value.len() < MIN_EVICT_BYTES || value.contains(EVICTED_MARKER) {
                        continue;
                    }
                    candidates.push(Candidate {
                        target: Target::Argument {
                            index,
                            call: position,
                            field,
                        },
                        form,
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        payload: Arc::from(value),
                        age,
                        subject: None,
                        saving: value.len().saturating_sub(estimate),
                    });
                }
            }
            _ => {}
        }
    }
    candidates.retain(|candidate| candidate.saving > 0);
    let saving = candidates.iter().map(|candidate| candidate.saving).sum();
    let suffix_bytes = candidates
        .iter()
        .map(|candidate| candidate.target.index())
        .min()
        .map_or(0, |first| {
            conversation
                .get(first..)
                .unwrap_or_default()
                .iter()
                .map(item_bytes)
                .sum()
        });
    Plan {
        candidates,
        rounds,
        saving,
        suffix_bytes,
    }
}

/// The batch label when the plan should run now, `None` when it should wait.
///
/// Cold, any saving is free. Warm, only with `warm_batches`: the batch must be
/// due and must pay for the cache break: the bytes it removes, replayed for
/// [`BATCH_INTERVAL_ROUNDS`] at the cached rate, must outweigh re-billing the
/// suffix at full price.
pub(crate) fn batch_label(
    plan: &Plan,
    cold: Option<ColdReason>,
    last_batch_rounds: usize,
    warm_batches: bool,
) -> Option<&'static str> {
    if plan.candidates.is_empty() {
        return None;
    }
    if let Some(reason) = cold {
        return Some(reason.label());
    }
    if !warm_batches {
        return None;
    }
    let due = plan.rounds.saturating_sub(last_batch_rounds) >= BATCH_INTERVAL_ROUNDS;
    let pays = plan.saving.saturating_mul(BATCH_INTERVAL_ROUNDS)
        >= plan.suffix_bytes.saturating_mul(BUST_COST_FACTOR);
    (due && pays).then_some("batch:warm")
}

/// Stores each candidate's original through `store` and renders its
/// replacement. A candidate whose store refuses, or whose replacement would
/// not be shorter, is left out: those bytes stay as they are.
pub(crate) fn render(
    plan: Plan,
    ask_stored_output: bool,
    mut store: impl FnMut(&str, &str, &str) -> Option<String>,
) -> (Vec<Replacement>, usize) {
    let mut replacements = Vec::new();
    let mut refused = 0usize;
    for candidate in plan.candidates {
        let Some(path) = store(&candidate.tool_name, &*candidate.arguments, &*candidate.payload)
        else {
            refused += 1;
            continue;
        };
        let bytes = candidate.payload.len();
        let age = candidate.age;
        let ask = ask_hint(ask_stored_output);
        let (text, decision, original_bytes) = match &candidate.form {
            Form::HeadTail => (
                head_tail_digest(&candidate.payload, age, &path, ask_stored_output),
                "evict:head-tail",
                bytes,
            ),
            Form::Superseded { by } => (
                format!(
                    "{SUPERSEDED_MARKER}: a later identical call ({by}) returned this {}'s current output, so this {bytes}-byte copy from {age} rounds ago was evicted. The original is stored at {path}; read it with read_file{ask}.]",
                    if candidate.subject.as_deref().is_some_and(|s| s.starts_with("read:")) {
                        "read"
                    } else {
                        "command"
                    },
                ),
                "evict:superseded",
                bytes,
            ),
            Form::WriteContent { path: written } => {
                let stub = format!(
                    "{EVICTED_MARKER} {age} rounds after this call: {bytes} bytes of content written to {written}, stored at {path}. The file on disk holds the current content; read it again if you need it.]"
                );
                let Some(arguments) = with_field(&candidate.arguments, &candidate.target, &stub)
                else {
                    continue;
                };
                (arguments, "evict:write-content", candidate.arguments.len())
            }
            Form::CommandHead => {
                let mut end = COMMAND_HEAD_BYTES.min(bytes);
                while !candidate.payload.is_char_boundary(end) {
                    end -= 1;
                }
                let head = candidate.payload.get(..end).unwrap_or_default();
                let command = format!(
                    "{head}\n{EVICTED_MARKER} {age} rounds after this call: {} more bytes of this command; the full command is stored at {path}.]",
                    bytes - end
                );
                let Some(arguments) =
                    with_field(&candidate.arguments, &candidate.target, &command)
                else {
                    continue;
                };
                (arguments, "evict:command", candidate.arguments.len())
            }
        };
        if text.len() >= original_bytes {
            continue;
        }
        replacements.push(Replacement {
            target: candidate.target,
            text,
            decision,
            original_bytes,
            subject: candidate.subject,
            stored_path: path,
        });
    }
    (replacements, refused)
}

/// `arguments` with the target's string field set to `value`, still valid JSON.
fn with_field(arguments: &str, target: &Target, value: &str) -> Option<String> {
    let Target::Argument { field, .. } = target else {
        return None;
    };
    let mut args: serde_json::Value = serde_json::from_str(arguments).ok()?;
    *args.get_mut(field.as_str())? = serde_json::Value::String(value.to_owned());
    serde_json::to_string(&args).ok()
}

/// Applies the replacements in place; returns how many items changed.
pub(crate) fn apply(conversation: &mut [ConversationItem], replacements: &[Replacement]) -> usize {
    let mut changed = 0usize;
    for replacement in replacements {
        match (&replacement.target, conversation.get_mut(replacement.target.index())) {
            (Target::Result { .. }, Some(ConversationItem::ToolResult(tr))) => {
                tr.content = Arc::from(replacement.text.as_str());
                changed += 1;
            }
            (Target::Argument { call, .. }, Some(ConversationItem::Assistant(assistant))) => {
                if let Some(tool_call) = assistant.tool_calls.get_mut(*call) {
                    tool_call.arguments = Arc::from(replacement.text.as_str());
                    changed += 1;
                }
            }
            _ => {}
        }
    }
    changed
}

/// Opens every head/tail digest in a compaction input.
pub const COMPACTION_DIGEST_MARKER: &str = "[compaction input:";

/// An old tool result in a compaction input that is cold anyway.
#[derive(Debug, Clone)]
pub struct ColdCompactionCandidate {
    /// Position in the compaction input.
    pub index: usize,
    pub tool_name: String,
    pub arguments: Arc<str>,
    pub payload: Arc<str>,
}

/// The tool results of [`MIN_EVICT_BYTES`] or more before the newest human
/// turn of a compaction input, largest first. The current turn's output stays
/// verbatim, and so do results with images, digests, and an earlier copy a
/// reuse note names.
pub fn cold_compaction_candidates(conversation: &[ConversationItem]) -> Vec<ColdCompactionCandidate> {
    use distill_sampling_types::SyntheticReason;
    let human_turn = conversation.iter().rposition(|item| {
        matches!(item, ConversationItem::User(user)
            if matches!(user.synthetic_reason, SyntheticReason::Human | SyntheticReason::Interjection))
    });
    let Some(boundary) =
        human_turn.or_else(|| conversation.iter().rposition(|item| matches!(item, ConversationItem::User(_))))
    else {
        return Vec::new();
    };
    let calls: BTreeMap<&str, &ToolCall> = conversation
        .iter()
        .filter_map(|item| match item {
            ConversationItem::Assistant(assistant) => Some(assistant.tool_calls.iter()),
            _ => None,
        })
        .flatten()
        .map(|call| (call.id.as_ref(), call))
        .collect();
    if ambiguous_call_ids(conversation) {
        return Vec::new();
    }
    let pinned = reuse_note_targets(conversation);
    let mut candidates: Vec<ColdCompactionCandidate> = conversation
        .get(..boundary)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let ConversationItem::ToolResult(tr) = item else {
                return None;
            };
            if tr.content.len() < MIN_EVICT_BYTES
                || !tr.images.is_empty()
                || already_evicted(&tr.content)
                || tr.content.starts_with(COMPACTION_DIGEST_MARKER)
                || pinned.contains(tr.tool_call_id.as_str())
            {
                return None;
            }
            let call = calls.get(tr.tool_call_id.as_str())?;
            Some(ColdCompactionCandidate {
                index,
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
                payload: tr.content.clone(),
            })
        })
        .collect();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.payload.len()));
    candidates
}

/// `text`'s first and last lines under a line naming its stored copy, for a
/// compaction input.
pub fn cold_compaction_digest(text: &str, stored_path: &str) -> String {
    let (head, tail, total, clipped) = head_tail(text);
    let omitted = total.saturating_sub(head.len() + tail.len());
    let cut = if clipped {
        format!(", long lines cut at {LINE_BYTES} bytes")
    } else {
        String::new()
    };
    let mut out = format!(
        "{COMPACTION_DIGEST_MARKER} an old tool output of {} bytes in {total} lines; the first {} and last {} lines are kept below{cut}. The full output is stored at {stored_path}.]",
        text.len(),
        head.len(),
        tail.len(),
    );
    for line in &head {
        out.push('\n');
        out.push_str(line);
    }
    if omitted > 0 {
        out.push_str(&format!("\n[… {omitted} lines omitted …]"));
    }
    for line in &tail {
        out.push('\n');
        out.push_str(line);
    }
    out
}

/// Sets the tool result at `index` to `text` when that is shorter; returns
/// whether it changed.
pub fn shrink_tool_result(conversation: &mut [ConversationItem], index: usize, text: &str) -> bool {
    match conversation.get_mut(index) {
        Some(ConversationItem::ToolResult(tr)) if text.len() < tr.content.len() => {
            tr.content = Arc::from(text);
            true
        }
        _ => false,
    }
}

impl ChatStateActor {
    /// Runs the eviction pass before a request is built, when a batch is due
    /// (see the module docs). Every outcome is counted in the session's usage
    /// ledger: `history_batch` per batch (its `bytes_in` is the suffix the
    /// batch re-bills), `history_evict` per item (original and new bytes).
    pub(super) fn evict_old_history(&mut self, ask_stored_output: bool) {
        if !self.pruning_config.history_eviction {
            return;
        }
        if self.eviction.cold.is_none()
            && self
                .eviction
                .last_model_output
                .is_some_and(|at| at.elapsed() >= COLD_IDLE)
        {
            self.eviction.cold = Some(ColdReason::Idle);
        }
        let cold = self.eviction.cold.take();
        let warm_batches = self.pruning_config.history_eviction_warm;
        let rounds = count_rounds(&self.state.conversation);
        if cold.is_none()
            && (!warm_batches
                || rounds.saturating_sub(self.eviction.last_batch_rounds) < BATCH_INTERVAL_ROUNDS)
        {
            return;
        }
        let plan = plan(&self.state.conversation);
        let Some(label) = batch_label(&plan, cold, self.eviction.last_batch_rounds, warm_batches)
        else {
            return;
        };
        let (suffix_bytes, planned) = (plan.suffix_bytes, plan.candidates.len());
        self.eviction.last_batch_rounds = rounds;
        let persistence = &mut self.persistence;
        let (replacements, refused) = render(plan, ask_stored_output, |tool, args, payload| {
            persistence.archive_evicted_text(tool, args, payload)
        });
        let (changed, _) = self.rewrite_history(HistoryRewrite::RetainedPrune, |conversation| {
            apply(conversation, &replacements)
        });
        let usage = &mut self.state.session_usage;
        usage.record_utility_outcome("history_batch", label, changed as u64, suffix_bytes as u64, 0);
        for _ in 0..refused {
            usage.record_utility_outcome("history_evict", "keep:unstored", 0, 0, 0);
        }
        let mut removed = 0usize;
        for replacement in &replacements {
            usage.record_utility_outcome(
                "history_evict",
                replacement.decision,
                0,
                replacement.original_bytes as u64,
                replacement.text.len() as u64,
            );
            removed += replacement.original_bytes.saturating_sub(replacement.text.len());
            if let Some(subject) = &replacement.subject {
                self.eviction.reread_subjects.insert(subject.clone());
            }
            self.eviction
                .stored_paths
                .insert(replacement.stored_path.clone());
        }
        if changed > 0 {
            self.persistence.history_evicted();
        }
        tracing::info!(
            batch = label,
            planned,
            evicted = changed,
            refused,
            bytes_removed = removed,
            suffix_bytes,
            "ChatState: old tool output evicted from history"
        );
    }

    /// Notes a model item: its time (for the idle-cold check) and any call
    /// that reads an evicted output again, counted as `history_reread`.
    pub(super) fn observe_model_item(&mut self, item: &ConversationItem) {
        let ConversationItem::Assistant(assistant) = item else {
            return;
        };
        self.eviction.last_model_output = Some(Instant::now());
        if self.eviction.stored_paths.is_empty() && self.eviction.reread_subjects.is_empty() {
            return;
        }
        for call in &assistant.tool_calls {
            let decision = if self
                .eviction
                .stored_paths
                .iter()
                .any(|path| call.arguments.contains(path.as_str()))
            {
                "reread:stored"
            } else if call_subject(call)
                .is_some_and(|subject| self.eviction.reread_subjects.contains(&subject))
            {
                "reread:source"
            } else {
                continue;
            };
            self.state
                .session_usage
                .record_utility_outcome("history_reread", decision, 0, 0, 0);
        }
    }

    /// Marks the next request cold (a model switch).
    pub(super) fn mark_history_cold(&mut self, reason: ColdReason) {
        self.eviction.cold = Some(reason);
    }

    /// Before the history becomes `items`: what it holds then counts as
    /// batched, so a warm batch waits a full interval. A compaction also makes
    /// the next request cold, unless it kept a cached prefix with tool rounds
    /// in it (a fork re-pins its parent's): rewriting that would re-bill it.
    pub(super) fn note_history_replaced(&mut self, items: &[ConversationItem], is_compaction: bool) {
        self.eviction.last_batch_rounds = count_rounds(items);
        if is_compaction {
            let kept = shared_prefix_len(&self.state.conversation, items);
            if count_rounds(items.get(..kept).unwrap_or_default()) == 0 {
                self.eviction.cold = Some(ColdReason::Compaction);
            }
        }
    }
}

/// How many leading items `after` has exactly as `before` had them.
fn shared_prefix_len(before: &[ConversationItem], after: &[ConversationItem]) -> usize {
    before
        .iter()
        .zip(after)
        .take_while(|(a, b)| {
            matches!((serde_json::to_string(a), serde_json::to_string(b)), (Ok(a), Ok(b)) if a == b)
        })
        .count()
}

#[cfg(test)]
#[path = "history_eviction_tests.rs"]
mod tests;
