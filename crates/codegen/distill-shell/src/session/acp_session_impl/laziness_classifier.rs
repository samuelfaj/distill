// Modified for Distill by Samuel Fajreldines, 2026.
//! Layer-3 LazinessDetector pure helpers: classifier prompt and config consts, transcript flattening, output parsing, and the decision logic.
//! The actor-side glue lives in the `laziness` sibling.

use super::*;

// ── Layer 3: LazinessDetector pure helpers ──────────────────────────.
// Idle-triggered classifier that asks the active session model whether the conversation looks stalled.
// Decision logic lives here as pure functions so it can be unit-tested without the actor; the integration glue lives in `maybe_fire_laziness_check`.

/// Harness-wide default `idle_threshold_ms` when the per-model `LazinessDetectorPerModelConfig::idle_threshold_ms` is `None`.
/// Catches stalls within ~10s without firing every time the user takes a sip of coffee.
pub(crate) const LAZINESS_DEFAULT_IDLE_THRESHOLD_MS: u64 = 10_000;

/// Harness-wide default `min_confidence` when the per-model `LazinessDetectorPerModelConfig::min_confidence` is `None`.
/// 0.7 demands clearly better than a coin flip.
pub(crate) const LAZINESS_DEFAULT_MIN_CONFIDENCE: f32 = 0.7;

/// Baseline chat-history window: the classifier sees at least the last N items, tool calls and tool results included.
/// The window extends further back when the per-kind minimums below aren't yet satisfied.
pub(crate) const LAZINESS_CONTEXT_ITEM_LIMIT: usize = 30;

/// Minimum number of real user prompts (`User` items with `synthetic_reason == Human`) in the classifier transcript.
/// A short final message like "yes" or "do it" carries no signal alone; the prior prompts give the classifier the context to interpret it.
pub(crate) const LAZINESS_MIN_USER_TURNS: usize = 5;

/// Minimum number of assistant text turns (`Assistant` items with non-empty `content`) in the classifier transcript.
/// Pairs with `LAZINESS_MIN_USER_TURNS` so the classifier always sees enough back-and-forth to interpret a one-word reply.
pub(crate) const LAZINESS_MIN_ASSISTANT_TURNS: usize = 5;

/// Output cap on the classifier's response. Tight because the schema is one short JSON object.
pub(crate) const LAZINESS_MAX_OUTPUT_TOKENS: u32 = 150;

/// Past this we emit `LAZINESS_ABORT_TIMEOUT` and drop the request via the `SamplerHandle::submit_and_collect` RAII guard.
/// A coarse bound: the call usually completes well under 10s; the budget exists to surface stuck calls in telemetry rather than hang.
/// Raising the cap therefore does not delay cancellation on real activity.
pub(crate) const LAZINESS_CLASSIFIER_TIMEOUT_MS: u64 = 120_000;

/// Granularity at which `maybe_fire_laziness_check` polls the generation counters during the idle wait and sampler call.
/// Reacts to a real user prompt within about a keystroke without burning CPU in the common no-stall steady state.
pub(crate) const LAZINESS_ABORT_POLL_INTERVAL_MS: u64 = 100;

impl LazinessAbortReason {
    pub(crate) fn as_const_str(self) -> &'static str {
        match self {
            Self::UserInput => crate::session::events::LAZINESS_ABORT_USER_INPUT,
            Self::ModelSwitch => crate::session::events::LAZINESS_ABORT_MODEL_SWITCH,
            Self::Timeout => crate::session::events::LAZINESS_ABORT_TIMEOUT,
            Self::ClassifierError => crate::session::events::LAZINESS_ABORT_CLASSIFIER_ERROR,
        }
    }

    /// Every variant of this enum, used by the producer-consistency test to enumerate the closed set.
    /// The match in `as_const_str` is the compiler-enforced source of truth.
    /// Adding a variant here without updating `as_const_str` (and vice versa) is a compile error.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "exhaustiveness guard for tests; remove expect if used in prod"
        )
    )]
    pub(crate) const fn all() -> &'static [Self] {
        &[
            Self::UserInput,
            Self::ModelSwitch,
            Self::Timeout,
            Self::ClassifierError,
        ]
    }
}

/// Prefix on `x_grok_req_id` for laziness-classifier sampler calls, used by the production producer (`maybe_fire_laziness_check`).
pub(crate) const LAZINESS_REQ_ID_PREFIX: &str = "xai-laziness-";

