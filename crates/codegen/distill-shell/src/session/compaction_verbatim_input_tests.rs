// Modified for Distill by Samuel Fajreldines, 2026.
use super::{
    CompactInputStage, SUMMARY_BUDGET_RESERVE_TOKENS, fitted_input_budget,
    start_verbatim_compact_turns,
};
use distill_chat_state::estimate_conversation_tokens;
use distill_sampling_types::ConversationItem;

#[test]
fn fitted_input_budget_subtracts_reserve_and_tools() {
    assert_eq!(
        fitted_input_budget(100_000, 10_000),
        100_000 - SUMMARY_BUDGET_RESERVE_TOKENS - 10_000
    );
}

#[test]
fn start_verbatim_stays_verbatim_when_estimate_fits() {
    let turns = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("hello"),
        ConversationItem::assistant("hi"),
    ];
    let (out, stage) = start_verbatim_compact_turns(turns.clone(), 0, 500_000);
    assert_eq!(stage, CompactInputStage::Verbatim);
    assert_eq!(out.len(), turns.len());
    assert_eq!(
        estimate_conversation_tokens(&out),
        estimate_conversation_tokens(&turns)
    );
}

#[test]
fn start_verbatim_fits_when_estimate_exceeds_budget() {
    let turns = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("x".repeat(200_000)),
    ];
    let window = 50_000;
    let budget = fitted_input_budget(window, 0);
    let (out, stage) = start_verbatim_compact_turns(turns, 0, window);
    assert_eq!(stage, CompactInputStage::VerbatimFitted);
    assert!(estimate_conversation_tokens(&out) <= budget);
}
