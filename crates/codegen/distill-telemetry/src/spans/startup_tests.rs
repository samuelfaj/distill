use super::*;

mod span_capture {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tracing::span::{Attributes, Id};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::registry::LookupSpan;

    pub(super) struct ClosedSpan {
        pub(super) name: String,
        pub(super) parent: Option<String>,
        pub(super) elapsed: Duration,
    }

    #[derive(Default)]
    pub(super) struct SpanLog {
        open: HashMap<u64, (String, Option<String>, Instant)>,
        pub(super) closed: Vec<ClosedSpan>,
    }

    pub(super) struct SpanTimingLayer(pub(super) Arc<Mutex<SpanLog>>);

    impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for SpanTimingLayer {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
            let mut log = self.0.lock().unwrap();
            let parent = attrs
                .parent()
                .and_then(|pid| log.open.get(&pid.into_u64()))
                .map(|(name, _, _)| name.clone());
            log.open.insert(
                id.into_u64(),
                (attrs.metadata().name().to_string(), parent, Instant::now()),
            );
        }

        fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
            let mut log = self.0.lock().unwrap();
            if let Some((name, parent, opened)) = log.open.remove(&id.into_u64()) {
                let elapsed = opened.elapsed();
                log.closed.push(ClosedSpan {
                    name,
                    parent,
                    elapsed,
                });
            }
        }
    }
}

#[test]
fn startup_phases_emit_spans_with_durations() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset_for_tests();
    crate::unified_log::redirect_to_temp_for_tests();

    let log = Arc::new(Mutex::new(span_capture::SpanLog::default()));
    let subscriber =
        tracing_subscriber::registry().with(span_capture::SpanTimingLayer(Arc::clone(&log)));
    let _guard = tracing::subscriber::set_default(subscriber);

    let _p = begin(Owner::Client);
    enter(StartupPhase::ConfigLoad);
    std::thread::sleep(Duration::from_millis(10));
    // Re-entering the open phase must not open a second span.
    enter(StartupPhase::ConfigLoad);
    enter(StartupPhase::Bootstrap);
    std::thread::sleep(Duration::from_millis(10));
    {
        let mut timer = crate::instrumentation::timer("session.git_divergence");
        timer.with_subphase(Subphase::SessionGitScan);
        std::thread::sleep(Duration::from_millis(10));
    }
    report_total(StartupOutcome::Ok);

    let log = log.lock().unwrap_or_else(|e| e.into_inner());
    let names: Vec<&str> = log.closed.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "startup.config_load",
            "startup.session_git_scan",
            "timer",
            "startup.bootstrap",
            "startup",
        ]
    );
    for c in &log.closed {
        assert!(
            c.elapsed >= Duration::from_millis(10),
            "{} must cover its region, got {:?}",
            c.name,
            c.elapsed
        );
    }

    for c in &log.closed {
        let expected_parent = match c.name.as_str() {
            "startup" => None,
            "timer" => Some("startup.bootstrap"),
            "startup.session_git_scan" => Some("timer"),
            _ => Some("startup"),
        };
        assert_eq!(c.parent.as_deref(), expected_parent, "{}", c.name);
    }

    // The launch-to-interactive bar contains every phase bar.
    let root = log
        .closed
        .iter()
        .find(|c| c.name == "startup")
        .expect("the root startup span closes at the ok total");
    let phase_sum: Duration = log
        .closed
        .iter()
        .filter(|c| c.parent.as_deref() == Some("startup"))
        .map(|c| c.elapsed)
        .sum();
    assert!(
        root.elapsed >= phase_sum,
        "root span ({:?}) must cover the phases it parents ({phase_sum:?})",
        root.elapsed
    );
}

#[tracing::instrument(name = "session.spawn", skip_all, fields(start_type = %start_type))]
async fn fake_spawn_session_actor(start_type: &str) {
    tracing::info_span!("spawn.actor_setup")
        .in_scope(|| std::thread::sleep(Duration::from_millis(2)));
}

fn drive_fake_spawn_under(parent: &tracing::Span) {
    use tracing::Instrument as _;
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let spawn = async {
        fake_spawn_session_actor("new").await;
    };
    rt.block_on(spawn.instrument(parent.clone()));
}

