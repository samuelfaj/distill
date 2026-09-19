use super::{
    ClassifierOutput, ClassifierParseError, LAZINESS_DEFAULT_MIN_CONFIDENCE, LazinessDecision,
    NoNudgeReason, build_laziness_nudge, evaluate_laziness, laziness_injection_active,
    parse_classifier_output,
};
use crate::agent::config::LazinessDetectorPerModelConfig;
use crate::session::events::LazinessCategory;

fn cfg_enabled(cap: u32) -> LazinessDetectorPerModelConfig {
    LazinessDetectorPerModelConfig {
        enabled: true,
        max_nudges_per_session: cap,
        idle_threshold_ms: None,
        min_confidence: None,
        include_reasoning: None,
    }
}

// ── JSON parser ─────────────────────────────────────────────────

#[test]
fn parse_classifier_output_clean_json() {
    let raw = r#"{"category":"stalled_narration","confidence":0.92,"evidence":"prose without tool call"}"#;
    let parsed = parse_classifier_output(raw).expect("clean JSON parses");
    assert_eq!(parsed.category, LazinessCategory::StalledNarration);
    assert!((parsed.confidence - 0.92).abs() < 1e-6);
    assert_eq!(parsed.evidence, "prose without tool call");
}

#[test]
fn parse_classifier_output_stalled_false_completion_round_trips() {
    // The prompt schema advertises `stalled_false_completion`
    // The parser must accept it and map it to `LazinessCategory::StalledFalseCompletion`
    let raw = r#"{"category":"stalled_false_completion","confidence":0.88,"evidence":"final message claims make test ran but no tool_call appears."}"#;
    let parsed = parse_classifier_output(raw).expect("stalled_false_completion parses");
    assert_eq!(parsed.category, LazinessCategory::StalledFalseCompletion);
    assert!((parsed.confidence - 0.88).abs() < 1e-6);
}

#[test]
fn parse_classifier_output_fence_wrapped() {
    let raw = "```json\n{\"category\":\"stalled_permission_asking\",\"confidence\":0.8,\"evidence\":\"asks for permission\"}\n```";
    let parsed = parse_classifier_output(raw).expect("fenced JSON parses");
    assert_eq!(parsed.category, LazinessCategory::StalledPermissionAsking);
    assert!((parsed.confidence - 0.8).abs() < 1e-6);
}

#[test]
fn parse_classifier_output_fence_lowercase_and_uppercase_marker() {
    // Robustness: model may emit `JSON` instead of `json`.
    let raw = "```JSON\n{\"category\":\"not_stalled_complete\",\"confidence\":0.99,\"evidence\":\"done\"}\n```";
    let parsed = parse_classifier_output(raw).expect("uppercase fence parses");
    assert_eq!(parsed.category, LazinessCategory::NotStalledComplete);
}

#[test]
fn parse_classifier_output_unfenced_with_trailing_prose() {
    // Brace-extract path: model wrote prose after the JSON.
    let raw = "{\"category\":\"stalled_no_todos_but_task_in_flight\",\"confidence\":0.75,\"evidence\":\"no todos\"}\n\nLet me know if you need more detail.";
    let parsed = parse_classifier_output(raw).expect("trailing prose extracts");
    assert_eq!(
        parsed.category,
        LazinessCategory::StalledNoTodosButTaskInFlight
    );
}

#[test]
fn parse_classifier_output_brace_extract_handles_escaped_quotes() {
    // Nested `"` inside evidence: the brace counter's string-mode tracking must not be fooled by the inner quote pair
    let raw = r#"prefix garbage {"category":"stalled_narration","confidence":0.71,"evidence":"said \"done\" without doing it"} trailing"#;
    let parsed = parse_classifier_output(raw).expect("escaped quotes parse");
    assert_eq!(parsed.evidence, "said \"done\" without doing it");
}

#[test]
fn parse_classifier_output_truncated_json_returns_unparseable() {
    let raw = "{\"category\":\"stalled_narration\",\"confidence\":";
    let err = parse_classifier_output(raw).expect_err("truncated JSON errors");
    assert!(matches!(err, ClassifierParseError::Unparseable));
}

#[test]
fn parse_classifier_output_unknown_category_returns_unparseable() {
    let raw = r#"{"category":"stalled_napping","confidence":0.9,"evidence":"zzz"}"#;
    let err = parse_classifier_output(raw).expect_err("unknown category errors");
    assert!(matches!(err, ClassifierParseError::Unparseable));
}