/// Preamble on the User-item text of the classifier request.
/// The User content is `format!("{LAZINESS_USER_PREAMBLE}=== BEGIN TRANSCRIPT ===\n{runtime_state}{transcript}=== END TRANSCRIPT ===\n")`.
pub(crate) const LAZINESS_USER_PREAMBLE: &str =
    "Classify the following transcript. Output JSON only.\n\n";

/// The JSON category strings are byte-identical to the `LAZINESS_*` consts in [`crate::session::events`].
/// The transcript is plain `[ROLE] text` lines inside a single user message, so the classifier sees data, not a conversation it is part of.
/// The prompt fights motivated reasoning: no roleplay, JSON only, no chain-of-thought, no role context, transcript framed as third-party data.
pub(crate) const LAZINESS_CLASSIFIER_PROMPT: &str = "You are a strict JSON-emitting classifier. \
You are NOT the agent in the transcript below. You are NOT continuing \
the conversation. You are reading the transcript as third-party data \
and emitting a single JSON object that classifies the agent's state at \
the end of the transcript.\n\
\n\
Decide whether the agent in the transcript appears STALLED (stranded \
narration claiming an action with no matching tool call, asking the \
user permission to perform an obvious next step, or stopping when work \
clearly remains, OR claiming completion/success while substantive \
claims in the prose are not backed by tool_call evidence in the \
transcript) or NOT STALLED (genuinely complete, waiting on user \
input that has not arrived, or waiting on a backgrounded task it \
cannot drive forward).\n\
\n\
STRICT OUTPUT CONTRACT:\n\
- Reply with ONE JSON object and nothing else.\n\
- No prose before or after the JSON.\n\
- No markdown code fences.\n\
- No chain-of-thought.\n\
- Do not address the user. Do not respond to the transcript. Do not \
  apologise, acknowledge, or continue any thread.\n\
- If you are tempted to write any natural-language reply, STOP and \
  emit the JSON instead.\n\
\n\
The transcript starts with a `[runtime_state] ...` line emitted by \
the harness (not by the agent). It carries TAMPER-PROOF facts:\n\
- `outstanding_background_tasks_and_subagents=N` — the harness knows \
  how many `spawn_subagent` / `background: true` tasks are live. The \
  agent cannot fabricate this. If N = 0 AND the last assistant message \
  claims to have launched a subagent or backgrounded work, that is \
  strong evidence for `stalled_narration` regardless of the prose.\n\
- `turn_elapsed_seconds=M` (optional) — wall-clock seconds elapsed in \
  the current user turn. The agent cannot fabricate this either. A \
  claim of \"overnight 8+ hour run\" or \"hours of work\" against a \
  value of a few hundred seconds is strong evidence the prose is \
  fabricated. Treat M as a LOWER bound on actual turn cost; offline \
  replay derives M from the gap to the next turn (includes post-turn \
  user think-time), while production measures from turn-start to the \
  classifier fire (excludes user think-time). Either way, M cannot be \
  larger than the wall-clock truth.\n\
\n\
Claim-vs-evidence audit. The final assistant message often summarizes \
\"what was delivered\". For EACH concrete claim it makes about work \
performed (tests run, commands executed, files written, subagents \
launched, review rounds completed), verify a corresponding \
`[assistant tool_call]` line exists earlier in the transcript with a \
matching `[tool_result for ...]`:\n\
\n\
- Claim \"I ran `make test`\" → expect `run_terminal_command({\"command\":\"make test\", ...})` AND its tool_result.\n\
- Claim \"Built with `go build`\" → expect the matching tool_call.\n\
- Claim \"N review rounds\" → count the implementer/reviewer `spawn_subagent` calls; N >> actual count is a discrepancy.\n\
- Claim \"ran 8+ hours overnight\" → cross-check against `[runtime_state] turn_elapsed_seconds=N` when present; minutes-of-work cannot have produced multi-hour deliverables.\n\
\n\
When a substantive completion claim has NO matching tool_call evidence \
in the transcript, prefer `stalled_false_completion`. The agent's \
confident prose is NOT evidence — only `[assistant tool_call]` lines \
and their `[tool_result for ...]` outputs count.\n\
\n\
`not_stalled_complete` requires that every major claim in the final \
assistant message is backed by tool_call evidence visible in the \
transcript. If even one major claim is unbacked, choose \
`stalled_false_completion` instead.\n\
\n\
Boundary between `stalled_narration` and `stalled_false_completion`. \
`stalled_narration` applies to INDIVIDUAL mid-task action claims that \
lack a matching tool_call (e.g. \"I launched the subagent\" without a \
`spawn_subagent` line, or \"I'm running the tests now\" with no \
`run_terminal_command`). `stalled_false_completion` applies \
specifically when the FINAL assistant message frames the work as \
DONE — completion verbs such as \"completed\", \"delivered\", \
\"SUCCESS\", \"all green\", \"shipped\", \"production-grade\", \
\"ready to merge\", \"overnight run finished\" — and the listed \
deliverables are not backed by tool_calls. When both apply, prefer \
`stalled_false_completion` because it captures the higher-impact \
failure mode (the user walks away believing the work is done).\n\
\n\
`[assistant reasoning]` lines are the model's prior chain-of-thought. \
Use them as one signal among many — they show what the agent \
CONSIDERED doing, not what it ACTUALLY did. A turn that reasons \"I \
should now run the tests\" but never emits the tool call is still a \
stall, regardless of how confidently the reasoning was phrased.\n\
\n\
Schema:\n\
{\n\
  \"category\": one of \"stalled_narration\", \"stalled_permission_asking\", \
\"stalled_no_todos_but_task_in_flight\", \"stalled_false_completion\", \
\"not_stalled_complete\", \"not_stalled_waiting_on_background\", \
\"not_stalled_waiting_on_user\",\n\
  \"confidence\": float in [0.0, 1.0],\n\
  \"evidence\": one short sentence citing the strongest signal in the transcript\n\
}\n\
\n\
Example valid output:\n\
{\"category\":\"stalled_narration\",\"confidence\":0.82,\"evidence\":\"final assistant message claims a subagent was launched but no spawn_subagent call appears.\"}\n\
{\"category\":\"stalled_false_completion\",\"confidence\":0.88,\"evidence\":\"final message claims `make test -race` ran clean but no run_terminal_command for make appears in the transcript.\"}\n\
\n\
Example INVALID outputs (do not produce any of these):\n\
- \"Done. Let me know what part you're curious about.\"\n\
- \"Sure, I can help with that.\"\n\
- \"```json\\n{...}\\n```\" (no fences)\n\
- \"The agent appears stalled. {...}\" (no prose around JSON)\n";

