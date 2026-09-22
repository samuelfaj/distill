// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use distill_chat_state::UsageLedger;
use distill_sampling_types::TokenUsage;

fn tu(prompt: u32, completion: u32) -> TokenUsage {
    TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        reasoning_tokens: 0,
        cached_prompt_tokens: 0,
        cache_creation_prompt_tokens: 0,
    }
}

fn attribution(id: &str) -> distill_chat_state::UsageAttribution {
    distill_chat_state::UsageAttribution {
        attempt_id: id.to_owned(),
        task_id: None,
        turn_id: None,
        request_id: None,
        role: "auxiliary".to_owned(),
        model_id: "m".to_owned(),
        endpoint: None,
        requested_effort: None,
        applied_effort: None,
        status: distill_chat_state::UsageCallStatus::Failed,
        usage: None,
        usage_complete: false,
        api_duration_ms: None,
        cost_usd_ticks: None,
        cost_basis: distill_chat_state::UsageCostBasis::Unknown,
    }
}

fn live(calls: &[(&str, u32, u32, Option<i64>)]) -> UsageSummary {
    let mut ledger = UsageLedger::default();
    for (model, prompt, completion, cost) in calls {
        ledger.record_main_loop_call(model, &tu(*prompt, *completion), Some(10), *cost);
    }
    UsageSummary::from_ledger(&ledger)
}

#[test]
fn first_turn_writes_session_and_one_turn() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50))]);
    file.apply_turn(1, "2026-08-26T00:00:00Z", &first, None);

    let [t0] = file.turns.as_slice() else {
        panic!("expected one turn: {:?}", file.turns);
    };
    assert_eq!(t0.turn_number, 1);
    assert_eq!(t0.usage.input_tokens, 100);
    assert_eq!(t0.usage.output_tokens, 20);
    assert_eq!(t0.usage.cost_usd_ticks, Some(50));
    assert_eq!(file.session.input_tokens, 100);
    assert_eq!(file.session.output_tokens, 20);
    assert_eq!(file.session.turn_count, 1);
    assert_eq!(file.session.cost_usd_ticks, Some(50));
    assert_eq!(file.session.primary_model_id.as_deref(), Some("grok-4"));
    assert_eq!(file.updated_at, "2026-08-26T00:00:00Z");
}

#[test]
fn auxiliary_missing_usage_is_visible_as_incomplete_and_non_free() {
    let mut ledger = UsageLedger::default();
    ledger.record_attribution(attribution("aux-missing"));

    let summary = UsageSummary::from_ledger(&ledger);
    assert_eq!(summary.model_calls, 1);
    assert_eq!(summary.input_tokens, 0);
    assert_eq!(summary.output_tokens, 0);
    assert_eq!(summary.cost_usd_ticks, None);
    assert!(summary.cost_is_partial);
    assert!(summary.usage_is_incomplete);
    assert_eq!(summary.model_usage["m"].model_calls, 1);
}

#[test]
fn session_primary_model_is_the_most_used_not_the_last_turn() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 80, 10, Some(40))]);
    file.apply_turn(1, "t1", &first, None);
    file.apply_turn(
        2,
        "t2",
        &live(&[
            ("grok-4", 100, 20, Some(50)),
            ("grok-4", 80, 10, Some(40)),
            ("grok-fast", 10, 2, Some(1)),
        ]),
        Some(&first),
    );

    assert_eq!(
        file.turns
            .get(1)
            .and_then(|t| t.usage.primary_model_id.as_deref()),
        Some("grok-fast")
    );
    assert_eq!(file.session.primary_model_id.as_deref(), Some("grok-4"));
}

#[test]
fn second_turn_appends_and_session_becomes_latest_ledger() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50))]);
    file.apply_turn(1, "t1", &first, None);
    file.apply_turn(
        2,
        "t2",
        &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
        Some(&first),
    );

    let [_, t1] = file.turns.as_slice() else {
        panic!("expected two turns: {:?}", file.turns);
    };
    assert_eq!(t1.turn_number, 2);
    assert_eq!(t1.usage.input_tokens, 40);
    assert_eq!(t1.usage.output_tokens, 10);
    assert_eq!(t1.usage.cost_usd_ticks, Some(20));
    assert_eq!(file.session.input_tokens, 140);
    assert_eq!(file.session.output_tokens, 30);
    assert_eq!(file.session.turn_count, 2);
    assert_eq!(file.session.cost_usd_ticks, Some(70));
}

