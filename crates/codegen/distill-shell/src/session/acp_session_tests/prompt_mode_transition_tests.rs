use super::support::*;
use super::*;
use crate::session::plan_mode::PlanModeState;
/// An actor plus the `SessionEvent` channel that carries its mode updates.
/// Plan-mode changes deliberately queue behind the turn's streaming deltas rather than emitting straight to the client.
/// The assertions therefore read that channel and not the gateway.
async fn actor_with_events() -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await
}
/// Every mode id the actor has queued for the client so far, in order.
fn mode_updates(rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Ok(SessionEvent::Notification(notification)) = rx.try_recv() {
        let SessionNotification::Acp(notification) = notification else {
            continue;
        };
        if let acp::SessionUpdate::CurrentModeUpdate(update) = &notification.update {
            seen.push(update.current_mode_id.0.to_string());
        }
    }
    seen
}
#[test]
fn prompt_mode_from_session_mode_id_uses_acp_session_mode() {
    assert_eq!(
        PromptMode::Ask,
        prompt_mode_from_session_mode_id(&acp::SessionModeId::new("ask"))
    );
    assert_eq!(
        PromptMode::Plan,
        prompt_mode_from_session_mode_id(&acp::SessionModeId::new("plan"))
    );
    assert_eq!(
        PromptMode::Agent,
        prompt_mode_from_session_mode_id(&acp::SessionModeId::new("default"))
    );
    assert_eq!(
        PromptMode::Agent,
        prompt_mode_from_session_mode_id(&acp::SessionModeId::new("browser_use"))
    );
}
fn fn_def(name: &str) -> ToolDefinition {
    ToolDefinition::function(name, None::<&str>, serde_json::json!({"type": "object"}))
}
fn names(defs: &[ToolDefinition]) -> Vec<&str> {
    defs.iter().map(|d| d.function.name.as_str()).collect()
}
#[test]
fn cursor_filter_in_plan_mode_keeps_writes_and_shows_create_plan() {
    let defs = vec![
        fn_def("Read"),
        fn_def("Grep"),
        fn_def("Write"),
        fn_def("StrReplace"),
        fn_def("CreatePlan"),
        fn_def("SwitchMode"),
        fn_def("AskQuestion"),
    ];
    let filtered = filter_cursor_tools_by_plan_mode(defs, true);
    let kept = names(&filtered);
    assert!(kept.contains(&"Read"));
    assert!(kept.contains(&"Grep"));
    assert!(kept.contains(&"CreatePlan"));
    assert!(kept.contains(&"SwitchMode"));
    assert!(kept.contains(&"AskQuestion"));
    assert!(kept.contains(&"Write"));
    assert!(kept.contains(&"StrReplace"));
}
#[test]
fn cursor_filter_is_noop_for_non_cursor_tools() {
    let defs = vec![
        fn_def("read_file"),
        fn_def("search_replace"),
        fn_def("write"),
        fn_def("ask_user_question"),
        fn_def("enter_plan_mode"),
        fn_def("exit_plan_mode"),
    ];
    let in_plan = filter_cursor_tools_by_plan_mode(defs.clone(), true);
    let out_of_plan = filter_cursor_tools_by_plan_mode(defs.clone(), false);
    assert_eq!(names(&in_plan).len(), defs.len());
    assert_eq!(names(&out_of_plan).len(), defs.len());
}
/// Pins the `reconcile_plan_mode_with_prompt` transitions: Plan enters Pending, re-entry is idempotent, and non-plan modes exit cleanly.
#[test]
fn prompt_mode_plan_drives_tracker_into_pending_when_inactive() {
    use crate::session::plan_mode::PlanModeTracker;
    use std::path::PathBuf;
    fn reconcile(tracker: &mut PlanModeTracker, mode: PromptMode) {
        match mode {
            PromptMode::Plan => {
                tracker.enter_pending();
            }
            PromptMode::Agent | PromptMode::Ask => {
                if tracker.state() != PlanModeState::Inactive {
                    tracker.user_exit(false);
                }
            }
        }
    }
    let mut tracker = PlanModeTracker::new(PathBuf::from("/tmp/test"));
    assert_eq!(tracker.state(), PlanModeState::Inactive);
    reconcile(&mut tracker, PromptMode::Plan);
    assert_eq!(tracker.state(), PlanModeState::Pending);
    reconcile(&mut tracker, PromptMode::Plan);
    assert_eq!(tracker.state(), PlanModeState::Pending);
    reconcile(&mut tracker, PromptMode::Agent);
    assert_eq!(tracker.state(), PlanModeState::Inactive);
    reconcile(&mut tracker, PromptMode::Plan);
    assert_eq!(tracker.state(), PlanModeState::Pending);
    reconcile(&mut tracker, PromptMode::Ask);
    assert_eq!(tracker.state(), PlanModeState::Inactive);
}
#[test]
fn session_mode_id_from_prompt_mode_inverts_the_parse() {
    for id in ["plan", "ask", "default"] {
        let mode_id = acp::SessionModeId::new(id);
        let round_tripped =
            session_mode_id_from_prompt_mode(prompt_mode_from_session_mode_id(&mode_id));
        assert_eq!(round_tripped.0.as_ref(), id);
    }
}
/// A prompt that declares `_meta.mode` is the client changing mode, and the client has to be told it took effect.
/// Both arms used to persist the transition and inject the model's reminder but emit nothing.
/// A client that carries its mode on the prompt could enter or leave plan mode with no signal, and `updates.jsonl` carried no mode line for replay.
#[tokio::test]
async fn a_declared_mode_change_is_published_to_the_client() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut event_rx) = actor_with_events().await;
            actor.reconcile_plan_mode_with_prompt(PromptMode::Plan);
            actor.reconcile_plan_mode_with_prompt(PromptMode::Agent);
            assert_eq!(actor.plan_mode.lock().state(), PlanModeState::Inactive);
            assert_eq!(
                mode_updates(&mut event_rx),
                vec!["plan".to_string(), "default".to_string()],
            );
        })
        .await;
}
/// `ask` is its own client-facing mode, so leaving plan for it must not report `default`.
#[tokio::test]
async fn leaving_plan_for_ask_reports_ask() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut event_rx) = actor_with_events().await;
            actor.reconcile_plan_mode_with_prompt(PromptMode::Plan);
            actor.reconcile_plan_mode_with_prompt(PromptMode::Ask);
            assert_eq!(
                mode_updates(&mut event_rx),
                vec!["plan".to_string(), "ask".to_string()],
            );
        })
        .await;
}
/// Re-declaring the mode already in effect is not a mode change.
/// A client that mirrors the session's mode back onto every prompt would otherwise emit one `CurrentModeUpdate` per turn.
#[tokio::test]
async fn redeclaring_the_mode_already_in_effect_publishes_nothing() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut event_rx) = actor_with_events().await;
            actor.reconcile_plan_mode_with_prompt(PromptMode::Plan);
            assert_eq!(mode_updates(&mut event_rx), vec!["plan".to_string()]);
            actor.reconcile_plan_mode_with_prompt(PromptMode::Plan);
            actor.reconcile_plan_mode_with_prompt(PromptMode::Plan);
            assert!(mode_updates(&mut event_rx).is_empty());
            actor.reconcile_plan_mode_with_prompt(PromptMode::Agent);
            assert_eq!(mode_updates(&mut event_rx), vec!["default".to_string()]);
            actor.reconcile_plan_mode_with_prompt(PromptMode::Agent);
            assert!(mode_updates(&mut event_rx).is_empty());
        })
        .await;
}
/// A synthetic turn (a background task wake, a goal summary, a notification drain) declares no mode; it is constructed with a placeholder `Agent`.
/// Treating that placeholder as a declaration ended plan mode just by waking the session.
/// It ended silently: nothing was emitted, so the indicator stayed lit for the rest of the session while the agent was back in agent mode.
#[tokio::test]
async fn a_synthetic_turn_inherits_plan_mode_instead_of_ending_it() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut event_rx) = actor_with_events().await;
            actor.reconcile_plan_mode_with_prompt(PromptMode::Plan);
            actor.plan_mode.lock().activate();
            let _ = mode_updates(&mut event_rx);
            for prompt_id in [
                "task-completed-abc",
                "subagent-completed-abc",
                "workflow-completed-abc",
                "notifications-1",
                "goal-summary-1",
                "goal-classifier-nudge-1",
                "scheduler-fired-1",
                "plan-resume-1",
            ] {
                let origin = crate::session::PromptOrigin::from_prompt_id(prompt_id);
                let resolved = actor.resolve_turn_prompt_mode(&origin, PromptMode::Agent);
                assert_eq!(
                    actor.plan_mode.lock().state(),
                    PlanModeState::Active,
                    "{prompt_id} must not end plan mode"
                );
                assert_eq!(
                    resolved,
                    PromptMode::Plan,
                    "{prompt_id} runs under the session's mode, so it is recorded under it too"
                );
                assert!(
                    mode_updates(&mut event_rx).is_empty(),
                    "{prompt_id} changed no mode, so it must announce none"
                );
            }
        })
        .await;
}
/// The other half of the same rule: a real user turn still applies what it declared, and the resolved mode is what it asked for.
#[tokio::test]
async fn a_user_turn_still_applies_its_declared_mode() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut event_rx) = actor_with_events().await;
            let origin = crate::session::PromptOrigin::from_prompt_id("prompt-1");
            assert!(!origin.is_synthetic(), "precondition");
            let resolved = actor.resolve_turn_prompt_mode(&origin, PromptMode::Plan);
            assert_eq!(resolved, PromptMode::Plan);
            assert_eq!(actor.plan_mode.lock().state(), PlanModeState::Pending);
            assert_eq!(mode_updates(&mut event_rx), vec!["plan".to_string()]);
        })
        .await;
}