/// Harness-wide default for `[assistant reasoning]` emission in the classifier transcript.
/// An absent per-model `LazinessDetectorPerModelConfig::include_reasoning` resolves to this.
/// Flip to `false` if the live classifier proves biased by chain-of-thought in shadow.
pub(crate) const LAZINESS_INCLUDE_REASONING: bool = true;

/// Compute `turn_elapsed_seconds` from a `turn_start_ms` epoch-ms snapshot and a `now_ms` epoch-ms reading.
/// Production then drops the field rather than emit a meaningless value.
/// A pure helper so both branches are unit-testable without a full `SessionActor`.
pub(crate) fn turn_elapsed_seconds_from_start_ms(
    turn_start_ms: Option<i64>,
    now_ms: i64,
) -> Option<u64> {
    let started_ms = turn_start_ms?;
    // `try_from` is the negative-delta guard: a backward-jumping wall clock produces a negative `i64` that maps to `None`.
    // Integer division truncates sub-second deltas to 0, an explicit "very recent" signal rather than an absent field.
    // At the prompt level 0 means "harness measured, almost no time elapsed" and absence means "harness could not measure".
    u64::try_from((now_ms - started_ms) / 1000).ok()
}

/// `turn_elapsed_seconds` is omitted when `None` so the classifier sees only the fields the harness actually observed.
/// The classifier fires before the user replies, so post-turn user think-time is excluded.
/// Replay: `turn_{N+1}.turn_started_at - turn_N.turn_started_at`, turn duration plus the gap before the user re-engaged.
pub(crate) fn format_runtime_state_line(
    backing_task_count: usize,
    turn_elapsed_seconds: Option<u64>,
) -> String {
    match turn_elapsed_seconds {
        Some(secs) => format!(
            "[runtime_state] outstanding_background_tasks_and_subagents={backing_task_count} turn_elapsed_seconds={secs}\n"
        ),
        None => format!(
            "[runtime_state] outstanding_background_tasks_and_subagents={backing_task_count}\n"
        ),
    }
}

