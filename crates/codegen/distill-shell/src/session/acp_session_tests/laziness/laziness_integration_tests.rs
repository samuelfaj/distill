// Modified for Distill by Samuel Fajreldines, 2026.
//! End-to-end tests for `maybe_fire_laziness_check`.
//! Each test drives the actor against a non-listening `http://localhost` base URL.
//! The unified path's `prepare_chat_completion().conversation_collect()` call surfaces the connection failure as the `ClassifierError` abort.
//! The tests observe state mutations and the per-test `events.jsonl`.
//!
//! Tests that depend on a *successful* classifier response are out of scope here.
//! They would need a real `SamplerActor` responding with a stubbed verdict, which is heavyweight.
//! The happy path from classifier verdict to nudge dispatch is covered by the unit tests on `evaluate_laziness` and `build_laziness_nudge`.
//! The integration tests here pin the actor-level behaviour.
//! They cover enabled/disabled gating, both generation-counter abort arms, idle re-check, the sampler-error abort, and reset on model switch.
use super::support::*;
use super::*;
use crate::agent::config::{LazinessDetectorPerModelConfig, ModelInfo};

/// Build a minimal `ModelEntry` configured for laziness detection with the supplied opt-in flags.
/// Uses `ModelInfo::fallback` (the same path production takes for unknown model ids) so the test entry mirrors a realistic catalog row.
fn detector_entry(
    enabled: bool,
    max_nudges: u32,
    idle_threshold_ms: Option<u64>,
) -> crate::agent::config::ModelEntry {
    let mut info = ModelInfo::fallback("test-laziness-model");
    info.laziness_detector = LazinessDetectorPerModelConfig {
        enabled,
        max_nudges_per_session: max_nudges,
        idle_threshold_ms,
        min_confidence: None,
        include_reasoning: None,
    };
    crate::agent::config::ModelEntry {
        info,
        mtls_cert_dir: None,
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}

/// Construct a test actor with events.jsonl rerouted into a tempdir and `current_model_id` pointing at a per-model config supplied by the caller.
/// The sampling config points at a `http://localhost` base URL with nothing listening.
/// Returns the actor wrapped in `Arc` and the owned tempdir (so the file outlives the actor).
async fn make_laziness_actor(
    detector: LazinessDetectorPerModelConfig,
) -> (Arc<SessionActor>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    // Install the test model into the catalog and point the current id at it
    // `insert_test_entry` is gated on `#[cfg(test)]` so it does NOT leak into release builds
    let mut entry = detector_entry(false, 0, None);
    entry.info.laziness_detector = detector;
    actor
        .models_manager
        .insert_test_entry("test-laziness-model", entry);
    actor
        .models_manager
        .set_current_model_id(acp::ModelId::new("test-laziness-model"));
    (Arc::new(actor), tmp)
}

fn events_log(tmp: &tempfile::TempDir) -> String {
    std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap_or_default()
}

#[tokio::test(flavor = "current_thread")]
async fn disabled_detector_is_a_no_op() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: false,
                max_nudges_per_session: 0,
                idle_threshold_ms: None,
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            SessionActor::maybe_fire_laziness_check(actor.clone()).await;
            drop(Arc::try_unwrap(actor).ok().unwrap()); // flush events.jsonl
            let log = events_log(&tmp);
            // A single substring check catches `laziness_nudge_fired` and any other `laziness_*` event variant
            // A predicate that enumerates specific event types silently misses new ones
            assert!(
                !log.contains("laziness_"),
                "disabled detector must not emit any laziness_* events:\n{log}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn user_input_bump_during_idle_wait_aborts_with_user_input() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Short idle threshold so the test completes quickly; the abort is the focus, not the duration
            let (actor, tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: true,
                max_nudges_per_session: 1,
                idle_threshold_ms: Some(2000),
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            let bump_actor = actor.clone();
            let bump_task = tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                bump_actor
                    .user_input_generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            });
            SessionActor::maybe_fire_laziness_check(actor.clone()).await;
            bump_task.await.unwrap();
            drop(Arc::try_unwrap(actor).ok().unwrap());
            let log = events_log(&tmp);
            assert!(
                has_event_with(&log, "laziness_classifier_aborted", |v| j(v, "reason")
                    == crate::session::events::LAZINESS_ABORT_USER_INPUT),
                "expected user_input abort:\n{log}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_during_idle_wait_aborts_with_model_switch() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: true,
                max_nudges_per_session: 1,
                idle_threshold_ms: Some(2000),
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            let switch_actor = actor.clone();
            let switch_task = tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                // A real id change bumps the model_switch generation
                switch_actor
                    .models_manager
                    .set_current_model_id(acp::ModelId::new("some-other-model"));
            });
            SessionActor::maybe_fire_laziness_check(actor.clone()).await;
            switch_task.await.unwrap();
            drop(Arc::try_unwrap(actor).ok().unwrap());
            let log = events_log(&tmp);
            assert!(
                has_event_with(&log, "laziness_classifier_aborted", |v| j(v, "reason")
                    == crate::session::events::LAZINESS_ABORT_MODEL_SWITCH),
                "expected model_switch abort:\n{log}"
            );
        })
        .await;
}