#[test]
fn parse_classifier_output_confidence_above_one_is_rejected() {
    let raw = r#"{"category":"stalled_narration","confidence":1.5,"evidence":"x"}"#;
    let err = parse_classifier_output(raw).expect_err("confidence > 1 errors");
    assert!(matches!(err, ClassifierParseError::ConfidenceOutOfRange(c) if (c - 1.5).abs() < 1e-6));
}

#[test]
fn parse_classifier_output_confidence_below_zero_is_rejected() {
    let raw = r#"{"category":"stalled_narration","confidence":-0.1,"evidence":"x"}"#;
    let err = parse_classifier_output(raw).expect_err("confidence < 0 errors");
    assert!(matches!(err, ClassifierParseError::ConfidenceOutOfRange(c) if (c + 0.1).abs() < 1e-6));
}

#[test]
fn parse_classifier_output_literal_nan_is_unparseable() {
    // `NaN` is not valid JSON; serde_json rejects it before our range check sees it
    // The diagnostic is therefore `Unparseable`, not `ConfidenceOutOfRange`
    let raw = r#"{"category":"stalled_narration","confidence":NaN,"evidence":"x"}"#;
    let err = parse_classifier_output(raw).expect_err("literal NaN is invalid JSON");
    assert!(matches!(err, ClassifierParseError::Unparseable));
}

#[test]
fn parse_classifier_output_huge_finite_number_is_out_of_range() {
    // A huge finite number parses but is outside [0.0, 1.0]
    // The contract: never silently accept; either OutOfRange or Unparseable
    // Pins the magnitude on the diagnostic so a future bug that truncates, saturates, or silently casts 1e20 toward 1.0 is caught
    let raw = r#"{"category":"stalled_narration","confidence":1e20,"evidence":"x"}"#;
    let err = parse_classifier_output(raw).expect_err("huge confidence rejected");
    // 1e20 is within f32 range but far outside [0, 1]
    // The assert accepts either a huge finite value or +inf; both mean the model emitted something absurd
    assert!(
        matches!(err, ClassifierParseError::ConfidenceOutOfRange(c) if (c.is_infinite() && c.is_sign_positive()) || (c.is_finite() && c > 1e10)),
        "expected ConfidenceOutOfRange with huge or +inf value, got {err:?}",
    );
}

#[test]
fn parse_classifier_output_brace_extract_handles_literal_braces_in_evidence() {
    // The brace counter's string-mode tracking must skip `{` and `}` inside the evidence string
    // Otherwise the counter closes the object early at the `}` after `1`
    let raw = r#"garbage {"category":"stalled_narration","confidence":0.71,"evidence":"saw {x: 1} in output"} trailing"#;
    let parsed = parse_classifier_output(raw).expect("literal braces in evidence parse");
    assert_eq!(parsed.evidence, "saw {x: 1} in output");
}

#[test]
fn parse_classifier_output_bad_first_pass_does_not_short_circuit_when_other_passes_converge() {
    // The parser chain accumulates bad-confidence errors instead of stopping at the first one.
    // On a bare bad JSON object every pass (strict, fence-strip, brace-extract) lands on the same object, so the diagnostic is the same either way A case where the passes disagree on which slice to parse is hard to construct.
    // Brace-extract takes the first balanced object, the same slice strict parses; fence-strip only fires when the trimmed input starts with a fence.
    let raw = r#"{"category":"stalled_narration","confidence":1.5,"evidence":"bad"}"#;
    let err = parse_classifier_output(raw).expect_err("bad confidence");
    assert!(matches!(err, ClassifierParseError::ConfidenceOutOfRange(c) if (c - 1.5).abs() < 1e-6));
}

#[test]
fn parse_classifier_output_strict_unparseable_then_brace_extract_recovers() {
    // Strict and fence-strip both fail: the input doesn't start with a fence after trimming, and strict can't parse a JSON object embedded in prose
    // Brace-extract finds the inner balanced object and recovers
    // This is the canonical "later pass succeeds where earlier passes failed" pin for the chain design
    let raw = "this is not json — the model said: {\"category\":\"stalled_narration\",\"confidence\":0.91,\"evidence\":\"good\"} extra trailing prose";
    let parsed = parse_classifier_output(raw).expect("brace-extract recovers");
    assert!((parsed.confidence - 0.91).abs() < 1e-6);
    assert_eq!(parsed.evidence, "good");
}

// ── evaluate_laziness ───────────────────────────────────────────