#[test]
fn startup_children_fold_under_their_phase_and_subphase() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset_for_tests();
    crate::unified_log::redirect_to_temp_for_tests();

    let folded = crate::span_profile::test_support::folded_with_layer(|| {
        let _p = begin(Owner::Client);

        enter(StartupPhase::SessionCreate);
        {
            let parent = current_phase_span().expect("session_create phase span is open");
            let _rpc = crate::region!(
                "startup.session_create.backend_rpc",
                crate::region::Parent::Explicit(&parent)
            );
            std::thread::sleep(Duration::from_millis(2));
        }

        {
            let mut timer = crate::instrumentation::timer("session.load_session_replay");
            timer.with_subphase(Subphase::SessionReplay);
            let parent = timer
                .subphase_span()
                .expect("session_replay subphase span is open");
            let read = tracing::info_span!(parent: &parent, "startup.session_replay.read_file");
            read.in_scope(|| std::thread::sleep(Duration::from_millis(2)));
        }

        {
            let mut timer = crate::instrumentation::timer("session.spawn_timer");
            timer.with_subphase(Subphase::SessionSpawn);
            let ctx = SpawnTraceContext::new(timer.subphase_span(), tracing::Span::current());
            drive_fake_spawn_under(&ctx.parent);
        }

        report_total(StartupOutcome::Ok);
    });

    let paths: Vec<&str> = folded
        .lines()
        .filter_map(|l| l.rsplit_once(' ').map(|(p, _)| p))
        .collect();
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("startup.session_create;startup.session_create.backend_rpc")),
        "backend_rpc must fold under startup.session_create:\n{folded}"
    );
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("startup.session_replay;startup.session_replay.read_file")),
        "replay steps must fold under startup.session_replay:\n{folded}"
    );
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("startup.session_spawn;session.spawn;spawn.actor_setup")),
        "session.spawn (and its actor-setup child) must fold under startup.session_spawn:\n{folded}"
    );
}

#[test]
fn spawn_children_fold_under_request_span_when_startup_inactive() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset_for_tests();
    crate::unified_log::redirect_to_temp_for_tests();
    assert!(
        !is_active(),
        "no startup timer begun, so startup must be inactive"
    );

    let folded = crate::span_profile::test_support::folded_with_layer(|| {
        // Not entered: without an ambient parent, nesting must come from the explicit spawn parent.
        let request = tracing::info_span!("acp.request");
        let ctx = SpawnTraceContext::new(None, request.clone());
        assert_eq!(
            ctx.parent.id(),
            request.id(),
            "inactive startup must fall back to the request span"
        );
        drive_fake_spawn_under(&ctx.parent);
        tracing::info_span!(parent: &ctx.parent, "spawn.history_load")
            .in_scope(|| std::thread::sleep(Duration::from_millis(2)));
    });

    let paths: Vec<&str> = folded
        .lines()
        .filter_map(|l| l.rsplit_once(' ').map(|(p, _)| p))
        .collect();
    assert!(
        paths.contains(&"acp.request;session.spawn;spawn.actor_setup"),
        "session.spawn (and its actor-setup child) must nest under the request span:\n{folded}"
    );
    assert!(
        paths.contains(&"acp.request;spawn.history_load"),
        "history_load must nest under the request span:\n{folded}"
    );
    assert!(
        !paths
            .iter()
            .any(|p| p.split(';').next() == Some("session.spawn")),
        "session.spawn must not be a trace root:\n{folded}"
    );
}

#[test]
fn agent_run_spans_close_at_discard() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset_for_tests();
    crate::unified_log::redirect_to_temp_for_tests();

    let log = Arc::new(Mutex::new(span_capture::SpanLog::default()));
    let subscriber =
        tracing_subscriber::registry().with(span_capture::SpanTimingLayer(Arc::clone(&log)));
    let _guard = tracing::subscriber::set_default(subscriber);

    let _p = begin(Owner::Agent);
    enter(StartupPhase::ConfigLoad);
    mark_agent_serving();

    let log = log.lock().unwrap_or_else(|e| e.into_inner());
    let names: Vec<&str> = log.closed.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["startup.config_load", "startup"],
        "a discarded run's spans close at the discard, not at timer drop"
    );
}

// The `phases` string is a frozen format that fleet dashboards parse.
#[test]
fn summary_is_byte_stable_for_fixed_inputs() {
    let snap = PhaseSnapshot {
        completed: vec![
            (StartupPhase::ConfigLoad, Duration::from_millis(12)),
            (StartupPhase::Bootstrap, Duration::from_millis(1500)),
        ],
        open: Some((StartupPhase::SessionCreate, Duration::from_millis(3))),
    };
    assert_eq!(
        snap.summary(),
        "config_load=12ms, bootstrap=1.5s, session_create>=3ms"
    );
}

