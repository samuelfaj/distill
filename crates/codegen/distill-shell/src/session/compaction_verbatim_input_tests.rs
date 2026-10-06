// Modified for Distill by Samuel Fajreldines, 2026.
use super::{
    CompactInputStage, SUMMARY_BUDGET_RESERVE_TOKENS, cold_digest_applies,
    compaction_fallback_model, fitted_input_budget, p3_recorte_applies, split_compaction_prompt,
    verbatim_start_stage,
};
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
    assert_eq!(verbatim_start_stage(&turns, 0, 500_000), CompactInputStage::Verbatim);
}

#[test]
fn start_verbatim_fits_when_estimate_exceeds_budget() {
    let turns = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("x".repeat(200_000)),
    ];
    assert_eq!(
        verbatim_start_stage(&turns, 0, 50_000),
        CompactInputStage::VerbatimFitted
    );
}

/// P3 drops middle segments: on the warm, cache-aligned verbatim input that
/// re-bills every later cached token, and a two-pass summary never reads the
/// input, so the Jev call would be wasted. It runs only on a cold input.
#[test]
fn p3_recorte_runs_only_on_a_cold_input_that_is_sampled() {
    assert!(!p3_recorte_applies(CompactInputStage::Verbatim, false));
    assert!(!p3_recorte_applies(CompactInputStage::VerbatimFitted, true));
    assert!(p3_recorte_applies(CompactInputStage::VerbatimFitted, false));
    assert!(p3_recorte_applies(CompactInputStage::Lossy, false));
}

/// A rejected compaction retries on the configured compaction model, else the
/// worker; never on the main model again, and not at all when neither is set
/// (today's failure stands).
#[test]
fn compaction_fallback_prefers_policy_then_worker_never_main() {
    let some = |s: &str| Some(s.to_owned());
    assert_eq!(compaction_fallback_model(some("cheap"), some("worker"), "main"), some("cheap"));
    assert_eq!(compaction_fallback_model(None, some("worker"), "main"), some("worker"));
    assert_eq!(compaction_fallback_model(some("main"), some("worker"), "main"), some("worker"));
    assert_eq!(compaction_fallback_model(None, some("main"), "main"), None);
    assert_eq!(compaction_fallback_model(Some(" ".into()), None, "main"), None);
    assert_eq!(compaction_fallback_model(None, None, "main"), None);
}

/// A prefired two-pass summary is used without reading the compaction input,
/// so digesting that input first would spend up to four utility selections,
/// and minutes of a blocking compaction, on nothing.
#[test]
fn the_cold_digest_waits_until_the_input_will_be_sampled() {
    assert!(cold_digest_applies(CompactInputStage::VerbatimFitted, false));
    assert!(!cold_digest_applies(CompactInputStage::VerbatimFitted, true));
    assert!(!cold_digest_applies(CompactInputStage::Verbatim, false));
    assert!(!cold_digest_applies(CompactInputStage::Lossy, false));
}

/// Pass 1 ends with the compaction prompt as a user item. Left in place it
/// would be the "newest human turn" of the cold digest, and the work in
/// progress would be digested; split off, that work stays verbatim.
#[test]
fn the_pass1_prompt_never_bounds_the_cold_digest() {
    let current = "test output line\n".repeat(400);
    let prefix = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("run the tests"),
        ConversationItem::assistant_tool_calls(vec![distill_sampling_types::ToolCall {
            id: "cur".into(),
            name: "run_terminal_command".to_owned(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        }]),
        ConversationItem::tool_result("cur", current),
    ];
    let pass1 = crate::session::two_pass::build_two_pass_pass1_history(&prefix, "Summarize.");
    assert_eq!(distill_chat_state::cold_compaction_candidates(&pass1).len(), 1, "the trap");
    let (history, prompt) = split_compaction_prompt(pass1);
    assert!(distill_chat_state::cold_compaction_candidates(&history).is_empty());
    assert_eq!(history.len(), prefix.len());
    assert!(prompt.is_some_and(|item| item.text_content() == "Summarize."));
}