fn output(category: LazinessCategory, confidence: f32) -> ClassifierOutput {
    ClassifierOutput {
        category,
        confidence,
        evidence: "ev".to_string(),
    }
}

#[test]
fn evaluate_laziness_observation_only_returns_nudge_cap_exhausted() {
    // With the feature enabled but a cap of 0, a stalled high-confidence verdict must still return NoNudge{CapExhausted}
    // The caller emits `LazinessClassifierFired` but not `LazinessNudgeFired`
    let cfg = cfg_enabled(0);
    let decision = evaluate_laziness(
        &output(LazinessCategory::StalledNarration, 0.9),
        &cfg,
        0,
        LAZINESS_DEFAULT_MIN_CONFIDENCE,
    );
    assert!(matches!(
        decision,
        LazinessDecision::NoNudge {
            reason: NoNudgeReason::CapExhausted,
            ..
        }
    ));
}

#[test]
fn evaluate_laziness_disabled_returns_feature_disabled() {
    let cfg = LazinessDetectorPerModelConfig::default();
    let decision = evaluate_laziness(
        &output(LazinessCategory::StalledNarration, 0.9),
        &cfg,
        0,
        LAZINESS_DEFAULT_MIN_CONFIDENCE,
    );
    assert!(matches!(
        decision,
        LazinessDecision::NoNudge {
            reason: NoNudgeReason::FeatureDisabled,
            ..
        }
    ));
}

#[test]
fn evaluate_laziness_low_confidence_returns_low_confidence() {
    let cfg = cfg_enabled(3);
    let decision = evaluate_laziness(
        &output(LazinessCategory::StalledNarration, 0.5),
        &cfg,
        0,
        LAZINESS_DEFAULT_MIN_CONFIDENCE,
    );
    assert!(matches!(
        decision,
        LazinessDecision::NoNudge {
            reason: NoNudgeReason::LowConfidence,
            ..
        }
    ));
}

#[test]
fn evaluate_laziness_not_stalled_returns_not_stalled() {
    let cfg = cfg_enabled(3);
    let decision = evaluate_laziness(
        &output(LazinessCategory::NotStalledComplete, 0.99),
        &cfg,
        0,
        LAZINESS_DEFAULT_MIN_CONFIDENCE,
    );
    assert!(matches!(
        decision,
        LazinessDecision::NoNudge {
            reason: NoNudgeReason::NotStalled,
            ..
        }
    ));
}

#[test]
fn evaluate_laziness_passes_when_all_gates_pass() {
    let cfg = cfg_enabled(3);
    let decision = evaluate_laziness(
        &output(LazinessCategory::StalledPermissionAsking, 0.85),
        &cfg,
        0,
        LAZINESS_DEFAULT_MIN_CONFIDENCE,
    );
    let LazinessDecision::Nudge {
        category,
        confidence,
        evidence,
    } = decision
    else {
        panic!("expected Nudge");
    };
    assert_eq!(category, LazinessCategory::StalledPermissionAsking);
    assert!((confidence - 0.85).abs() < 1e-6);
    assert_eq!(evidence, "ev");
}

#[test]
fn evaluate_laziness_per_model_min_confidence_overrides_default() {
    // Per-model override: if the caller is willing to nudge at lower confidence, `evaluate_laziness` must honor it
    let cfg = LazinessDetectorPerModelConfig {
        enabled: true,
        max_nudges_per_session: 3,
        idle_threshold_ms: None,
        min_confidence: Some(0.4),
        include_reasoning: None,
    };
    let decision = evaluate_laziness(
        &output(LazinessCategory::StalledNarration, 0.5),
        &cfg,
        0,
        LAZINESS_DEFAULT_MIN_CONFIDENCE,
    );
    assert!(matches!(decision, LazinessDecision::Nudge { .. }));
}

#[test]
fn evaluate_laziness_session_counter_at_cap_returns_cap_exhausted() {
    let cfg = cfg_enabled(2);
    let decision = evaluate_laziness(
        &output(LazinessCategory::StalledNarration, 0.9),
        &cfg,
        2,
        LAZINESS_DEFAULT_MIN_CONFIDENCE,
    );
    assert!(matches!(
        decision,
        LazinessDecision::NoNudge {
            reason: NoNudgeReason::CapExhausted,
            ..
        }
    ));
}

// ── nudge text builder ──────────────────────────────────────────

