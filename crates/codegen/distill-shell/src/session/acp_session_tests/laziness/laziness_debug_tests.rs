// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for the `--laziness-debug-log` prototype: `classify_debug_decision`, the JSONL line shape, and the file append.
//! The debug branch inside `maybe_fire_laziness_check` needs a live sampler responder, so it is out of scope here.
//! `laziness_integration_tests` covers it indirectly by driving the production path with the dev flag off.
use super::{
    ClassifierOutput, DebugClassifierOutput, DebugDecision, DebugTodoSnapshot,
    LazinessDebugLogLine, LazinessFireMeta, LazinessFireOutcome, LazinessSuppressReason,
    append_laziness_debug_log_line, build_laziness_debug_line, classify_debug_decision,
    flatten_transcript_for_classifier,
};
use crate::session::events::{LAZINESS_ABORT_USER_INPUT, LazinessCategory};
use distill_sampling_types::{
    AssistantItem, ContentPart, ConversationItem, SyntheticReason, ToolCall, ToolResultItem,
    UserItem,
};

fn user_text(text: &str) -> ConversationItem {
    ConversationItem::User(UserItem {
        content: vec![ContentPart::Text { text: text.into() }],
        synthetic_reason: SyntheticReason::Human,
        ..Default::default()
    })
}

fn assistant_text(text: &str) -> ConversationItem {
    ConversationItem::Assistant(AssistantItem {
        content: text.into(),
        tool_calls: vec![],
        model_id: None,
        model_fingerprint: None,
        reasoning_effort: None,
    })
}

fn assistant_with_tool_call(text: &str, name: &str, args: &str) -> ConversationItem {
    ConversationItem::Assistant(AssistantItem {
        content: text.into(),
        tool_calls: vec![ToolCall {
            id: "call-1".into(),
            name: name.to_string(),
            arguments: args.into(),
        }],
        model_id: None,
        model_fingerprint: None,
        reasoning_effort: None,
    })
}

/// Build an `AssistantItem` with arbitrary `reasoning`, `content`, and `tool_calls` for the `[assistant reasoning]` test coverage.
/// Trivially-defaulted fields (`raw_output`, `model_id`, `model_fingerprint`) are filled with `None` so each test stays a one-liner.
/// When `reasoning_text` is empty, no Reasoning item is emitted (callers who want an encrypted-only sibling should build that variant inline).
fn assistant_with_reasoning_items(
    reasoning_text: &str,
    content: &str,
    tool_calls: Vec<ToolCall>,
) -> Vec<ConversationItem> {
    let mut out = Vec::new();
    if !reasoning_text.is_empty() {
        out.push(ConversationItem::Reasoning(
            distill_sampling_types::rs::ReasoningItem {
                id: String::new(),
                summary: vec![distill_sampling_types::rs::SummaryPart::SummaryText(
                    distill_sampling_types::rs::SummaryTextContent {
                        text: reasoning_text.to_string(),
                    },
                )],
                content: None,
                encrypted_content: None,
                status: None,
            },
        ));
    }
    out.push(ConversationItem::Assistant(AssistantItem {
        content: content.into(),
        tool_calls,
        model_id: None,
        model_fingerprint: None,
        reasoning_effort: None,
    }));
    out
}

#[test]
fn flatten_renders_roles_in_order_without_synthesising_an_assistant_turn() {
    // Regression guard: the classifier once saw raw `ConversationItem::Assistant` items and continued the conversation instead of classifying
    // The flattener MUST emit every assistant message as a `[assistant]` text line, never a structured assistant turn
    // That keeps assistant items out of the request that wraps the output, so the model has nothing to latch onto
    let items = vec![
        user_text("hello"),
        assistant_text("done"),
        user_text("more?"),
    ];
    let out = flatten_transcript_for_classifier(&items, true);
    assert_eq!(
        out, "[user] hello\n[assistant] done\n[user] more?\n",
        "transcript should be plain `[role] text` lines in order",
    );
}

