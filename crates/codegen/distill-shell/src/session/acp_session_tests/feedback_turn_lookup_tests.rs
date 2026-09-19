// Modified for Distill by Samuel Fajreldines, 2026.
use super::{FeedbackTurnLookup, turn_texts_for_feedback};
use distill_sampling_types::{ConversationItem, ReasoningEffort};

fn user_turn(content: &str, prompt_index: usize) -> ConversationItem {
    let mut item = ConversationItem::user(content);
    item.set_prompt_index(prompt_index);
    item
}

fn assistant_with_effort(content: &str, effort: ReasoningEffort) -> ConversationItem {
    let mut item = ConversationItem::assistant(content);
    if let ConversationItem::Assistant(a) = &mut item {
        a.reasoning_effort = Some(effort);
    }
    item
}

fn lookup(
    user: Option<&str>,
    assistant: Option<&str>,
    effort: Option<ReasoningEffort>,
) -> FeedbackTurnLookup {
    FeedbackTurnLookup {
        user_text: user.map(str::to_string),
        assistant_text: assistant.map(str::to_string),
        reasoning_effort: effort,
        ..Default::default()
    }
}

#[test]
fn turn_n_returns_nth_exchange() {
    let conv = vec![
        user_turn("q1", 0),
        ConversationItem::assistant("a1"),
        user_turn("q2", 1),
        ConversationItem::assistant("a2"),
        user_turn("q3", 2),
        ConversationItem::assistant("a3"),
    ];
    assert_eq!(
        turn_texts_for_feedback(&conv, 0),
        lookup(Some("q1"), Some("a1"), None)
    );
    assert_eq!(
        turn_texts_for_feedback(&conv, 1),
        lookup(Some("q2"), Some("a2"), None)
    );
}

#[test]
fn out_of_range_returns_none() {
    let conv = vec![
        user_turn("only q", 0),
        ConversationItem::assistant("only a"),
    ];
    assert_eq!(
        turn_texts_for_feedback(&conv, 5),
        FeedbackTurnLookup::default()
    );
    assert_eq!(
        turn_texts_for_feedback(&[], 0),
        FeedbackTurnLookup::default()
    );
}

#[test]
fn no_assistant_yet_returns_user_only() {
    // q2 has no assistant response yet.
    let conv = vec![
        user_turn("q1", 0),
        ConversationItem::assistant("a1"),
        user_turn("q2", 1),
    ];
    assert_eq!(
        turn_texts_for_feedback(&conv, 1),
        lookup(Some("q2"), None, None)
    );
}

#[test]
fn does_not_bleed_assistant_into_next_turn() {
    // Turn 0 (q1) has no assistant; turn 1 (q2) does
    // The lookup for turn 0 must NOT pick up turn 1's assistant
    let conv = vec![
        user_turn("q1", 0),
        user_turn("q2", 1),
        ConversationItem::assistant("a2"),
    ];
    assert_eq!(
        turn_texts_for_feedback(&conv, 0),
        lookup(Some("q1"), None, None)
    );
}

#[test]
fn skips_whitespace_only_assistant() {
    let conv = vec![
        user_turn("q", 0),
        ConversationItem::assistant("   \n  "),
        ConversationItem::assistant("real answer"),
    ];
    assert_eq!(
        turn_texts_for_feedback(&conv, 0),
        lookup(Some("q"), Some("real answer"), None)
    );
}

/// Per-turn feedback must render the same `*User:*` text in Slack as latest-turn feedback (which goes through `extract_user_query`).
/// Without this stripping the same channel sees raw `<user_query>` blobs from per-turn submissions and clean prose from spontaneous ones.
#[test]
fn strips_user_query_metadata_tags() {
    let raw = "<user_info>internal</user_info><user_query>fix the bug</user_query><project_layout>tree</project_layout>";
    let conv = vec![user_turn(raw, 0), ConversationItem::assistant("on it")];
    assert_eq!(
        turn_texts_for_feedback(&conv, 0),
        lookup(Some("fix the bug"), Some("on it"), None)
    );
}

#[test]
fn rated_turn_echoed_effort_wins_over_later_turn() {
    let conv = vec![
        user_turn("q1", 0),
        assistant_with_effort("a1", ReasoningEffort::High),
        user_turn("q2", 1),
        assistant_with_effort("a2", ReasoningEffort::Max),
    ];
    assert_eq!(
        turn_texts_for_feedback(&conv, 0),
        lookup(Some("q1"), Some("a1"), Some(ReasoningEffort::High))
    );
    assert_eq!(
        turn_texts_for_feedback(&conv, 1),
        lookup(Some("q2"), Some("a2"), Some(ReasoningEffort::Max))
    );
}