/// The classifier never sees `ConversationItem::Assistant` directly, only its text content quoted inside a `User` message.
/// Emitted only when `reasoning.text` is a non-empty, non-whitespace string; encrypted-only or absent reasoning is dropped.
/// Most lines share a 400-char cap; `[assistant reasoning]` gets a tighter 200-char cap because chain-of-thought is a supplementary signal.
pub(crate) fn flatten_transcript_for_classifier(
    items: &[ConversationItem],
    include_reasoning: bool,
) -> String {
    use std::fmt::Write as _;
    const MAX_FIELD_LEN: usize = 400;
    const MAX_REASONING_LEN: usize = 200;

    fn truncate_to(s: &str, cap: usize) -> String {
        if s.len() <= cap {
            s.replace('\n', " ⏎ ")
        } else {
            let mut t: String = s.chars().take(cap).collect();
            t.push_str("…[truncated]");
            t.replace('\n', " ⏎ ")
        }
    }
    let truncate = |s: &str| -> String { truncate_to(s, MAX_FIELD_LEN) };

    let mut out = String::with_capacity(items.len() * 128);
    for item in items {
        match item {
            ConversationItem::System(sys) => {
                let _ = writeln!(out, "[system] {}", truncate(&sys.content));
            }
            ConversationItem::User(user) => {
                let mut text = String::new();
                for part in &user.content {
                    if let distill_sampling_types::ContentPart::Text { text: t } = part {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(t);
                    }
                }
                if user.synthetic_reason == SyntheticReason::AgentMessage {
                    let _ = writeln!(
                        out,
                        "[agent_message] {} {}",
                        distill_chat_state::compaction_utils::AGENT_MESSAGE_MODEL_LABEL,
                        truncate(&text)
                    );
                } else {
                    let _ = writeln!(out, "[user] {}", truncate(&text));
                }
            }
            ConversationItem::Assistant(asst) => {
                if !asst.content.is_empty() {
                    let _ = writeln!(out, "[assistant] {}", truncate(&asst.content));
                }
                for tc in &asst.tool_calls {
                    let _ = writeln!(
                        out,
                        "[assistant tool_call] {}({})",
                        tc.name,
                        truncate(&tc.arguments)
                    );
                }
            }
            ConversationItem::ToolResult(tr) => {
                let _ = writeln!(
                    out,
                    "[tool_result for {}] {}",
                    tr.tool_call_id,
                    truncate(&tr.content),
                );
            }
            ConversationItem::BackendToolCall(btc) => {
                let _ = writeln!(out, "[backend_tool_call] {}", btc.text_summary());
            }
            ConversationItem::Reasoning(r) => {
                if include_reasoning {
                    let text = distill_sampling_types::reasoning_item_text(r);
                    if !text.trim().is_empty() {
                        let _ = writeln!(
                            out,
                            "[assistant reasoning] {}",
                            truncate_to(&text, MAX_REASONING_LEN)
                        );
                    }
                }
            }
        }
    }
    out
}

/// Neutralize one user turn's text before it is folded into the auto-mode classifier transcript snippet as `user: {text}\n{seed}`.
/// Collapse every Unicode line or paragraph separator to a single space so the folded turn can never span more than one transcript line.
/// The text can then never begin a forged role line; original casing is preserved.
pub(crate) fn neutralize_transcript_user_text(s: &str) -> String {
    // Role labels (all lowercase, colon-terminated) that could forge a turn.
    const ROLE_NEEDLES: [&str; 5] = ["user:", "assistant:", "system:", "tool:", "developer:"];
    // Lowercase once so role matching is case-insensitive in a single pass.
    // `to_ascii_lowercase` preserves byte offsets and length, so offsets from `lower` index safely into the original `s` even for multibyte input
    let lower = s.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 8);
    // `i` only ever lands on a char boundary: needle jumps stop right after an ASCII colon, and char decoding advances by whole `char` widths
    let mut i = 0;
    while i < bytes.len() {
        if let Some(needle) = ROLE_NEEDLES.iter().find(|n| {
            lower_bytes
                .get(i..)
                .is_some_and(|t| t.starts_with(n.as_bytes()))
        }) {
            // Needles end in ':'; emit the (originally-cased) label, then " :".
            let colon = i + needle.len() - 1;
            if let Some(label) = s.get(i..colon) {
                out.push_str(label);
            }
            out.push(' ');
            out.push(':');
            i = colon + 1;
            continue;
        }
        // Decode from the original to handle multibyte separators (NEL/LS/PS).
        let Some(ch) = s.get(i..).and_then(|t| t.chars().next()) else {
            break;
        };
        let mapped = if matches!(
            ch,
            '\r' | '\n' | '\u{0085}' | '\u{000B}' | '\u{000C}' | '\u{2028}' | '\u{2029}'
        ) {
            ' '
        } else {
            ch
        };
        out.push(mapped);
        i += ch.len_utf8();
    }
    out
}