#[test]
fn flatten_marks_agent_message_as_untrusted_not_human() {
    let items = vec![ConversationItem::agent_message("review this change")];
    let out = flatten_transcript_for_classifier(&items, true);
    assert_eq!(
        out,
        format!(
            "[agent_message] {} review this change\n",
            distill_chat_state::compaction_utils::AGENT_MESSAGE_MODEL_LABEL
        )
    );
}

#[test]
fn flatten_renders_tool_calls_as_lines() {
    let items = vec![assistant_with_tool_call(
        "checking",
        "read_file",
        "{\"path\":\"x\"}",
    )];
    let out = flatten_transcript_for_classifier(&items, true);
    assert!(
        out.contains("[assistant] checking"),
        "assistant text rendered: {out}",
    );
    assert!(
        out.contains("[assistant tool_call] read_file({\"path\":\"x\"})"),
        "tool call rendered: {out}",
    );
}

#[test]
fn flatten_truncates_long_fields() {
    let long = "a".repeat(2_000);
    let items = vec![ConversationItem::ToolResult(ToolResultItem {
        tool_call_id: "call-1".to_string(),
        content: long.into(),
        images: vec![],
    })];
    let out = flatten_transcript_for_classifier(&items, true);
    assert!(
        out.contains("…[truncated]"),
        "long content truncated: {out}"
    );
    assert!(
        out.len() < 800,
        "truncation cap respected: {} chars",
        out.len()
    );
}

#[test]
fn flatten_collapses_newlines_to_keep_one_line_per_item() {
    let items = vec![user_text("line1\nline2\nline3")];
    let out = flatten_transcript_for_classifier(&items, true);
    // Exactly one `\n` ends the user line; internal newlines collapse to the U+23CE arrow
    assert_eq!(out, "[user] line1 ⏎ line2 ⏎ line3\n");
}

#[test]
fn flatten_handles_system_items() {
    let items = vec![ConversationItem::system("remember X")];
    let out = flatten_transcript_for_classifier(&items, true);
    assert_eq!(out, "[system] remember X\n");
}

#[test]
fn flatten_renders_empty_input_as_empty_string() {
    let items: Vec<ConversationItem> = vec![];
    assert_eq!(flatten_transcript_for_classifier(&items, true), "");
}

#[test]
fn flatten_renders_assistant_reasoning() {
    // Plain-text reasoning becomes a `[assistant reasoning]` line so the classifier can use chain-of-thought as a signal
    // An agent that reasoned about running tests but never called the tool is still a stall
    let items = assistant_with_reasoning_items("I should run the tests now.", "", vec![]);
    let out = flatten_transcript_for_classifier(&items, true);
    assert_eq!(
        out, "[assistant reasoning] I should run the tests now.\n",
        "reasoning text rendered as its own line: {out}",
    );
}