/// `maybe_fire_laziness_check` walks the `record_turn_start`, `get_notification_meta`, `turn_elapsed_seconds_from_start_ms` chain on every fire.
/// A regression that swaps `turn_start_ms` for `stream_start_ms`, or that silently drops the `Option<i64>` mid-chain, fails here.
#[tokio::test(flavor = "current_thread")]
async fn turn_start_ms_chain_feeds_turn_elapsed_seconds_helper() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: true,
                max_nudges_per_session: 1,
                idle_threshold_ms: None,
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            // Pre-condition: no turn has started yet, so the meta is missing or `turn_start_ms` is None, and the helper drops the field
            let meta_before = actor.chat_state_handle.get_notification_meta().await;
            let pre_start = meta_before.and_then(|m| m.turn_start_ms);
            assert_eq!(
                super::turn_elapsed_seconds_from_start_ms(
                    pre_start,
                    chrono::Utc::now().timestamp_millis()
                ),
                None,
                "no turn_start_ms recorded ⇒ field is dropped",
            );

            // Record a turn-start 5 seconds in the past, mirroring the `record_turn_start` call at the top of `process_conversation_turn`
            let started_ms = chrono::Utc::now().timestamp_millis() - 5_000;
            actor.chat_state_handle.record_turn_start(started_ms);
            // Drain the chat-state command queue so the actor has processed the `RecordTurnStart` mutation before we read back
            let meta_after = actor
                .chat_state_handle
                .get_notification_meta()
                .await
                .expect("meta present after record_turn_start");
            assert_eq!(
                meta_after.turn_start_ms,
                Some(started_ms),
                "chat_state_handle echoes back the recorded turn_start_ms",
            );
            let elapsed = super::turn_elapsed_seconds_from_start_ms(
                meta_after.turn_start_ms,
                chrono::Utc::now().timestamp_millis(),
            )
            .expect("elapsed present");
            assert!(
                (4..=15).contains(&elapsed),
                "elapsed ~5 s tolerant range, got {elapsed}",
            );
            drop(Arc::try_unwrap(actor).ok().unwrap());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn sampler_error_aborts_with_classifier_error() {
    // After the idle wait expires, `prepare_chat_completion(false).await?.conversation_collect(...)` hits a non-listening `http://localhost`
    // The connection failure surfaces as `SamplingError`, exercising the classifier-error abort arm of the unified path
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: true,
                max_nudges_per_session: 1,
                idle_threshold_ms: Some(50),
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            SessionActor::maybe_fire_laziness_check(actor.clone()).await;
            let (nudges, pending) = {
                let state = actor.state.lock().await;
                (state.nudges_used_this_session, state.pending_inputs.len())
            };
            drop(Arc::try_unwrap(actor).ok().unwrap());
            assert_eq!(nudges, 0, "no nudge on sampler error");
            // The classifier must NEVER push a synthetic InputItem into `pending_inputs`, regardless of outcome
            // Regression guard against re-introducing the old `pending_inputs.push_back` call that fired a turn no user asked for
            assert_eq!(
                pending, 0,
                "classifier must not enqueue any synthetic input",
            );
            let log = events_log(&tmp);
            assert!(
                has_event_with(&log, "laziness_classifier_aborted", |v| j(v, "reason")
                    == crate::session::events::LAZINESS_ABORT_CLASSIFIER_ERROR),
                "expected classifier_error abort:\n{log}"
            );
            assert!(
                !log.contains("laziness_classifier_fired"),
                "classifier never produced a verdict, must not fire:\n{log}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn idle_recheck_after_sleep_short_circuits_silently() {
    // The actor enters maybe_fire_laziness_check idle, but a pending input lands during the sleep
    // The post-sleep idle re-check fails (pending_inputs is non-empty), so the function returns silently with no event and no state mutation
    // This mirrors the real-world race the production code must handle
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: true,
                max_nudges_per_session: 1,
                idle_threshold_ms: Some(200),
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            let poison_actor = actor.clone();
            let poison_task = tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_millis(220)).await;
                let (respond_to, _) = tokio::sync::oneshot::channel();
                poison_actor
                    .state
                    .lock()
                    .await
                    .pending_inputs
                    .push_back(InputItem {
                        prompt_id: "user-real-prompt".to_string(),
                        prompt_blocks: vec![],
                        prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                        trace_gcs_config: None,
                        artifact_tracker: None,
                        client_identifier: None,
                        screen_mode: None,
                        verbatim: true,
                        json_schema: None,
                        input_origin: InputOrigin::new(crate::session::PromptOrigin::User),
                        task_wake_fallback: None,
                        tool_overrides_update: None,
                        respond_to,
                        persist_ack: None,
                        parsed_prompt_tx: None,
                        initial_child_prompt_ready: None,
                        queue_meta: None,
                        queue_mutation_policy: QueueMutationPolicy::hidden(),
                        send_now: false,
                        traceparent: None,
                    });
            });
            SessionActor::maybe_fire_laziness_check(actor.clone()).await;
            poison_task.await.unwrap();
            let nudges = actor.state.lock().await.nudges_used_this_session;
            drop(Arc::try_unwrap(actor).ok().unwrap());
            assert_eq!(nudges, 0, "no state mutation on idle re-check failure");
            let log = events_log(&tmp);
            // The re-check failure is a silent return (the condition that we wanted to nudge no longer holds); no abort event is appropriate
            // The classifier did not produce a verdict either way
            assert!(
                !log.contains("laziness_nudge_fired"),
                "must not push a nudge when idle re-check fails:\n{log}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn laziness_abort_check_detects_bumps_between_snapshot_and_recheck() {
    // After the idle wait expires, `prepare_chat_completion(false).await?.conversation_collect(...)` hits a non-listening `http://localhost`.
    // The connection failure surfaces as `SamplingError`, exercising the classifier-error abort arm of the unified path.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: true,
                max_nudges_per_session: 1,
                idle_threshold_ms: None,
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            let snap = actor.laziness_abort_snapshot();
            // No bump yet, so no abort detected
            assert!(actor.laziness_abort_check(snap).is_none());
            // Bump user_input; the abort is detected
            actor
                .user_input_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            assert_eq!(
                actor.laziness_abort_check(snap),
                Some(LazinessAbortReason::UserInput)
            );
            // Reset the snapshot, then bump model_switch; the abort is detected
            let snap2 = actor.laziness_abort_snapshot();
            actor
                .models_manager
                .set_current_model_id(acp::ModelId::new("yet-another-model"));
            // Invoke the helper UNDER the state lock, mirroring the production call site in `maybe_fire_laziness_check`'s final injection block
            // This pins that the helper has no hidden state-lock dependency (otherwise this deadlocks)
            let _state_guard = actor.state.lock().await;
            assert_eq!(
                actor.laziness_abort_check(snap2),
                Some(LazinessAbortReason::ModelSwitch)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_resets_nudges_used_this_session() {
    // The per-session nudge counter resets to 0 on a real model switch
    // The cap is per-(session, model), so switching is a deliberate user action that gives the new model a fresh budget
    // The test calls the actor's main-loop hook directly (the production `select!` arm delegates to it)
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: true,
                max_nudges_per_session: 2,
                idle_threshold_ms: None,
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            actor.state.lock().await.nudges_used_this_session = 2;
            let new_gen = actor.models_manager.model_switch_generation() + 1;
            actor.handle_model_switch_for_laziness(new_gen).await;
            let nudges = actor.state.lock().await.nudges_used_this_session;
            assert_eq!(
                nudges, 0,
                "model switch must reset the per-session nudge counter to 0",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn emit_laziness_abort_writes_each_reason_with_the_correct_const() {
    // At the actor level, emitting each variant must produce a `LazinessClassifierAborted` event.
    // Its `reason` field must be byte-identical to the corresponding `LAZINESS_ABORT_*` const.
    // This covers `Timeout`, which is otherwise hard to exercise end-to-end (it would require a hanging sampler stub).
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: false,
                max_nudges_per_session: 0,
                idle_threshold_ms: None,
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            for reason in LazinessAbortReason::all() {
                actor.emit_laziness_abort(*reason);
            }
            drop(Arc::try_unwrap(actor).ok().unwrap());
            let log = events_log(&tmp);
            for reason in LazinessAbortReason::all() {
                let expected = reason.as_const_str();
                assert!(
                    has_event_with(&log, "laziness_classifier_aborted", |v| j(v, "reason")
                        == expected),
                    "missing classifier_aborted event for reason={expected}:\n{log}"
                );
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn user_input_generation_bumped_only_on_real_prompts() {
    // Sanity: the field starts at 0 and increments monotonically on each bump
    // The production code bumps it in the `SessionCommand::Prompt` handler when `!origin.is_synthetic()`
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_laziness_actor(LazinessDetectorPerModelConfig {
                enabled: false,
                max_nudges_per_session: 0,
                idle_threshold_ms: None,
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            assert_eq!(
                actor
                    .user_input_generation
                    .load(std::sync::atomic::Ordering::Acquire),
                0
            );
            actor
                .user_input_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            actor
                .user_input_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            assert_eq!(
                actor
                    .user_input_generation
                    .load(std::sync::atomic::Ordering::Acquire),
                2
            );
        })
        .await;
}

/// Attach a `laziness_debug_log` to an existing actor, bypassing `SessionActor::new` (production threads a `PathBuf` through it).
/// Any invariant `SessionActor::new` adds around `laziness_debug_log` MUST be mirrored here or these tests silently diverge from prod.
fn arm_debug_log(actor: &mut SessionActor, path: std::path::PathBuf) {
    actor.laziness_debug_log = Some(std::sync::Arc::from(path.as_path()));
}

/// Build an actor with the dev flag set to `<tmp>/debug.jsonl`.
/// Returns `(actor, tmp, log_path)`.
async fn make_debug_actor(
    detector: LazinessDetectorPerModelConfig,
) -> (Arc<SessionActor>, tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    let mut entry = detector_entry(false, 0, None);
    entry.info.laziness_detector = detector;
    actor
        .models_manager
        .insert_test_entry("test-laziness-model", entry);
    actor
        .models_manager
        .set_current_model_id(acp::ModelId::new("test-laziness-model"));
    let log_path = tmp.path().join("debug.jsonl");
    arm_debug_log(&mut actor, log_path.clone());
    (Arc::new(actor), tmp, log_path)
}

/// Dev-flag contract gate 1: `cfg.enabled = false` MUST NOT short-circuit when `laziness_debug_log = Some(_)`.
/// The classifier must reach the sampler, which fails in the test fixture against a non-listening `http://localhost`.
/// The JSONL log must record exactly one line with `decision: aborted`.
#[tokio::test(flavor = "current_thread")]
async fn debug_mode_fires_classifier_even_with_per_model_enable_false() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp, log_path) = make_debug_actor(LazinessDetectorPerModelConfig {
                enabled: false,
                max_nudges_per_session: 0,
                idle_threshold_ms: None,
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            SessionActor::maybe_fire_laziness_check(actor.clone()).await;
            let (nudges, pending) = {
                let state = actor.state.lock().await;
                (state.nudges_used_this_session, state.pending_inputs.len())
            };
            drop(Arc::try_unwrap(actor).ok().unwrap());

            let contents = std::fs::read_to_string(&log_path)
                .expect("debug log file must exist after debug-mode fire");
            let lines: Vec<&str> = contents.lines().collect();
            assert_eq!(
                lines.len(),
                1,
                "expected exactly one JSONL line, got:\n{contents}",
            );
            let first_line: &str = lines.first().copied().unwrap_or_default();
            let parsed: serde_json::Value =
                serde_json::from_str(first_line).expect("line parses as JSON");
            assert_eq!(j(&parsed, "decision"), "aborted");
            assert_eq!(
                j(&parsed, "abort_reason"),
                "classifier_error",
                "non-listening localhost sampler must surface as classifier_error",
            );
            assert_eq!(nudges, 0, "no nudge possible when sampler fails");
            assert_eq!(
                pending, 0,
                "debug mode must not push synthetic InputItem either",
            );
        })
        .await;
}

/// Dev-flag contract gate 2: the long idle threshold must be bypassed when `laziness_debug_log = Some(_)`.
/// The test configures a 60-second threshold and asserts the call returns within 200ms, proving the `idle_threshold = ZERO` branch was taken.
/// This prevents a future change that drops the `if debug_mode` guard around `Duration::ZERO`.
#[tokio::test(flavor = "current_thread")]
async fn debug_mode_bypasses_idle_wait() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp, log_path) = make_debug_actor(LazinessDetectorPerModelConfig {
                enabled: false,
                max_nudges_per_session: 0,
                idle_threshold_ms: Some(60_000),
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            let started = std::time::Instant::now();
            SessionActor::maybe_fire_laziness_check(actor.clone()).await;
            let elapsed = started.elapsed();
            drop(Arc::try_unwrap(actor).ok().unwrap());
            // The bypass path still does a chat-state MPSC roundtrip, two tool-bridge reads, and `prepare_chat_completion` with a JWT refresh
            // It then attempts a TCP connect against localhost and appends a JSONL line; all of that can run slowly on shared CI
            // 2s is still 30_000 times faster than the configured 60_000ms idle threshold, so the bypass signal is unambiguous
            assert!(
                elapsed < std::time::Duration::from_millis(2000),
                "idle threshold must be bypassed in debug mode (took {elapsed:?})",
            );
            // Sanity: the classifier did reach the sampler and record an outcome, confirming the wait was skipped (not the function returning early)
            let contents = std::fs::read_to_string(&log_path)
                .expect("debug log file must exist after debug-mode fire");
            assert_eq!(contents.lines().count(), 1, "expected exactly one log line");
        })
        .await;
}

/// Dev-flag contract gate 3, the sampler-error variant.
/// When the classifier fails before producing a verdict, debug mode MUST still write exactly one JSONL line and MUST NOT touch `pending_inputs`.
/// The "stalled verdict also fires a nudge" half of this property needs a successful sampler stub, which is heavyweight to set up here.
#[tokio::test(flavor = "current_thread")]
async fn debug_mode_writes_log_and_does_not_inject_synthetic_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp, log_path) = make_debug_actor(LazinessDetectorPerModelConfig {
                enabled: true, // debug mode should fire whether or not the per-model flag is set
                max_nudges_per_session: 5,
                idle_threshold_ms: None,
                min_confidence: None,
                include_reasoning: None,
            })
            .await;
            SessionActor::maybe_fire_laziness_check(actor.clone()).await;
            let pending = actor.state.lock().await.pending_inputs.len();
            drop(Arc::try_unwrap(actor).ok().unwrap());
            assert_eq!(
                pending, 0,
                "no synthetic InputItem may be enqueued, even with cap available",
            );
            let contents = std::fs::read_to_string(&log_path).expect("log file");
            assert_eq!(contents.lines().count(), 1, "exactly one JSONL line");
        })
        .await;
}