const CLASSIFIER_TURN_MAX_LEN: usize = distill_workspace::permission::CLASSIFIER_TURN_MAX_LEN;
const LEGACY_TOOL_IMAGE_FOLLOWUP: &str = "[Image extracted from tool result above]";

/// Project the complete resident conversation, retaining only trusted user intent and assistant tool calls.
/// Every projected field is neutralized and capped.
pub(crate) fn build_classifier_turns(
    items: &[ConversationItem],
) -> Vec<distill_workspace::permission::ClassifierTurn> {
    use distill_workspace::permission::ClassifierTurn;
    let mut turns = Vec::new();
    for item in items {
        match item {
            ConversationItem::User(user) => {
                let item_text = item.text_content();
                // Older sessions persisted tool-extracted images as untagged user items.
                let is_legacy_tool_image = item_text == LEGACY_TOOL_IMAGE_FOLLOWUP
                    && user
                        .content
                        .iter()
                        .any(|part| matches!(part, ContentPart::Image { .. }));
                let text = if user.synthetic_reason == SyntheticReason::Interjection {
                    item_text
                } else if distill_chat_state::compaction_utils::is_real_user_turn(item)
                    && !is_project_instructions(item)
                    && !is_legacy_tool_image
                {
                    distill_chat_state::compaction_utils::extract_user_query(&item_text)
                } else {
                    continue;
                };
                if !text.is_empty() {
                    let text = neutralize_transcript_user_text(&text);
                    let text = distill_tools::util::truncate_str_with_marker(
                        &text,
                        CLASSIFIER_TURN_MAX_LEN,
                    )
                    .into_owned();
                    turns.push(ClassifierTurn::UserText(text));
                }
            }
            ConversationItem::Assistant(assistant) => {
                for tc in &assistant.tool_calls {
                    let args = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|_| tc.arguments.to_string());
                    let args = neutralize_transcript_user_text(&args);
                    let args = distill_tools::util::truncate_str_with_marker(
                        &args,
                        CLASSIFIER_TURN_MAX_LEN,
                    )
                    .into_owned();
                    let tool = neutralize_transcript_user_text(&tc.name);
                    let tool = distill_tools::util::truncate_str_with_marker(
                        &tool,
                        CLASSIFIER_TURN_MAX_LEN,
                    )
                    .into_owned();
                    turns.push(ClassifierTurn::AssistantToolUse { tool, args });
                }
            }
            _ => {}
        }
    }
    turns
}

pub(crate) fn refresh_classifier_transcript(
    permissions: &PermissionHandle,
    items: &[ConversationItem],
) {
    permissions.set_classifier_transcript(build_classifier_turns(items));
}

/// Raw AGENTS.md body for the auto-mode classifier's project-instructions.
/// It is the reminder the main agent sees with the `<system-reminder>` wrapper stripped, which is main-agent framing the classifier does not need.
pub(crate) fn agents_md_classifier_body(reminder: &str) -> String {
    reminder
        .trim()
        .trim_start_matches("<system-reminder>")
        .trim_end_matches("</system-reminder>")
        .trim()
        .to_string()
}

/// Whether a session should push AGENTS.md project-instructions to its permission actor's classifier.
/// Only a top-level session that owns its manager and has a non-empty AGENTS.md section should.
/// Re-setting from a subagent would clobber the shared slot with no restore path.
pub(crate) fn should_set_classifier_project_instructions(
    owns_permission_manager: bool,
    section: Option<&str>,
) -> bool {
    owns_permission_manager && section.is_some()
}