#[test]
fn over_budget_flags_slow_completed_and_the_open_phase() {
    let snap = PhaseSnapshot {
        completed: vec![
            (StartupPhase::ConfigLoad, Duration::from_millis(20)),
            (StartupPhase::SessionCreate, Duration::from_secs(6)),
        ],
        open: Some((StartupPhase::Bootstrap, Duration::from_secs(30))),
    };
    assert_eq!(
        snap.over_budget(),
        vec![
            (StartupPhase::SessionCreate, Duration::from_secs(6)),
            (StartupPhase::Bootstrap, Duration::from_secs(30)),
        ],
        "flag the slow completed phase and the slow open phase (the one that stalls on timeout), not the fast one",
    );
}

#[test]
fn slow_phase_warning_fires_once_per_open_phase() {
    // `StartupTimer::new` defaults to the exempt `Owner::Agent`.
    let client_timer = || {
        let timer = Arc::new(StartupTimer::new());
        timer.lock().owner = Owner::Client;
        timer
    };
    let timer = client_timer();
    let mut warned = WarnedPhases::default();

    assert!(
        slow_phase_to_warn(&timer, Duration::ZERO, &mut warned).is_none(),
        "no open phase, nothing to warn about",
    );

    timer.enter(StartupPhase::Bootstrap);
    assert!(
        slow_phase_to_warn(&timer, Duration::from_secs(3600), &mut warned).is_none(),
        "a phase within budget stays quiet",
    );
    assert!(matches!(
        slow_phase_to_warn(&timer, Duration::ZERO, &mut warned),
        Some((StartupPhase::Bootstrap, _))
    ));
    assert!(
        slow_phase_to_warn(&timer, Duration::ZERO, &mut warned).is_none(),
        "one warning per phase",
    );

    timer.enter(StartupPhase::SessionCreate);
    assert!(matches!(
        slow_phase_to_warn(&timer, Duration::ZERO, &mut warned),
        Some((StartupPhase::SessionCreate, _))
    ));

    let replacement = client_timer();
    replacement.enter(StartupPhase::Bootstrap);
    assert!(
        slow_phase_to_warn(&replacement, Duration::ZERO, &mut warned).is_some(),
        "a replacement timer warns afresh for the same phase",
    );

    let agent = Arc::new(StartupTimer::new());
    agent.lock().owner = Owner::Agent;
    agent.enter(StartupPhase::Bootstrap);
    assert!(
        slow_phase_to_warn(&agent, Duration::ZERO, &mut warned).is_none(),
        "agent-owned timers idle with a phase open by design",
    );
}

// phase_durations_ms feeds grok_code.startup.phase_duration; it must key a completed and a still-open phase alike.
#[test]
fn phase_durations_ms_keys_completed_and_open_phases() {
    let p = StartupTimer::new();
    p.enter(StartupPhase::ConfigLoad);
    p.enter(StartupPhase::ManagedPolicy);
    p.enter(StartupPhase::ModelCatalog);

    let d = p.phase_durations_ms();
    assert!(
        d.contains_key("config_load") && d.contains_key("model_catalog"),
        "{d:?}"
    );
}