#[test]
fn inherited_turn_number_without_fold_appends() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50))]);
    file.apply_turn(1, "t1", &first, None);
    file.restore_apply_cursor(None, None);
    let resumed = live(&[("grok-4", 10, 2, Some(5))]);
    file.apply_turn(1, "t-resume", &resumed, None);

    let [t0, t1] = file.turns.as_slice() else {
        panic!("expected two turns: {:?}", file.turns);
    };
    assert_eq!(t0.turn_number, 1);
    assert_eq!(t0.usage.input_tokens, 100);
    assert_eq!(t1.turn_number, 2);
    assert_eq!(t1.usage.input_tokens, 10);
    assert_eq!(file.session.input_tokens, 110);
    assert_eq!(file.session.turn_count, 2);
}

#[test]
fn duplicate_turn_number_zero_delta_does_not_mutate_turns() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50))]);
    file.apply_turn(1, "t1", &first, None);
    file.apply_turn(1, "t1-again", &first, Some(&first));

    let [t0] = file.turns.as_slice() else {
        panic!("expected one turn: {:?}", file.turns);
    };
    assert_eq!(t0.ended_at, "t1");
    assert_eq!(file.session.turn_count, 1);
    assert_eq!(file.updated_at, "t1-again");
}

#[test]
fn duplicate_turn_number_folds_extra_live_usage() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50))]);
    file.apply_turn(1, "t1", &first, None);
    let continued = live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]);
    file.apply_turn(1, "t1-late", &continued, Some(&first));

    let [t0] = file.turns.as_slice() else {
        panic!("expected one turn: {:?}", file.turns);
    };
    assert_eq!(t0.ended_at, "t1-late");
    assert_eq!(t0.usage.input_tokens, 140);
    assert_eq!(t0.usage.output_tokens, 30);
    assert_eq!(t0.usage.cost_usd_ticks, Some(70));
    assert_eq!(file.session.input_tokens, 140);
    assert_eq!(file.session.output_tokens, 30);
    assert_eq!(file.session.turn_count, 1);
    assert_eq!(file.session.cost_usd_ticks, Some(70));
}

#[test]
fn resume_folds_new_process_ledger_onto_persisted_session() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50))]);
    file.apply_turn(1, "t1", &first, None);
    file.apply_turn(
        2,
        "t2",
        &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
        Some(&first),
    );

    let post_resume_1 = live(&[("grok-4", 25, 5, Some(8))]);
    file.apply_turn(3, "t3", &post_resume_1, None);

    let [_, _, t2] = file.turns.as_slice() else {
        panic!("expected three turns: {:?}", file.turns);
    };
    assert_eq!(t2.turn_number, 3);
    assert_eq!(t2.usage.input_tokens, 25);
    assert_eq!(t2.usage.output_tokens, 5);
    assert_eq!(file.session.input_tokens, 165);
    assert_eq!(file.session.output_tokens, 35);
    assert_eq!(file.session.turn_count, 3);
    assert_eq!(file.session.cost_usd_ticks, Some(78));
}

#[test]
fn resume_later_turns_use_process_local_delta() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50))]);
    file.apply_turn(1, "t1", &first, None);
    file.apply_turn(
        2,
        "t2",
        &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
        Some(&first),
    );

    let post_resume_1 = live(&[("grok-4", 25, 5, Some(8))]);
    file.apply_turn(3, "t3", &post_resume_1, None);
    file.apply_turn(
        4,
        "t4",
        &live(&[("grok-4", 25, 5, Some(8)), ("grok-4", 30, 6, Some(9))]),
        Some(&post_resume_1),
    );

    let [_, _, _, t3] = file.turns.as_slice() else {
        panic!("expected four turns: {:?}", file.turns);
    };
    assert_eq!(t3.usage.input_tokens, 30);
    assert_eq!(t3.usage.output_tokens, 6);
    assert_eq!(file.session.input_tokens, 195);
    assert_eq!(file.session.output_tokens, 41);
    assert_eq!(file.session.turn_count, 4);
    assert_eq!(file.session.cost_usd_ticks, Some(87));
}

#[test]
fn retain_turns_through_drops_later_turns_and_rebuilds_session() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50))]);
    file.apply_turn(1, "t1", &first, None);
    file.apply_turn(
        2,
        "t2",
        &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
        Some(&first),
    );
    file.retain_turns_through(1);

    let [t0] = file.turns.as_slice() else {
        panic!("expected one turn: {:?}", file.turns);
    };
    assert_eq!(t0.turn_number, 1);
    assert_eq!(file.session.input_tokens, 100);
    assert_eq!(file.session.turn_count, 1);
    assert_eq!(file.session.cost_usd_ticks, Some(50));
}

#[test]
fn turn_lookup_returns_matching_row() {
    let mut file = SessionUsageFile::new("sess-1");
    let first = live(&[("grok-4", 100, 20, Some(50))]);
    file.apply_turn(1, "t1", &first, None);
    file.apply_turn(
        2,
        "t2",
        &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
        Some(&first),
    );

    assert_eq!(file.turn(2).unwrap().usage.input_tokens, 40);
    assert!(file.turn(3).is_none());
}