/// Returns the earliest of three candidate start indices, so every invariant is satisfied simultaneously.
/// Same idea for assistant context: a short final assistant turn ("ok done") is meaningless without the earlier replies that built up to it.
/// An assistant text turn excludes assistant items whose `.content` is empty (tool-call-only routing turns with no prose).
pub(crate) fn laziness_window_start(
    items: &[ConversationItem],
    item_limit: usize,
    min_user_turns: usize,
    min_assistant_turns: usize,
) -> usize {
    let tail_start = items.len().saturating_sub(item_limit);

    // Walk in reverse, tracking when each minimum is satisfied.
    // `nth_user_idx` becomes Some when we've seen `min_user_turns` real user prompts; same for assistant
    let mut user_seen = 0usize;
    let mut assistant_seen = 0usize;
    let mut nth_user_idx: Option<usize> = None;
    let mut nth_assistant_idx: Option<usize> = None;
    for (idx, item) in items.iter().enumerate().rev() {
        match item {
            ConversationItem::User(u) if u.synthetic_reason.is_human() => {
                user_seen += 1;
                if user_seen == min_user_turns.max(1) && nth_user_idx.is_none() {
                    nth_user_idx = Some(idx);
                }
            }
            ConversationItem::Assistant(a) if !a.content.is_empty() => {
                assistant_seen += 1;
                if assistant_seen == min_assistant_turns.max(1) && nth_assistant_idx.is_none() {
                    nth_assistant_idx = Some(idx);
                }
            }
            _ => {}
        }
        if (min_user_turns == 0 || nth_user_idx.is_some())
            && (min_assistant_turns == 0 || nth_assistant_idx.is_some())
            && idx <= tail_start
        {
            break;
        }
    }

    [Some(tail_start), nth_user_idx, nth_assistant_idx]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(0)
}

/// Strictly-typed classifier output.
/// `category` deserializes via the closed `LazinessCategory` enum, so an unknown string is a parse failure, not a silent fallback.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub(crate) struct ClassifierOutput {
    pub(crate) category: crate::session::events::LazinessCategory,
    pub(crate) confidence: f32,
    pub(crate) evidence: String,
}

impl std::fmt::Display for ClassifierParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparseable => f.write_str("classifier output not parseable as JSON"),
            Self::ConfidenceOutOfRange(c) => write!(f, "confidence {c} outside [0.0, 1.0]"),
        }
    }
}

impl std::error::Error for ClassifierParseError {}

/// Strip a leading `\`\`\`json` or `\`\`\`` fence and the matching trailing fence.
/// Returns `None` if no fence is found. Whitespace surrounding the fences is permitted.
fn strip_code_fence(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))?;
    let body = body.trim_start_matches(['\n', '\r']);
    body.trim_end()
        .strip_suffix("```")
        .map(|s| s.trim_end_matches(['\n', '\r']))
}

/// Scan for the first balanced `{...}` object via a brace counter, handling nested objects and stopping at the matching closing brace.
/// Returns `None` if no balanced object is found.
/// Honors basic string-literal escaping so an unbalanced `{` inside `evidence` doesn't fool the counter.
fn extract_first_balanced_object(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut escape = false;
    for (offset, &b) in bytes.get(start..)?.iter().enumerate() {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + 1;
                    return raw.get(start..end);
                }
            }
            _ => {}
        }
    }
    None
}

/// A pass that parses but yields an out-of-range value does not short-circuit; later passes still get a chance to find a valid object.
/// `Err(ConfidenceOutOfRange)` is returned only when every pass that produced JSON had bad confidence.
/// NaN is implicitly rejected because `(0.0..=1.0).contains(&NaN) == false`.
pub(crate) fn parse_classifier_output(raw: &str) -> Result<ClassifierOutput, ClassifierParseError> {
    // Per-pass outcome: Some(Ok) is a valid object, Some(Err) parsed JSON with confidence out of range, None didn't parse
    fn try_parse(slice: &str) -> Option<Result<ClassifierOutput, f32>> {
        let parsed: ClassifierOutput = serde_json::from_str(slice).ok()?;
        if (0.0..=1.0).contains(&parsed.confidence) {
            Some(Ok(parsed))
        } else {
            Some(Err(parsed.confidence))
        }
    }
    // The first bad-confidence sighting wins the diagnostic
    // Later passes that also fail with bad confidence don't overwrite it, so the caller's log mentions the value the model most plainly produced
    let mut out_of_range: Option<f32> = None;
    let mut accept = |attempt: Option<Result<ClassifierOutput, f32>>| match attempt {
        Some(Ok(parsed)) => Some(parsed),
        Some(Err(bad)) => {
            if out_of_range.is_none() {
                out_of_range = Some(bad);
            }
            None
        }
        None => None,
    };
    let attempts = [
        try_parse(raw),
        strip_code_fence(raw).and_then(try_parse),
        extract_first_balanced_object(raw).and_then(try_parse),
    ];
    for attempt in attempts {
        if let Some(parsed) = accept(attempt) {
            return Ok(parsed);
        }
    }
    if let Some(bad) = out_of_range {
        return Err(ClassifierParseError::ConfidenceOutOfRange(bad));
    }
    Err(ClassifierParseError::Unparseable)
}