#[test]
fn flatten_skips_reasoning_when_encrypted_only() {
    // Encrypted reasoning is opaque to a text classifier, so drop it rather than emit a meaningless line
    let items = vec![
        ConversationItem::Reasoning(distill_sampling_types::rs::ReasoningItem {
            id: String::new(),
            summary: vec![],
            content: None,
            encrypted_content: Some("opaque_base64".into()),
            status: None,
        }),
        ConversationItem::Assistant(AssistantItem {
            content: "ok".into(),
            tool_calls: vec![],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];
    let out = flatten_transcript_for_classifier(&items, true);
    assert!(
        !out.contains("[assistant reasoning]"),
        "encrypted-only reasoning must NOT produce a line: {out}",
    );
    assert_eq!(out, "[assistant] ok\n");
}

#[test]
fn flatten_skips_reasoning_when_text_is_empty() {
    // Empty-string reasoning is treated as no reasoning; a line with no content would only waste tokens
    let items = vec![
        ConversationItem::Reasoning(distill_sampling_types::rs::ReasoningItem {
            id: String::new(),
            summary: vec![distill_sampling_types::rs::SummaryPart::SummaryText(
                distill_sampling_types::rs::SummaryTextContent {
                    text: String::new(),
                },
            )],
            content: None,
            encrypted_content: None,
            status: None,
        }),
        ConversationItem::Assistant(AssistantItem {
            content: "ok".into(),
            tool_calls: vec![],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];
    let out = flatten_transcript_for_classifier(&items, true);
    assert!(
        !out.contains("[assistant reasoning]"),
        "empty reasoning text must NOT produce a line: {out}",
    );
    assert_eq!(out, "[assistant] ok\n");
}

#[test]
fn flatten_skips_reasoning_when_text_is_whitespace_only() {
    // Whitespace-only reasoning (spaces, tabs, newlines) carries zero signal
    let items = assistant_with_reasoning_items("   \n\t  \n  ", "ok", vec![]);
    let out = flatten_transcript_for_classifier(&items, true);
    assert!(
        !out.contains("[assistant reasoning]"),
        "whitespace-only reasoning must NOT produce a line: {out}",
    );
    assert_eq!(out, "[assistant] ok\n");
}

#[test]
fn flatten_orders_reasoning_before_content_and_tools() {
    // Chronological order matches how the agent actually produced the turn: reason first, then write visible output, then call tools
    // The classifier reads top to bottom, so the line order is part of the format
    let items = assistant_with_reasoning_items(
        "I should read the file first",
        "let me check",
        vec![ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"x\"}".into(),
        }],
    );
    let out = flatten_transcript_for_classifier(&items, true);
    assert_eq!(
        out,
        "[assistant reasoning] I should read the file first\n\
             [assistant] let me check\n\
             [assistant tool_call] read_file({\"path\":\"x\"})\n",
        "lines emitted in chronological order: reasoning → content → tool_calls",
    );
}

#[test]
fn flatten_truncates_long_reasoning_text() {
    // Reasoning uses a tighter 200-char cap than the 400-char cap applied to other line types
    let long = "r".repeat(2_000);
    let items = assistant_with_reasoning_items(&long, "", vec![]);
    let out = flatten_transcript_for_classifier(&items, true);
    assert!(
        out.starts_with("[assistant reasoning] "),
        "reasoning line is emitted: {out}",
    );
    // Pin the *content* of the truncated prefix, not just the sentinel
    // A bug that truncated to 0 chars, or replaced the body with the `…[truncated]` sentinel alone, would still pass a "contains the sentinel" check
    assert!(
        out.contains(&"r".repeat(200)),
        "first 200 chars of reasoning preserved in output: {out}",
    );
    assert!(
        !out.contains(&"r".repeat(201)),
        "truncation cap is exactly 200, not larger: {out}",
    );
    assert!(
        out.contains("…[truncated]"),
        "long reasoning text is truncated: {out}",
    );
    assert!(
        out.len() < 400,
        "truncation cap respected: {} chars",
        out.len(),
    );
}

#[test]
fn flatten_drops_reasoning_when_include_reasoning_is_false() {
    // The per-model or CLI override path: when `include_reasoning` is `false`, even a non-empty reasoning.text is dropped
    // The assistant content and tool calls are unaffected
    let items = assistant_with_reasoning_items(
        "plan: read first",
        "ok",
        vec![ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"x\"}".into(),
        }],
    );
    let out = flatten_transcript_for_classifier(&items, false);
    assert!(
        !out.contains("[assistant reasoning]"),
        "include_reasoning=false suppresses the reasoning line: {out}",
    );
    assert_eq!(
        out, "[assistant] ok\n[assistant tool_call] read_file({\"path\":\"x\"})\n",
        "content + tool_call lines are untouched: {out}",
    );
}

#[test]
fn flatten_keeps_reasoning_when_include_reasoning_is_true() {
    // Sibling of `flatten_drops_reasoning_when_include_reasoning_is_false`: same item, opposite flag, so the reasoning line IS emitted
    let items = assistant_with_reasoning_items(
        "plan: read first",
        "ok",
        vec![ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"x\"}".into(),
        }],
    );
    let out = flatten_transcript_for_classifier(&items, true);
    assert_eq!(
        out,
        "[assistant reasoning] plan: read first\n\
             [assistant] ok\n\
             [assistant tool_call] read_file({\"path\":\"x\"})\n",
        "include_reasoning=true emits all three lines in order: {out}",
    );
}

fn synthetic_user_text(
    text: &str,
    reason: distill_sampling_types::SyntheticReason,
) -> ConversationItem {
    ConversationItem::User(UserItem {
        content: vec![ContentPart::Text { text: text.into() }],
        synthetic_reason: reason,
        ..Default::default()
    })
}

// ── laziness_window_start coverage ────────────────────────────

#[test]
fn window_keeps_last_user_prompt_even_when_30_tool_calls_follow_it() {
    // Regression for the original "tool-call burst eats the user prompt" bug
    // With min_user_turns=1, the window must extend back to capture that prompt even when the last 30 items don't include it
    let mut items = vec![user_text("write a Rust function that sorts a list")];
    for _ in 0..40 {
        items.push(assistant_with_tool_call("checking", "read_file", "{}"));
    }
    let start = super::laziness_window_start(&items, 30, 1, 1);
    assert_eq!(start, 0, "user prompt at idx 0 must be retained");
}

#[test]
fn window_pins_min_user_turns_user_prompts_into_view() {
    // Five user prompts each separated by 10 tool calls
    // With min_user_turns=3 the window must extend back to the 3rd-from-last user prompt
    // That lets a short final reply like "yes" be interpreted against the prior exchange
    let mut items: Vec<ConversationItem> = Vec::new();
    for i in 0..5 {
        items.push(user_text(&format!("prompt {i}")));
        for _ in 0..10 {
            items.push(assistant_with_tool_call("step", "read_file", "{}"));
        }
    }
    // Layout: U(0).
    // With min_user_turns=3, the 3rd-from-last user idx = U(22) at idx 22 tail_start = 55 - 30 = 25.
    // Window must start at min(25, 22) = 22.
    let start = super::laziness_window_start(&items, 30, 3, 0);
    assert_eq!(start, 22);
}

#[test]
fn window_pins_min_assistant_turns_assistant_replies_into_view() {
    // Symmetric to the user-pin test: a "yes" final user reply is meaningless without seeing the assistant's prior suggestion
    // When tool calls dominate the tail, min_assistant_turns must pull older assistant text turns into the window
    let mut items: Vec<ConversationItem> = Vec::new();
    for i in 0..6 {
        items.push(assistant_text(&format!("reply {i}")));
        for _ in 0..6 {
            items.push(assistant_with_tool_call("step", "read_file", "{}"));
        }
    }
    // Layout: AT(0) AC×6 AT(7) AC×6 AT(14) AC×6 AT(21) AC×6 AT(28) AC×6 AT(35) AC×6 (AT = assistant text turn, AC = assistant-with-tool-call, whose non-empty content "step" also counts it as an assistant text turn.).
    // Every assistant item is an eligible assistant text turn, so there are 42 3rd-from-last assistant-text turn idx = 42 - 3 = 39.
    // Window must start at min(12, 39) = 12.
    let start = super::laziness_window_start(&items, 30, 0, 3);
    assert_eq!(start, 12);
}

#[test]
fn window_takes_earliest_of_user_pin_and_assistant_pin_and_tail() {
    // Realistic combined case: the chat mixes user prompts and tool calls, and both minimums demand earlier indices than the 30-item tail
    // The window picks the EARLIEST so both invariants are satisfied
    let mut items: Vec<ConversationItem> = Vec::new();
    for i in 0..3 {
        items.push(user_text(&format!("u{i}")));
        for _ in 0..15 {
            items.push(assistant_with_tool_call("x", "read_file", "{}"));
        }
    }
    // Layout: U(0) AC×15 U(16) AC×15 U(32) AC×15, total 48.
    // For min_user_turns=2: user idxs = [0, 16, 32]; 2nd-from-last = idx 16.
    // For min_assistant_turns=10: assistant_text idxs are every AC (45 of them); 10th-from-last = idx 47 - 9 = 38 tail_start = 48 - 30 = 18.
    let start = super::laziness_window_start(&items, 30, 2, 10);
    assert_eq!(start, 16);
}

#[test]
fn window_relaxes_minimums_when_chat_lacks_enough_turns() {
    // A chat with only 1 user prompt and 2 assistant turns must not panic or pad; the window just starts at 0
    let items = vec![
        user_text("only prompt"),
        assistant_text("first reply"),
        assistant_text("second reply"),
    ];
    let start = super::laziness_window_start(&items, 30, 5, 5);
    assert_eq!(start, 0);
}

#[test]
fn window_ignores_synthetic_user_items_when_pinning() {
    // SystemReminder and AutoContinue user items are synthesised by the runtime, not typed by the user
    // They MUST NOT count toward `min_user_turns`.
    use distill_sampling_types::SyntheticReason;
    let mut items = vec![user_text("real user prompt")]; // idx 0
    for _ in 0..29 {
        items.push(assistant_text("tool work"));
    }
    items.push(synthetic_user_text(
        "<system-reminder>...",
        SyntheticReason::SystemReminder,
    ));
    for _ in 0..5 {
        items.push(assistant_text("more"));
    }
    // Real user prompts = [idx 0]. With min_user_turns=1, nth_user_idx = 0.
    // tail_start = 36 - 30 = 6.
    // Window must start at 0.
    let start = super::laziness_window_start(&items, 30, 1, 0);
    assert_eq!(start, 0);
}

#[test]
fn window_falls_back_to_tail_when_no_real_user_prompt_present() {
    // With no real user items at all, the user pin is None, so the window falls back to the plain 30-item tail (and the assistant pin if applicable)
    use distill_sampling_types::SyntheticReason;
    let mut items: Vec<ConversationItem> = Vec::new();
    for _ in 0..40 {
        items.push(assistant_text("solo"));
    }
    items.push(synthetic_user_text(
        "<system-reminder>",
        SyntheticReason::SystemReminder,
    ));
    // With min_assistant_turns=3, the 3rd-from-last assistant-text idx = 40 - 3 = 37
    // tail_start = 41 - 30 = 11.
    // Earliest = 11.
    let start = super::laziness_window_start(&items, 30, 5, 3);
    assert_eq!(start, 11);
}

#[test]
fn window_short_session_returns_zero() {
    // With fewer items than the limit, the window starts at 0
    let items = vec![user_text("hi"), assistant_text("hello")];
    assert_eq!(super::laziness_window_start(&items, 30, 5, 5), 0);
}

#[test]
fn window_assistant_text_pin_skips_empty_assistant_turns() {
    // Assistant items with empty `.content` (tool-call-only routing turns) MUST NOT count toward min_assistant_turns
    // They have no prose for the classifier to interpret
    let empty_asst = ConversationItem::Assistant(distill_sampling_types::AssistantItem {
        content: String::new().into(),
        tool_calls: vec![distill_sampling_types::ToolCall {
            id: "c".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }],
        model_id: None,
        model_fingerprint: None,
        reasoning_effort: None,
    });
    // 5 real text turns at idxs 0..5, then 10 empty turns.
    let mut items: Vec<ConversationItem> =
        (0..5).map(|i| assistant_text(&format!("t{i}"))).collect();
    for _ in 0..10 {
        items.push(empty_asst.clone());
    }
    // tail_start = 15 - 30 = 0 (saturating).
    // With min_assistant_turns=3, the 3rd-from-last assistant TEXT turn = idx 5 - 3 = 2 (text turns are 0..5 inclusive of 4)
    // Earliest = 0.
    let start = super::laziness_window_start(&items, 30, 0, 3);
    assert_eq!(start, 0);
}

fn parsed(category: LazinessCategory, confidence: f32) -> ClassifierOutput {
    ClassifierOutput {
        category,
        confidence,
        evidence: "ev".to_string(),
    }
}

#[test]
fn classify_debug_decision_would_nudge_for_stalled_above_threshold() {
    let p = parsed(LazinessCategory::StalledNarration, 0.9);
    assert_eq!(classify_debug_decision(&p, 0.7), DebugDecision::WouldNudge);
}

#[test]
fn classify_debug_decision_low_confidence_for_stalled_below_threshold() {
    let p = parsed(LazinessCategory::StalledPermissionAsking, 0.5);
    assert_eq!(
        classify_debug_decision(&p, 0.7),
        DebugDecision::NoNudgeLowConfidence,
    );
}

#[test]
fn classify_debug_decision_not_stalled_irrespective_of_confidence() {
    // A not-stalled verdict stays NoNudgeNotStalled even at high confidence; the threshold only gates stalled_* verdicts
    let p = parsed(LazinessCategory::NotStalledComplete, 0.99);
    assert_eq!(
        classify_debug_decision(&p, 0.7),
        DebugDecision::NoNudgeNotStalled,
    );
}

#[test]
fn classify_debug_decision_all_stalled_variants_route_to_would_nudge() {
    // Drive the loop off `LazinessCategory::all().filter(is_stalled)` so adding a new stalled variant grows this test automatically
    // There is no hand-coded array to drift; the compiler enforces exhaustivity via `LazinessCategory::is_stalled`'s match
    let mut covered = 0usize;
    for &cat in LazinessCategory::all() {
        if !cat.is_stalled() {
            continue;
        }
        covered += 1;
        let p = parsed(cat, 0.85);
        assert_eq!(
            classify_debug_decision(&p, 0.7),
            DebugDecision::WouldNudge,
            "{cat:?} should route to WouldNudge above threshold",
        );
    }
    assert!(
        covered >= 4,
        "expected at least four stalled variants, got {covered}",
    );
}

fn sample_line() -> LazinessDebugLogLine {
    LazinessDebugLogLine {
        timestamp: "2026-05-21T22:14:01.123Z".to_string(),
        session_id: "019e4c65-434b-7d62-9d4b-8137d1d413e4".to_string(),
        model_id: "grok-4.5".to_string(),
        items_sent: 28,
        todo_snapshot: vec![DebugTodoSnapshot {
            id: "turn-finish-test-1".to_string(),
            status: "pending",
        }],
        backing_task_count: 0,
        classifier_raw_output: Some(
            "{\"category\":\"stalled_narration\",\"confidence\":0.87,\"evidence\":\"...\"}"
                .to_string(),
        ),
        parsed: Some(DebugClassifierOutput {
            category: "stalled_narration".to_string(),
            confidence: 0.87,
            evidence: "...".to_string(),
        }),
        decision: DebugDecision::WouldNudge,
        abort_reason: None,
        error_detail: None,
        classifier_elapsed_ms: 1834,
    }
}

#[test]
fn log_line_serializes_to_expected_jsonl_shape() {
    let line = sample_line();
    let json = serde_json::to_string(&line).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("re-parse");
    // Top-level keys present.
    for key in [
        "timestamp",
        "session_id",
        "model_id",
        "items_sent",
        "todo_snapshot",
        "backing_task_count",
        "classifier_raw_output",
        "parsed",
        "decision",
        "abort_reason",
        "classifier_elapsed_ms",
    ] {
        assert!(parsed.get(key).is_some(), "missing key: {key}");
    }
    // `decision` serializes to a fixed snake_case string via serde's rename_all
    // Pinning it catches a typo or rename without forcing a match-arm update across consumers
    assert_eq!(
        parsed
            .pointer("/decision")
            .unwrap_or(&serde_json::Value::Null),
        "would_nudge"
    );
    assert_eq!(
        parsed
            .pointer("/parsed/category")
            .unwrap_or(&serde_json::Value::Null),
        "stalled_narration"
    );
    assert_eq!(
        parsed
            .pointer("/items_sent")
            .unwrap_or(&serde_json::Value::Null),
        28
    );
    assert!(
        parsed
            .pointer("/abort_reason")
            .unwrap_or(&serde_json::Value::Null)
            .is_null()
    );
}

#[test]
fn log_line_aborted_decision_serializes_with_reason() {
    let mut line = sample_line();
    line.decision = DebugDecision::Aborted;
    line.abort_reason = Some(LAZINESS_ABORT_USER_INPUT);
    line.classifier_raw_output = None;
    line.parsed = None;
    let json = serde_json::to_string(&line).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("re-parse");
    assert_eq!(
        parsed
            .pointer("/decision")
            .unwrap_or(&serde_json::Value::Null),
        "aborted"
    );
    assert_eq!(
        parsed
            .pointer("/abort_reason")
            .unwrap_or(&serde_json::Value::Null),
        "user_input"
    );
    assert!(
        parsed
            .pointer("/parsed")
            .unwrap_or(&serde_json::Value::Null)
            .is_null()
    );
    assert!(
        parsed
            .pointer("/classifier_raw_output")
            .unwrap_or(&serde_json::Value::Null)
            .is_null()
    );
}

#[test]
fn build_laziness_debug_line_suppressed_not_goal_mode_includes_parsed_verdict() {
    let meta = LazinessFireMeta {
        session_id: "sess".to_string(),
        todo_snapshot: vec![],
        backing_task_count: 0,
    };
    let parsed = parsed(LazinessCategory::StalledNarration, 0.9);
    let raw_text =
        r#"{"category":"stalled_narration","confidence":0.9,"evidence":"stalled"}"#.to_string();
    let line = build_laziness_debug_line(
        meta,
        "test-model",
        12,
        500,
        LazinessFireOutcome::Suppressed {
            reason: LazinessSuppressReason::NotGoalMode,
            parsed: parsed.clone(),
            raw_text: raw_text.clone(),
        },
    );
    assert_eq!(line.decision, DebugDecision::SuppressedNotGoalMode);
    assert_eq!(
        line.classifier_raw_output.as_deref(),
        Some(raw_text.as_str())
    );
    assert_eq!(
        line.parsed.as_ref().map(|p| p.category.as_str()),
        Some("stalled_narration")
    );
    let json = serde_json::to_string(&line).expect("serialize");
    let v: serde_json::Value = serde_json::from_str(&json).expect("re-parse");
    assert_eq!(
        v.pointer("/decision").unwrap_or(&serde_json::Value::Null),
        "suppressed_not_goal_mode"
    );
    assert_eq!(
        v.pointer("/parsed/category")
            .unwrap_or(&serde_json::Value::Null),
        "stalled_narration"
    );
    assert!(
        v.pointer("/classifier_raw_output")
            .unwrap_or(&serde_json::Value::Null)
            .is_string()
    );
}

/// Write two lines, parse them back from disk, and confirm both round-trip cleanly.
/// Each append must produce its own JSON object on its own `\n`-separated line.
#[tokio::test]
async fn append_writes_two_lines_each_parseable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("debug.jsonl");
    let handle: std::sync::Arc<std::path::Path> = std::sync::Arc::from(path.as_path());

    let line1 = sample_line();
    let mut line2 = sample_line();
    line2.decision = DebugDecision::NoNudgeNotStalled;
    line2.classifier_elapsed_ms = 921;

    append_laziness_debug_log_line(&handle, &line1)
        .await
        .expect("append 1");
    append_laziness_debug_log_line(&handle, &line2)
        .await
        .expect("append 2");

    let contents = std::fs::read_to_string(&path).expect("read");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "expected exactly two newline-separated lines"
    );
    let [line1, line2] = lines.as_slice() else {
        panic!("expected exactly two newline-separated lines: {lines:?}");
    };
    let parsed1: serde_json::Value = serde_json::from_str(line1).expect("parse line 1");
    let parsed2: serde_json::Value = serde_json::from_str(line2).expect("parse line 2");
    assert_eq!(
        parsed1
            .pointer("/decision")
            .unwrap_or(&serde_json::Value::Null),
        "would_nudge"
    );
    assert_eq!(
        parsed2
            .pointer("/decision")
            .unwrap_or(&serde_json::Value::Null),
        "no_nudge_not_stalled"
    );
    assert_eq!(
        parsed2
            .pointer("/classifier_elapsed_ms")
            .unwrap_or(&serde_json::Value::Null),
        921
    );
}