#[test]
fn covers_detects_same_process_vs_reset_ledger() {
    let bigger = live(&[("m", 10, 1, None), ("m", 5, 1, None)]);
    let smaller = live(&[("m", 5, 1, None)]);
    assert!(bigger.covers(&smaller));
    assert!(!smaller.covers(&bigger));
    assert!(smaller.covers(&UsageSummary::default()));
}

#[test]
fn usage_summary_keeps_attribution_ids_unique_when_rows_are_folded() {
    let duplicate = attribution("attempt-1");
    let mut first = UsageSummary::default();
    first.attributions.push(duplicate.clone());
    let mut second = UsageSummary::default();
    second
        .attributions
        .extend([duplicate, attribution("attempt-2")]);

    let merged = first.saturating_add(&second);
    assert_eq!(
        merged
            .attributions
            .iter()
            .map(|row| row.attempt_id.as_str())
            .collect::<Vec<_>>(),
        vec!["attempt-1", "attempt-2"]
    );

    let mut different = UsageSummary::default();
    different.attributions.push(attribution("other"));
    assert!(!different.covers(&first));
}

fn reported_live(calls: &[(&str, i64)]) -> UsageSummary {
    let mut ledger = UsageLedger::default();
    for (id, cost) in calls {
        let mut row = attribution(id);
        row.status = distill_chat_state::UsageCallStatus::Completed;
        row.usage = Some(tu(1, 1));
        row.usage_complete = true;
        row.cost_usd_ticks = Some(*cost);
        row.cost_basis = distill_chat_state::UsageCostBasis::Reported;
        ledger.record_attribution(row);
    }
    UsageSummary::from_ledger(&ledger)
}

fn reported_free_live(ids: &[&str]) -> UsageSummary {
    let calls: Vec<_> = ids.iter().map(|id| (*id, 0)).collect();
    reported_live(&calls)
}

#[test]
fn persistence_preserves_reported_free_cost_but_not_legacy_zero() {
    let first = reported_free_live(&["free-1"]);
    let continued = reported_free_live(&["free-1", "free-2"]);
    let mut file = SessionUsageFile::new("sess-1");
    file.apply_turn(1, "t1", &first, None);
    file.apply_turn(2, "t2", &continued, Some(&first));

    assert_eq!(file.turn(1).unwrap().usage.cost_usd_ticks, Some(0));
    assert_eq!(file.turn(2).unwrap().usage.cost_usd_ticks, Some(0));
    assert_eq!(file.session.cost_usd_ticks, Some(0));
    for usage in [
        &file.turn(1).unwrap().usage,
        &file.turn(2).unwrap().usage,
        &file.session,
    ] {
        let model = usage.model_usage.get("m").expect("free model row");
        assert_eq!(model.cost_usd_ticks, Some(0));
        assert!(!model.cost_is_partial);
    }

    let paid = reported_live(&[("paid-1", 7)]);
    let paid_and_free = first.saturating_add(&paid);
    assert_eq!(paid_and_free.cost_usd_ticks, Some(7));
    assert!(!paid_and_free.cost_is_partial);
    let model = paid_and_free
        .model_usage
        .get("m")
        .expect("paid and free model row");
    assert_eq!(model.cost_usd_ticks, Some(7));
    assert!(!model.cost_is_partial);

    let legacy_zero = UsageSummary {
        cost_usd_ticks: Some(0),
        ..UsageSummary::default()
    };
    assert_eq!(
        legacy_zero
            .saturating_add(&UsageSummary::default())
            .cost_usd_ticks,
        None
    );
    assert_eq!(
        legacy_zero
            .saturating_sub(&UsageSummary::default())
            .cost_usd_ticks,
        None
    );

    let legacy_unknown_call = UsageSummary {
        model_calls: 1,
        cost_usd_ticks: Some(0),
        ..UsageSummary::default()
    };
    let mixed = legacy_unknown_call.saturating_add(&first);
    assert_eq!(mixed.cost_usd_ticks, Some(0));
    assert!(mixed.cost_is_partial);

    let mut legacy_model = legacy_unknown_call.clone();
    legacy_model.model_usage.insert(
        "m".to_owned(),
        UsageSummary {
            model_calls: 1,
            cost_usd_ticks: Some(0),
            ..UsageSummary::default()
        },
    );
    let mixed_model = legacy_model.saturating_add(&first);
    let model = mixed_model.model_usage.get("m").expect("mixed model row");
    assert_eq!(model.cost_usd_ticks, None);
    assert!(model.cost_is_partial);
}