/// Observation-only mode (`enabled = true, max_nudges_per_session = 0`) therefore genuinely fires the classifier and emits `LazinessClassifierFired`.
/// Takes `parsed` by reference and clones `evidence` only on the `Nudge` path.
/// The NoNudge path is ~99% of fires (healthy turns returning `not_stalled_*`), so the `String` clone is paid only when a nudge actually fires.
pub(crate) fn evaluate_laziness(
    parsed: &ClassifierOutput,
    cfg: &crate::agent::config::LazinessDetectorPerModelConfig,
    nudges_used_this_session: u32,
    default_min_confidence: f32,
) -> LazinessDecision {
    let category = parsed.category;
    let confidence = parsed.confidence;
    if !cfg.enabled {
        return LazinessDecision::NoNudge {
            category,
            confidence,
            reason: NoNudgeReason::FeatureDisabled,
        };
    }
    if !category.is_stalled() {
        return LazinessDecision::NoNudge {
            category,
            confidence,
            reason: NoNudgeReason::NotStalled,
        };
    }
    let min_conf = cfg.min_confidence.unwrap_or(default_min_confidence);
    if confidence < min_conf {
        return LazinessDecision::NoNudge {
            category,
            confidence,
            reason: NoNudgeReason::LowConfidence,
        };
    }
    if nudges_used_this_session >= cfg.max_nudges_per_session {
        return LazinessDecision::NoNudge {
            category,
            confidence,
            reason: NoNudgeReason::CapExhausted,
        };
    }
    LazinessDecision::Nudge {
        category,
        confidence,
        evidence: parsed.evidence.clone(),
    }
}

/// Build the category-specific nudge text injected as a `<system-reminder>`.
/// Each variant quotes the relevant `<task_completion_discipline>` rule by name.
/// The trailing `evidence` sentence is the classifier's own one-liner.
pub(crate) fn build_laziness_nudge(
    category: crate::session::events::LazinessCategory,
    evidence: &str,
    todo_tool: Option<&str>,
) -> String {
    use crate::session::events::LazinessCategory as L;
    let rule = match category {
        L::StalledNarration => {
            "Per <task_completion_discipline> Rule 1, don't narrate progress in prose without \
             a corresponding tool call. Make the next concrete tool call this turn or mark the \
             affected todo cancelled with a reason."
        }
        L::StalledPermissionAsking => {
            "Per <task_completion_discipline> Rule 2, don't ask permission to continue a task \
             that is in flight. Resume work in your next turn — only pause for genuine \
             ambiguity that changes the approach."
        }
        L::StalledNoTodosButTaskInFlight => {
            let tool = todo_tool.unwrap_or("plan/todo");
            return format!(
                "Idle-stall detector flagged this session: {evidence}\n\n\
                 Per <task_completion_discipline> Rule 3, a multi-step task is clearly in flight \
                 — make the next concrete tool call now. A {tool} list of the remaining phases \
                 can help you keep track, but the priority is to resume the work this turn."
            );
        }
        L::StalledFalseCompletion => {
            "Per <task_completion_discipline>, you declared completion but evidence is missing \
             in the transcript. Either run the tool_calls that back your claims, or correct the \
             claim and continue the actual work."
        }
        // Defensive: only the stalled_* variants reach this function via `evaluate_laziness`
        // The exhaustive match keeps the compiler honest if `is_stalled` gains a variant
        L::NotStalledComplete | L::NotStalledWaitingOnBackground | L::NotStalledWaitingOnUser => {
            return String::new();
        }
    };
    format!("Idle-stall detector flagged this session: {evidence}\n\n{rule}")
}