#[test]
fn build_laziness_nudge_quotes_rule_by_name_per_category() {
    let n = build_laziness_nudge(LazinessCategory::StalledNarration, "ev1", None);
    assert!(n.contains("ev1"));

    let n = build_laziness_nudge(LazinessCategory::StalledPermissionAsking, "ev2", None);
    assert!(n.contains("ev2"));

    let n = build_laziness_nudge(
        LazinessCategory::StalledNoTodosButTaskInFlight,
        "ev3",
        Some("todo_write"),
    );
    assert!(n.contains("todo_write"));
    assert!(n.contains("ev3"));

    let n = build_laziness_nudge(
        LazinessCategory::StalledNoTodosButTaskInFlight,
        "ev3b",
        None,
    );
    assert!(n.contains("ev3b"));

    let n = build_laziness_nudge(LazinessCategory::StalledFalseCompletion, "ev4", None);
    assert!(n.contains("ev4"));
}

// ── turn_elapsed_seconds_from_start_ms ─────────────────────────

#[test]
fn turn_elapsed_seconds_from_start_ms_returns_none_when_start_absent() {
    // No `turn_start_ms` recorded yet (fresh session, pre-prompt), so the field must be dropped, not emitted as 0
    assert_eq!(
        super::turn_elapsed_seconds_from_start_ms(None, 1_000_000),
        None,
    );
}

#[test]
fn turn_elapsed_seconds_from_start_ms_computes_seconds() {
    // A 5 432 ms delta gives 5 s; the sub-second portion truncates
    assert_eq!(
        super::turn_elapsed_seconds_from_start_ms(Some(1_000_000), 1_005_432),
        Some(5),
    );
}

#[test]
fn turn_elapsed_seconds_from_start_ms_truncates_sub_second_to_zero() {
    // A 500 ms delta gives 0, the explicit "very recent" value (see the helper doc)
    assert_eq!(
        super::turn_elapsed_seconds_from_start_ms(Some(1_000_000), 1_000_500),
        Some(0),
    );
}

#[test]
fn turn_elapsed_seconds_from_start_ms_returns_none_on_negative_delta() {
    // A backward clock jump: an NTP step, or a snapshot from a future session restored into an older actor
    // Drop the field rather than emit a meaningless value
    assert_eq!(
        super::turn_elapsed_seconds_from_start_ms(Some(2_000_000), 1_000_000),
        None,
    );
}

#[test]
fn turn_elapsed_seconds_from_start_ms_handles_long_overnight_run() {
    // A gap of about 10 hours overnight must not overflow or truncate the u64 cast
    let start = 1_000_000_000_000_i64;
    let now = start + 10 * 60 * 60 * 1000;
    assert_eq!(
        super::turn_elapsed_seconds_from_start_ms(Some(start), now),
        Some(36_000),
    );
}

#[test]
fn evaluate_laziness_false_completion_above_threshold_returns_nudge() {
    let cfg = cfg_enabled(3);
    let decision = evaluate_laziness(
        &output(LazinessCategory::StalledFalseCompletion, 0.9),
        &cfg,
        0,
        LAZINESS_DEFAULT_MIN_CONFIDENCE,
    );
    let LazinessDecision::Nudge { category, .. } = decision else {
        panic!("expected Nudge for StalledFalseCompletion at confidence 0.9");
    };
    assert_eq!(category, LazinessCategory::StalledFalseCompletion);
}

#[test]
fn laziness_post_classifier_nudge_off_goal_skips_injection() {
    let nudge = LazinessDecision::Nudge {
        category: LazinessCategory::StalledNarration,
        confidence: 0.9,
        evidence: "stalled".to_string(),
    };
    assert!(matches!(nudge, LazinessDecision::Nudge { .. }));
    assert!(
        !laziness_injection_active(
            false,
            Some(crate::session::goal_tracker::GoalStatus::Active)
        ),
        "inactive goal_enabled blocks injection after ClassifierFired"
    );
    assert!(laziness_injection_active(
        true,
        Some(crate::session::goal_tracker::GoalStatus::Active)
    ));
}

#[test]
fn build_laziness_nudge_returns_empty_for_not_stalled() {
    // Only stalled_* variants reach the nudge builder via `evaluate_laziness`
    // The function still returns empty text if a caller bypasses the gate
    for variant in [
        LazinessCategory::NotStalledComplete,
        LazinessCategory::NotStalledWaitingOnBackground,
        LazinessCategory::NotStalledWaitingOnUser,
    ] {
        assert!(
            build_laziness_nudge(variant, "ev", None).is_empty(),
            "{variant:?} must produce no nudge text"
        );
    }
}