// Process-wide statics: `SERIAL` serializes this with the other global tests; interleaved runs race
#[test]
fn global_lifecycle_records_then_ends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset_for_tests();
    crate::unified_log::redirect_to_temp_for_tests();

    let p = begin(Owner::Client);
    enter(StartupPhase::ManagedPolicy);
    set_auth_mode(AuthMode::Deployment);
    assert_eq!(p.phase_snapshot().stuck_in(), "managed_policy");
    assert_eq!(p.auth_mode().label(), "deployment");
    assert!(
        agent_owned().is_none(),
        "client-owned: agent must not report"
    );

    let p2 = begin(Owner::Client);
    enter(StartupPhase::Bootstrap);
    assert_eq!(p2.phase_snapshot().stuck_in(), "bootstrap");
    assert_eq!(p.phase_snapshot().stuck_in(), "managed_policy");

    drop(crate::instrumentation::timer("startup.mirror_probe_active"));

    let mut git_scan_timer = crate::instrumentation::timer("session.git_divergence");
    git_scan_timer.with_subphase(Subphase::SessionGitScan);
    drop(git_scan_timer);
    record_first_frame();

    enter(StartupPhase::SessionCreate);
    report_total(StartupOutcome::Ok);

    drop(crate::instrumentation::timer("startup.mirror_probe_done"));
    let log = String::from_utf8_lossy(&crate::unified_log::snapshot_log().unwrap_or_default())
        .into_owned();
    assert!(log.contains("startup.mirror_probe_active"), "{log}");
    assert!(
        !log.contains("startup.mirror_probe_done"),
        "done: timers must not mirror, {log}"
    );
    assert!(log.contains("\"session_git_scan_ms\":"), "{log}");
    assert!(log.contains("\"time_to_first_frame_ms\":"), "{log}");

    report_total(StartupOutcome::Ok);
    enter(StartupPhase::ModelCatalog);
    assert_eq!(
        p2.phase_snapshot().stuck_in(),
        "unknown",
        "ok total closes the open phase"
    );
    assert!(p2.summary().contains("session_create="), "{}", p2.summary());
    let p3 = begin(Owner::Agent);
    enter(StartupPhase::ConfigLoad);
    assert_eq!(
        p3.phase_snapshot().stuck_in(),
        "unknown",
        "ended: enter records nothing"
    );

    clear();
    assert!(agent_owned().is_none(), "cleared: nothing installed");

    reset_for_tests();
    mark_utility_process();
    enter(StartupPhase::Bootstrap);
    assert!(agent_owned().is_none(), "utility: nothing records");

    reset_for_tests();
    let p4 = begin(Owner::Client);
    let token = PendingStartup::new();
    enter(StartupPhase::ConfigLoad);
    assert_eq!(p4.phase_snapshot().stuck_in(), "config_load");
    drop(token);
    enter(StartupPhase::Bootstrap);
    assert_eq!(
        p4.phase_snapshot().stuck_in(),
        "config_load",
        "dropped token ended startup"
    );

    record_first_frame();
    let sub = *subphases();
    assert!(
        sub.time_to_first_frame_ms.is_none(),
        "ended startup: draw stamp records nothing"
    );
}

// Absent is not zero: with no prefetch the caller never stamps, so the record omits `prefetch_wait_ms` rather than reporting a spurious zero
#[test]
fn startup_completed_omits_prefetch_wait_without_a_prefetch() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset_for_tests();
    crate::unified_log::redirect_to_temp_for_tests();

    let _p = begin(Owner::Client);
    enter(StartupPhase::ConfigLoad);
    report_total(StartupOutcome::Ok);

    let log = String::from_utf8_lossy(&crate::unified_log::snapshot_log().unwrap_or_default())
        .into_owned();
    assert!(!log.contains("prefetch_wait_ms"), "{log}");
}

// mark_agent_serving finalizes an agent-owned (leader) timer, so a sub-timer
// recorded after it is dropped; the client/embedded path keeps recording.
#[test]
fn sub_timer_after_serving_records_only_on_client_path() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let records_after_serving = |owner| {
        reset_for_tests();
        crate::unified_log::redirect_to_temp_for_tests();
        let timer = begin(owner);
        mark_agent_serving();
        record_sub_timing("startup.acp_initialize.handler", Duration::from_millis(5));
        timer
            .sub_timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(key, _)| key == "acp_initialize.handler")
    };

    assert!(
        !records_after_serving(Owner::Agent),
        "leader path: mark_agent_serving finalizes the timer, later sub-timers drop"
    );
    assert!(
        records_after_serving(Owner::Client),
        "embedded/client path: mark_agent_serving is a no-op, sub-timers still record"
    );
}

// A failed attempt drains its subtimers at emit_telemetry with its own outcome, so a fallback begin()
// cannot re-tag them Cancelled; an Ok attempt leaves them for report_total at the first frame.
#[test]
fn emit_telemetry_drains_failed_attempt_subtimers() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let drained_after = |outcome| {
        reset_for_tests();
        crate::unified_log::redirect_to_temp_for_tests();
        let timer = begin(Owner::Client);
        record_sub_timing("startup.acp_initialize.handler", Duration::from_millis(5));
        timer.emit_telemetry(AgentKind::Embedded, outcome, None, false);
        timer
            .sub_timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    };

    assert!(
        drained_after(StartupOutcome::Timeout),
        "a failed attempt drains its subtimers with its outcome, none left for a fallback begin() to tag Cancelled"
    );
    assert!(
        !drained_after(StartupOutcome::Ok),
        "an Ok attempt leaves its subtimers for report_total at the first frame"
    );
}

// Each attempt owns its sub-timers: a superseded attempt drains once with its own outcome that a later
// begin() cannot re-tag, and record_sub_timing routes by current attempt, not the DONE latch that used to drop it.
#[test]
fn each_attempt_owns_its_subtimers_across_supersede_and_done() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset_for_tests();
    crate::unified_log::redirect_to_temp_for_tests();

    let first = begin(Owner::Client);
    record_sub_timing(
        "startup.acp_initialize.silent_refresh",
        Duration::from_millis(9),
    );
    assert_eq!(
        first
            .sub_timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        1,
        "record_sub_timing routes to the current attempt's own buffer"
    );

    let second = begin(Owner::Client);
    assert!(
        first.sub_timers_drained.load(Ordering::Relaxed),
        "begin() drained the superseded attempt with its own Cancelled outcome"
    );
    assert!(
        first
            .sub_timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty(),
        "the superseded attempt's buffer is emptied by its own drain"
    );

    first.drain_sub_timers(StartupOutcome::Ok);
    assert!(
        first
            .sub_timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty(),
        "a re-drain is a no-op, so a drained attempt cannot be re-tagged"
    );

    DONE.store(true, Ordering::Relaxed);
    record_sub_timing("startup.acp_initialize.handler", Duration::from_millis(3));
    assert_eq!(
        second
            .sub_timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        1,
        "record_sub_timing still lands after DONE flips, routed to the current attempt"
    );
    assert!(
        first
            .sub_timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty(),
        "a superseded attempt never receives another attempt's sub-timers"
    );
}

#[test]
fn record_subphase_routes_each_arm_and_first_frame_first_write_wins() {
    use strum::IntoEnumIterator as _;
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    for sp in Subphase::iter() {
        reset_for_tests();
        record_subphase(sp, Duration::from_millis(7));
        let timings = serde_json::to_value(*subphases()).expect("serialize timings");
        let name: &'static str = sp.into();
        let field = format!("{name}_ms");
        assert_eq!(
            timings.get(&field).and_then(|v| v.as_u64()),
            Some(7),
            "{field} routes to its own field"
        );
        let set = timings
            .as_object()
            .expect("timings object")
            .values()
            .filter(|v| v.as_u64() == Some(7))
            .count();
        assert_eq!(set, 1, "{field} sets exactly one field");
    }

    reset_for_tests();
    record_subphase(Subphase::InitProcess, Duration::from_millis(7));
    record_subphase(Subphase::InitProcess, Duration::from_millis(999));
    assert_eq!(
        subphases().init_process_ms,
        Some(7),
        "init_process keeps its first write across a bootstrap retry"
    );

    reset_for_tests();
    record_subphase(Subphase::SessionLoad, Duration::from_millis(7));
    record_subphase(Subphase::SessionLoad, Duration::from_millis(999));
    assert_eq!(
        subphases().session_load_ms,
        Some(999),
        "session subphases keep the latest write"
    );

    reset_for_tests();
    record_first_frame();
    let first = subphases().time_to_first_frame_ms;
    assert!(first.is_some(), "first frame stamps time_to_first_frame_ms");
    subphases().time_to_first_frame_ms = Some(1);
    record_first_frame();
    assert_eq!(
        subphases().time_to_first_frame_ms,
        Some(1),
        "first write wins"
    );
}

#[test]
fn interactive_frame_records_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    crate::unified_log::redirect_to_temp_for_tests();
    reset_for_tests();
    let _guard = begin(Owner::Client);
    set_auth_mode(AuthMode::Team);
    report_total(StartupOutcome::Ok);

    // Snapshot after the timer has ended, so `appended` is only the interactive record.
    let mark = crate::unified_log::snapshot_log().unwrap_or_default().len();
    assert!(record_interactive_frame(), "first call records");
    assert!(!record_interactive_frame(), "second call is a no-op");

    let log = crate::unified_log::snapshot_log().unwrap_or_default();
    let start = mark.min(log.len());
    let appended = String::from_utf8_lossy(log.get(start..).unwrap_or(&[]));
    assert_eq!(appended.matches(STARTUP_INTERACTIVE_MSG).count(), 1);
    assert!(
        appended.contains("\"startup_total_ms\":"),
        "pairs the total recorded before the frame"
    );
    assert!(
        appended.contains("\"auth_mode\":\"team\""),
        "carries the stashed auth mode past the ended timer"
    );
}
