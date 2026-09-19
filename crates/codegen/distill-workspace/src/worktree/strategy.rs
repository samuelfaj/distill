// Modified for Distill by Samuel Fajreldines, 2026.
//! Build the strategy report from a dispatch outcome plus the request gate,
//! and relay the daemon's redirect telemetry ring to the host's `log_event`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use distill_fast_worktree::{
    ArmSkip, GroveHardFail, GroveSkip, NfsWorktreeClient, NfsWorktreeOpts, WorktreeReport,
};
use distill_telemetry::events::{
    CloneCancellationDisposition, CloneDaemonCapabilityClass, CloneFallbackReason, CloneOutcome,
    CloneStrategy, CloneTransport, RedirectEvent, WorktreeEnded, WorktreeLifecycle,
};
use distill_telemetry::session_ctx::log_event;
use distill_workspace_types::rpc::worktree::{
    StrategyReport, WorktreeType, is_grove_resolved, transport_for_resolved,
};

/// Budget for one whole drain. The fetch gets all of it and the ack gets what
/// is left, but `NfsWorktreeClient::call` applies the value it is handed to
/// connect, write, and read separately, so a daemon that accepts and stalls
/// costs at most about three times the remaining budget per call before the
/// ack is skipped.
const REDIRECT_EVENTS_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct WorktreeEndedEmit<'a> {
    pub lifecycle: WorktreeLifecycle,
    pub outcome: CloneOutcome,
    pub duration_ms: u64,
    pub grove_enabled: bool,
    pub grove_gate_source: Option<&'a str>,
    pub requested_type: WorktreeType,
    pub rewrite_reason: Option<&'a str>,
    pub worktree: Option<&'a WorktreeReport>,
    pub cancellation_disposition: Option<CloneCancellationDisposition>,
    pub builder_failure: Option<CloneFallbackReason>,
}

pub(super) fn emit_worktree_ended(emit: WorktreeEndedEmit<'_>) -> WorktreeEnded {
    let report = emit.worktree.map(|worktree| {
        report_from_worktree(
            emit.grove_enabled,
            emit.grove_gate_source,
            emit.requested_type,
            emit.rewrite_reason,
            worktree,
        )
    });
    let requested = report
        .as_ref()
        .and_then(|r| r.requested_strategy.as_deref())
        .unwrap_or_else(|| {
            requested_strategy(
                emit.grove_enabled,
                emit.grove_gate_source,
                emit.requested_type,
            )
        });
    let event = WorktreeEnded {
        lifecycle: emit.lifecycle,
        duration_ms: emit.duration_ms,
        outcome: emit.outcome,
        transport: report
            .as_ref()
            .and_then(|r| r.transport.as_deref())
            .and_then(CloneTransport::from_transport_str),
        requested_strategy: CloneStrategy::from_strategy_str(requested),
        resolved_strategy: report
            .as_ref()
            .and_then(|r| r.resolved_strategy.as_deref())
            .and_then(CloneStrategy::from_strategy_str),
        fallback_reason: classify_fallback(
            emit.grove_enabled,
            emit.grove_gate_source,
            emit.worktree.map(|w| w.resolved_strategy),
            emit.rewrite_reason,
            emit.worktree.map(|w| w.skipped.as_slice()).unwrap_or(&[]),
            emit.builder_failure,
        ),
        cancellation_disposition: emit.cancellation_disposition,
        daemon_capability_class: report
            .as_ref()
            .and_then(|r| r.daemon_capability_class.as_deref())
            .and_then(CloneDaemonCapabilityClass::from_class_str),
    };
    log_event(event.clone());
    drain_redirect_events(&event);
    #[cfg(test)]
    LAST_WORKTREE_ENDED.with(|slot| {
        *slot.borrow_mut() = Some(event.clone());
    });
    event
}

/// Set while one drain is in flight, so concurrent worktree ends in one
/// process do not both fetch and log the same unacked entries.
static REDIRECT_DRAIN_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Holds `REDIRECT_DRAIN_IN_FLIGHT`. Releasing on `Drop` keeps the flag from
/// wedging shut when tokio drops the spawned closure unrun at runtime shutdown
/// or when the drain panics.
#[must_use]
struct RedirectDrainGuard;

impl RedirectDrainGuard {
    fn try_acquire() -> Option<Self> {
        REDIRECT_DRAIN_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(RedirectDrainGuard)
    }
}

impl Drop for RedirectDrainGuard {
    fn drop(&mut self) {
        REDIRECT_DRAIN_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// worker thread. Only a grove-backed worktree talked to the daemon, and a
/// host with telemetry off must not ack entries a telemetry-enabled peer
/// sharing the daemon would have logged.
pub(crate) fn drain_redirect_events(event: &WorktreeEnded) {
    if event.transport.is_none()
        || !distill_telemetry::is_enabled()
        || tokio::runtime::Handle::try_current().is_err()
    {
        return;
    }
    let Some(guard) = RedirectDrainGuard::try_acquire() else {
        return;
    };
    let opts = NfsWorktreeOpts::default();
    #[cfg(test)]
    let opts = NfsWorktreeOpts {
        control_sock: REDIRECT_DRAIN_CONTROL_SOCK.with(|slot| slot.borrow().clone()),
        ..opts
    };
    // The ring is daemon-wide, so its entries carry no session id; the
    // blocking thread has no task-local session context and none is wanted.
    tokio::task::spawn_blocking(move || {
        let _in_flight = guard;
        drain_redirect_events_sync(&NfsWorktreeClient::from_opts(&opts));
    });
}

/// Blocking drain, decode, log, then ack through the last seq seen. Returns
/// `(logged, undecodable)`. Delivery is at least once, since the ring keeps an
/// entry until acked and a host that dies mid-translation, or a second host
/// sharing the daemon, logs it again. Undecodable entries are acked too,
/// because the host cannot use them and leaving them would block the ring
/// forever.
fn drain_redirect_events_sync(client: &NfsWorktreeClient) -> (usize, usize) {
    // Each call receives the time left on this deadline and applies it per
    // socket operation, so the deadline bounds the ack's start, not the total.
    let deadline = Instant::now() + REDIRECT_EVENTS_DRAIN_TIMEOUT;
    let (values, next_seq) = match client.redirect_events(0, REDIRECT_EVENTS_DRAIN_TIMEOUT) {
        Ok(reply) => reply,
        Err(_) => {
            tracing::debug!("redirect telemetry drain skipped; daemon unreachable or too old");
            return (0, 0);
        }
    };
    if values.is_empty() {
        return (0, 0);
    }
    let mut logged = 0;
    let mut undecodable = 0;
    for value in values {
        match serde_json::from_value::<RedirectEvent>(value) {
            Ok(event) => {
                log_redirect_event(event);
                logged += 1;
            }
            // The serde message would echo daemon-supplied text; the count is enough.
            Err(_) => undecodable += 1,
        }
    }
    tracing::debug!(logged, undecodable, "redirect telemetry drained");
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        tracing::debug!("redirect telemetry ack skipped; drain budget spent, entries stay queued");
    } else if client
        .redirect_events(next_seq.saturating_sub(1), remaining)
        .is_err()
    {
        tracing::debug!("redirect telemetry ack failed; entries stay queued");
    }
    (logged, undecodable)
}

fn log_redirect_event(event: RedirectEvent) {
    #[cfg(test)]
    LOGGED_REDIRECT_EVENTS.with(|slot| slot.borrow_mut().push(event.clone()));
    match event {
        RedirectEvent::Applied(applied) => log_event(applied),
        RedirectEvent::FixupFailed(failed) => log_event(failed),
        RedirectEvent::Demoted(demoted) => log_event(demoted),
        RedirectEvent::Overwrite(overwrite) => log_event(overwrite),
        RedirectEvent::LimitHit(hit) => log_event(hit),
    }
}

fn classify_fallback(
    grove_enabled: bool,
    grove_gate_source: Option<&str>,
    resolved: Option<&str>,
    rewrite_reason: Option<&str>,
    skipped: &[ArmSkip],
    builder_failure: Option<CloneFallbackReason>,
) -> Option<CloneFallbackReason> {
    if !grove_enabled {
        return match grove_gate_source {
            Some("remote_kill") => Some(CloneFallbackReason::RemoteKill),
            _ => None,
        };
    }
    if builder_failure.is_some() {
        return builder_failure;
    }
    if resolved.is_some_and(is_grove_resolved) {
        return None;
    }
    if let Some(skip) = skipped.iter().find(|s| s.arm.is_grove()) {
        return Some(match skip.grove_skip {
            Some(grove) => map_grove_skip(grove),
            None => CloneFallbackReason::Other,
        });
    }
    if rewrite_reason == Some(distill_fast_worktree::SKIP_SOURCE_IS_GROVE_MOUNT) {
        return Some(CloneFallbackReason::SourceIsGrove);
    }
    None
}

pub(super) fn emit_failed_without_report(
    lifecycle: WorktreeLifecycle,
    grove_enabled: bool,
    grove_gate_source: Option<&str>,
    requested_type: WorktreeType,
) {
    emit_worktree_ended(WorktreeEndedEmit {
        lifecycle,
        outcome: CloneOutcome::Failed,
        duration_ms: 0,
        grove_enabled,
        grove_gate_source,
        requested_type,
        rewrite_reason: None,
        worktree: None,
        cancellation_disposition: None,
        builder_failure: None,
    });
}

pub(super) fn fallback_from_builder_err(err: &anyhow::Error) -> Option<CloneFallbackReason> {
    match distill_fast_worktree::grove_hard_fail(err) {
        Some(GroveHardFail::InFlight) => Some(CloneFallbackReason::InFlight),
        Some(
            GroveHardFail::StorageFull
            | GroveHardFail::IdentityConflict
            | GroveHardFail::DestStillMounted,
        ) => Some(CloneFallbackReason::Other),
        None => None,
    }
}

fn map_grove_skip(skip: GroveSkip) -> CloneFallbackReason {
    match skip {
        #[cfg(target_os = "linux")]
        GroveSkip::FuseUnavailable => CloneFallbackReason::FuseUnavailable,
        #[cfg(target_os = "linux")]
        GroveSkip::PrivateMountNamespace => CloneFallbackReason::Other,
        #[cfg(windows)]
        GroveSkip::ProjfsUnavailable => CloneFallbackReason::ProjfsUnavailable,
        GroveSkip::SourceIsGroveMount => CloneFallbackReason::SourceIsGrove,
        GroveSkip::DaemonDeclined => CloneFallbackReason::DaemonDeclined,
        GroveSkip::PreserveOnLinkedView
        | GroveSkip::MountTableInconclusive
        | GroveSkip::PreserveOnInconclusiveLinkedView
        | GroveSkip::JjSourceRepo
        | GroveSkip::PreserveNonHeadRef
        | GroveSkip::HeadUnreadableAfterAdopt => CloneFallbackReason::Other,
    }
}

#[cfg(test)]
std::thread_local! {
    static LAST_WORKTREE_ENDED: std::cell::RefCell<Option<WorktreeEnded>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn last_worktree_ended_for_test() -> Option<WorktreeEnded> {
    LAST_WORKTREE_ENDED.with(|slot| slot.borrow_mut().take())
}

#[cfg(test)]
std::thread_local! {
    static LOGGED_REDIRECT_EVENTS: std::cell::RefCell<Vec<RedirectEvent>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn take_logged_redirect_events_for_test() -> Vec<RedirectEvent> {
    LOGGED_REDIRECT_EVENTS.with(|slot| std::mem::take(&mut *slot.borrow_mut()))
}

// Where a test points the drain spawned by `emit_worktree_ended`, so a gate
// regression is observed on a fake socket instead of the host's daemon.
#[cfg(test)]
std::thread_local! {
    static REDIRECT_DRAIN_CONTROL_SOCK: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) fn report_from_worktree(
    grove_enabled: bool,
    grove_gate_source: Option<&str>,
    requested_type: WorktreeType,
    rewrite_reason: Option<&str>,
    report: &WorktreeReport,
) -> StrategyReport {
    let requested = requested_strategy(grove_enabled, grove_gate_source, requested_type);
    let resolved = report.resolved_strategy;
    let grove_transport = report
        .strategy_metadata
        .as_ref()
        .and_then(|m| m.get("grove"))
        .and_then(|g| g.get("transport"))
        .and_then(|v| v.as_str());
    StrategyReport {
        requested_strategy: Some(requested.into()),
        resolved_strategy: Some(resolved.into()),
        transport: transport_for_resolved(resolved, grove_transport).map(str::to_owned),
        // The adopt reply carries no object source; only the clone path knows it.
        source_mode: None,
        fallback_reason: fallback_reason(
            grove_enabled,
            grove_gate_source,
            resolved,
            rewrite_reason,
            &report.skipped,
        ),
        daemon_capability_class: report.daemon_capability_class.map(str::to_owned),
    }
}

fn requested_strategy(
    grove_enabled: bool,
    grove_gate_source: Option<&str>,
    requested_type: WorktreeType,
) -> &'static str {
    if grove_enabled || grove_gate_source == Some("remote_kill") {
        return "grove";
    }
    match requested_type {
        WorktreeType::Linked => "linked",
        WorktreeType::Standalone => "standalone",
        WorktreeType::Git => "git",
    }
}

fn fallback_reason(
    grove_enabled: bool,
    grove_gate_source: Option<&str>,
    resolved: &str,
    rewrite_reason: Option<&str>,
    skipped: &[ArmSkip],
) -> Option<String> {
    if !grove_enabled {
        return match grove_gate_source {
            Some("remote_kill") => Some("remote Grove is off".into()),
            _ => None,
        };
    }
    if is_grove_resolved(resolved) {
        return None;
    }
    // Only a grove skip explains why grove did not run: a snapshot arm's skip
    // means an earlier arm won and grove was never reached.
    skipped
        .iter()
        .find(|s| s.arm.is_grove())
        .map(ArmSkip::to_string)
        // A pre-dispatch rewrite kept every arm from running, so it is the only
        // account of why grove did not serve this worktree.
        .or_else(|| rewrite_reason.map(str::to_owned))
}

pub(super) fn creating_progress(grove_enabled: bool) -> &'static str {
    if grove_enabled {
        "Creating worktree with Grove..."
    } else {
        "Creating worktree..."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use distill_fast_worktree::{CopyReport, WorktreeArm};

    fn report(
        resolved: &'static str,
        skipped: Vec<ArmSkip>,
        daemon: Option<&'static str>,
    ) -> WorktreeReport {
        WorktreeReport {
            worktree_path: PathBuf::from("/wt"),
            commit: "abc".into(),
            unignored_copy: CopyReport::default(),
            ignored_copy: None,
            resolved_strategy: resolved,
            strategy_metadata: None,
            skipped,
            daemon_capability_class: daemon,
        }
    }

    #[test]
    fn grove_success_and_copy_fallback_print_the_right_reason() {
        let ok = report_from_worktree(true, Some("request"), WorktreeType::Linked, None, &{
            let mut r = report("grove-fuse", Vec::new(), Some("current"));
            r.strategy_metadata = Some(serde_json::json!({"grove":{"transport":"fuse"}}));
            r
        });
        assert_eq!(ok.requested_strategy.as_deref(), Some("grove"));
        assert_eq!(ok.resolved_strategy.as_deref(), Some("grove-fuse"));
        assert_eq!(ok.transport.as_deref(), Some("fuse"));
        assert_eq!(ok.source_mode, None);
        assert!(ok.fallback_reason.is_none());
        assert_eq!(ok.summary(), "Requested Grove; using `grove-fuse`.");

        let copy = report_from_worktree(
            false,
            Some("remote_kill"),
            WorktreeType::Linked,
            None,
            &report("copy", Vec::new(), Some("unknown")),
        );
        assert_eq!(copy.requested_strategy.as_deref(), Some("grove"));
        assert_eq!(copy.resolved_strategy.as_deref(), Some("copy"));
        assert_eq!(copy.fallback_reason.as_deref(), Some("remote Grove is off"));
        assert_eq!(
            copy.summary(),
            "Requested Grove; using copy because remote Grove is off."
        );

        let skipped = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "copy",
                vec![ArmSkip::new(
                    WorktreeArm::GroveFuse,
                    "/dev/fuse or fusermount missing",
                )],
                Some("unknown"),
            ),
        );
        assert_eq!(
            skipped.fallback_reason.as_deref(),
            Some("grove-fuse: /dev/fuse or fusermount missing")
        );
        assert!(skipped.summary().contains("grove-fuse:"));

        let old_and_skip = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "copy",
                vec![ArmSkip::new(
                    WorktreeArm::GroveFuse,
                    "/dev/fuse or fusermount missing",
                )],
                Some("old"),
            ),
        );
        assert_eq!(
            old_and_skip.fallback_reason.as_deref(),
            Some("grove-fuse: /dev/fuse or fusermount missing")
        );
        assert!(old_and_skip.summary().contains("grove-fuse:"));
        assert!(!old_and_skip.summary().contains("daemon too old"));

        let overlay = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report("overlay", Vec::new(), Some("old")),
        );
        assert_eq!(overlay.resolved_strategy.as_deref(), Some("overlay"));
        assert_eq!(overlay.daemon_capability_class.as_deref(), Some("old"));
        assert!(overlay.fallback_reason.is_none());
        assert_eq!(overlay.summary(), "Requested Grove; using overlay.");
    }

    #[test]
    fn earlier_arm_skip_lines_are_not_a_grove_fallback_reason() {
        let btrfs_won = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "btrfs",
                vec![ArmSkip::new(WorktreeArm::Overlay, "mount failed: EPERM")],
                Some("current"),
            ),
        );
        assert!(
            btrfs_won.fallback_reason.is_none(),
            "grove never ran, so an overlay error cannot explain its absence: {:?}",
            btrfs_won.fallback_reason
        );
        assert_eq!(btrfs_won.summary(), "Requested Grove; using btrfs.");

        let copy_after_grove = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "copy",
                vec![
                    ArmSkip::new(WorktreeArm::Overlay, "mount failed: EPERM"),
                    ArmSkip::new(WorktreeArm::GroveFuse, "daemon declined or unreachable"),
                ],
                Some("current"),
            ),
        );
        assert_eq!(
            copy_after_grove.fallback_reason.as_deref(),
            Some("grove-fuse: daemon declined or unreachable")
        );
    }

    #[test]
    fn a_pre_dispatch_rewrite_still_explains_itself() {
        // to git before dispatch: no arm ran, and no arm recorded a skip.
        let rewritten = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            Some(distill_fast_worktree::SKIP_SOURCE_IS_GROVE_MOUNT),
            &report("git", Vec::new(), None),
        );
        assert_eq!(
            rewritten.summary(),
            "Requested Grove; using git because source is itself a Grove mount."
        );

        // A grove skip line still wins: it is the arm's own account.
        let both = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            Some(distill_fast_worktree::SKIP_SOURCE_IS_GROVE_MOUNT),
            &report(
                "copy",
                vec![ArmSkip::new(
                    WorktreeArm::GroveFuse,
                    "the source is a jj repo",
                )],
                None,
            ),
        );
        assert_eq!(
            both.fallback_reason.as_deref(),
            Some("grove-fuse: the source is a jj repo")
        );
    }

    #[test]
    fn notice_is_silent_for_an_ordinary_copy_worktree() {
        let plain = report_from_worktree(
            false,
            Some("default"),
            WorktreeType::Linked,
            None,
            &report("copy", Vec::new(), None),
        );
        assert_eq!(plain.summary(), "Using copy.");
        assert_eq!(plain.notice(), None);

        let grove = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "copy",
                vec![ArmSkip::new(
                    WorktreeArm::GroveFuse,
                    "private mount namespace",
                )],
                None,
            ),
        );
        assert_eq!(
            grove.notice().as_deref(),
            Some("Requested Grove; using copy because grove-fuse: private mount namespace.")
        );
    }

    fn emit_from(
        lifecycle: WorktreeLifecycle,
        outcome: CloneOutcome,
        grove_enabled: bool,
        gate: Option<&str>,
        rewrite: Option<&str>,
        worktree: Option<&WorktreeReport>,
        cancellation: Option<CloneCancellationDisposition>,
    ) -> WorktreeEnded {
        emit_worktree_ended(WorktreeEndedEmit {
            lifecycle,
            outcome,
            duration_ms: 9,
            grove_enabled,
            grove_gate_source: gate,
            requested_type: WorktreeType::Linked,
            rewrite_reason: rewrite,
            worktree,
            cancellation_disposition: cancellation,
            builder_failure: None,
        })
    }

    #[test]
    fn worktree_ended_success_has_strategy_transport_daemon_and_no_fallback() {
        let mut wt = report("grove-fuse", Vec::new(), Some("current"));
        wt.strategy_metadata = Some(serde_json::json!({"grove":{"transport":"fuse"}}));
        let event = emit_from(
            WorktreeLifecycle::Create,
            CloneOutcome::Success,
            true,
            Some("request"),
            None,
            Some(&wt),
            None,
        );
        assert_eq!(event.lifecycle, WorktreeLifecycle::Create);
        assert_eq!(event.outcome, CloneOutcome::Success);
        assert_eq!(event.requested_strategy, Some(CloneStrategy::Grove));
        assert_eq!(event.resolved_strategy, Some(CloneStrategy::GroveFuse));
        assert_eq!(event.transport, Some(CloneTransport::Fuse));
        assert_eq!(
            event.daemon_capability_class,
            Some(CloneDaemonCapabilityClass::Current)
        );
        assert_eq!(event.fallback_reason, None);
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("fallback_reason").is_none());
        assert!(json.get("source_mode").is_none());
    }

    #[test]
    fn worktree_ended_grove_to_copy_uses_closed_fallback_not_skip_text() {
        let wt = report(
            "copy",
            vec![ArmSkip::from_grove(
                WorktreeArm::GroveFuse,
                GroveSkip::DaemonDeclined,
            )],
            Some("current"),
        );
        let event = emit_from(
            WorktreeLifecycle::Create,
            CloneOutcome::Success,
            true,
            Some("request"),
            None,
            Some(&wt),
            None,
        );
        assert_eq!(
            event.fallback_reason,
            Some(CloneFallbackReason::DaemonDeclined)
        );
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("http"), "{text}");
        assert!(!text.contains("url"), "{text}");
        assert!(!text.contains("repo"), "{text}");
        assert!(!text.contains("/dev/fuse"), "{text}");
        assert!(!text.contains("declined or unreachable"), "{text}");
        assert!(!text.contains("grove-fuse:"), "{text}");
    }

    #[test]
    fn worktree_ended_remote_kill_is_typed() {
        let event = emit_from(
            WorktreeLifecycle::Create,
            CloneOutcome::Success,
            false,
            Some("remote_kill"),
            None,
            Some(&report("copy", Vec::new(), Some("unknown"))),
            None,
        );
        assert_eq!(event.requested_strategy, Some(CloneStrategy::Grove));
        assert_eq!(event.resolved_strategy, Some(CloneStrategy::Copy));
        assert_eq!(event.fallback_reason, Some(CloneFallbackReason::RemoteKill));
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("remote Grove is off"), "{text}");
    }

    #[test]
    fn worktree_ended_fork_cancel_records_disposition() {
        let event = emit_from(
            WorktreeLifecycle::Fork,
            CloneOutcome::Cancelled,
            true,
            Some("request"),
            None,
            Some(&report("grove-fuse", Vec::new(), Some("current"))),
            Some(CloneCancellationDisposition::ClientCancelled),
        );
        assert_eq!(event.lifecycle, WorktreeLifecycle::Fork);
        assert_eq!(event.outcome, CloneOutcome::Cancelled);
        assert_eq!(
            event.cancellation_disposition,
            Some(CloneCancellationDisposition::ClientCancelled)
        );
    }

    #[test]
    fn worktree_ended_untyped_grove_skip_is_other_and_drops_detail() {
        let wt = report(
            "copy",
            vec![ArmSkip::new(
                WorktreeArm::GroveFuse,
                "/dev/fuse or fusermount missing",
            )],
            Some("unknown"),
        );
        let event = emit_from(
            WorktreeLifecycle::Create,
            CloneOutcome::Success,
            true,
            Some("request"),
            None,
            Some(&wt),
            None,
        );
        assert_eq!(event.fallback_reason, Some(CloneFallbackReason::Other));
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("/dev/fuse"), "{text}");
        assert!(!text.contains("fusermount"), "{text}");
    }

    #[test]
    fn worktree_ended_source_is_grove_rewrite_is_typed() {
        let event = emit_from(
            WorktreeLifecycle::Create,
            CloneOutcome::Success,
            true,
            Some("request"),
            Some(distill_fast_worktree::SKIP_SOURCE_IS_GROVE_MOUNT),
            Some(&report("git", Vec::new(), None)),
            None,
        );
        assert_eq!(
            event.fallback_reason,
            Some(CloneFallbackReason::SourceIsGrove)
        );
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("source is itself"), "{text}");
    }

    #[test]
    fn worktree_ended_source_is_grove_skip_variant_is_typed() {
        let wt = report(
            "copy",
            vec![ArmSkip::from_grove(
                WorktreeArm::GroveFuse,
                GroveSkip::SourceIsGroveMount,
            )],
            None,
        );
        let event = emit_from(
            WorktreeLifecycle::Create,
            CloneOutcome::Success,
            true,
            Some("request"),
            None,
            Some(&wt),
            None,
        );
        assert_eq!(
            event.fallback_reason,
            Some(CloneFallbackReason::SourceIsGrove)
        );
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("source is itself"), "{text}");
        assert!(!text.contains("Grove mount"), "{text}");
        assert!(!text.contains("grove-fuse:"), "{text}");
    }

    #[test]
    fn worktree_ended_failed_without_report_omits_resolved_fields() {
        let event = emit_from(
            WorktreeLifecycle::Create,
            CloneOutcome::Failed,
            true,
            Some("request"),
            None,
            None,
            None,
        );
        assert_eq!(event.requested_strategy, Some(CloneStrategy::Grove));
        assert_eq!(event.resolved_strategy, None);
        assert_eq!(event.transport, None);
        assert_eq!(event.fallback_reason, None);
        assert_eq!(event.daemon_capability_class, None);
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("resolved_strategy").is_none());
        assert!(json.get("transport").is_none());
        assert!(json.get("fallback_reason").is_none());
        assert!(json.get("daemon_capability_class").is_none());
        assert!(json.get("cancellation_disposition").is_none());
    }

    #[test]
    fn worktree_ended_builder_in_flight_is_typed() {
        let event = emit_worktree_ended(WorktreeEndedEmit {
            lifecycle: WorktreeLifecycle::Create,
            outcome: CloneOutcome::Failed,
            duration_ms: 4,
            grove_enabled: true,
            grove_gate_source: Some("request"),
            requested_type: WorktreeType::Linked,
            rewrite_reason: None,
            worktree: None,
            cancellation_disposition: None,
            builder_failure: Some(CloneFallbackReason::InFlight),
        });
        assert_eq!(event.fallback_reason, Some(CloneFallbackReason::InFlight));
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("in flight"), "{text}");
        assert!(!text.contains("phase"), "{text}");
    }

    /// Answers each connection with the next canned reply (an `err` once they
    /// run out) and returns every request seen once the `stop` op arrives.
    #[cfg(unix)]
    fn fake_daemon(
        sock: &std::path::Path,
        replies: Vec<&'static str>,
    ) -> std::thread::JoinHandle<Vec<serde_json::Value>> {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::os::unix::net::UnixListener::bind(sock).unwrap();
        std::thread::spawn(move || {
            let mut replies = replies.into_iter();
            let mut requests = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                if request.get("op").and_then(|v| v.as_str()) == Some("stop") {
                    return requests;
                }
                requests.push(request);
                let reply = replies
                    .next()
                    .unwrap_or(r#"{"status":"err","data":{"v":1,"error":"unexpected call"}}"#);
                writeln!(stream, "{reply}").unwrap();
            }
        })
    }

    #[cfg(unix)]
    fn stop_fake_daemon(sock: &std::path::Path) {
        use std::io::Write;
        let mut stream = std::os::unix::net::UnixStream::connect(sock).unwrap();
        writeln!(stream, r#"{{"op":"stop"}}"#).unwrap();
    }

    #[cfg(unix)]
    fn client_for(sock: &std::path::Path) -> NfsWorktreeClient {
        NfsWorktreeClient::from_opts(&NfsWorktreeOpts {
            control_sock: Some(sock.to_path_buf()),
            runtime_dir: Some(sock.parent().unwrap().to_path_buf()),
            ..NfsWorktreeOpts::default()
        })
    }

    #[cfg(unix)]
    #[test]
    fn drain_translates_ring_events_to_telemetry_structs_and_acks() {
        use distill_telemetry::events::{
            LimitDisposition, RedirectDemoteReason, RedirectDemoted, RedirectLimitHit,
            RedirectLimitKind, RedirectMechanismKind, RedirectTransport,
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = tmp.path().join("control.sock");
        let daemon = fake_daemon(
            &sock,
            vec![
                concat!(
                    r#"{"status":"ok","data":{"v":1,"redirect_events":["#,
                    r#"{"seq":5,"event":"redirect_demoted","transport":"fuse","mechanism":"bind","reason":"shutdown","demote_ms":5},"#,
                    r#"{"seq":6,"event":"redirect_limit_hit","limit_kind":"user_entries","limit":64,"observed":65,"disposition":"rejected"},"#,
                    r#"{"seq":7,"event":"redirect_from_a_newer_daemon","transport":"fuse"}"#,
                    r#"],"redirect_next_seq":8}}"#
                ),
                r#"{"status":"ok","data":{"v":1,"redirect_next_seq":8}}"#,
            ],
        );
        take_logged_redirect_events_for_test();

        let report = drain_redirect_events_sync(&client_for(&sock));
        stop_fake_daemon(&sock);

        assert_eq!((2, 1), report);
        assert_eq!(
            vec![
                RedirectEvent::Demoted(RedirectDemoted {
                    transport: RedirectTransport::Fuse,
                    mechanism: RedirectMechanismKind::Bind,
                    reason: RedirectDemoteReason::Shutdown,
                    demote_ms: 5,
                }),
                RedirectEvent::LimitHit(RedirectLimitHit {
                    limit_kind: RedirectLimitKind::UserEntries,
                    limit: 64,
                    observed: 65,
                    disposition: LimitDisposition::Rejected,
                }),
            ],
            take_logged_redirect_events_for_test()
        );
        assert_eq!(
            vec![
                serde_json::json!({"op":"redirect_events","v":1,"ack_through":0}),
                serde_json::json!({"op":"redirect_events","v":1,"ack_through":7}),
            ],
            daemon.join().unwrap(),
            "undecodable entries are acked and dropped, not retried"
        );
    }

    #[cfg(unix)]
    #[test]
    fn drain_still_reports_logged_events_when_the_ack_fails() {
        use distill_telemetry::events::{
            RedirectDemoteReason, RedirectDemoted, RedirectMechanismKind, RedirectTransport,
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = tmp.path().join("control.sock");
        let daemon = fake_daemon(
            &sock,
            vec![concat!(
                r#"{"status":"ok","data":{"v":1,"redirect_events":["#,
                r#"{"seq":1,"event":"redirect_demoted","transport":"fuse","mechanism":"bind","reason":"cleanup","demote_ms":1},"#,
                r#"{"seq":2,"event":"redirect_demoted","transport":"nfs","mechanism":"image","reason":"convert","demote_ms":2}"#,
                r#"],"redirect_next_seq":3}}"#
            )],
        );
        take_logged_redirect_events_for_test();

        let report = drain_redirect_events_sync(&client_for(&sock));
        stop_fake_daemon(&sock);

        assert_eq!((2, 0), report);
        assert_eq!(
            vec![
                RedirectEvent::Demoted(RedirectDemoted {
                    transport: RedirectTransport::Fuse,
                    mechanism: RedirectMechanismKind::Bind,
                    reason: RedirectDemoteReason::Cleanup,
                    demote_ms: 1,
                }),
                RedirectEvent::Demoted(RedirectDemoted {
                    transport: RedirectTransport::Nfs,
                    mechanism: RedirectMechanismKind::Image,
                    reason: RedirectDemoteReason::Convert,
                    demote_ms: 2,
                }),
            ],
            take_logged_redirect_events_for_test()
        );
        assert_eq!(
            vec![
                serde_json::json!({"op":"redirect_events","v":1,"ack_through":0}),
                serde_json::json!({"op":"redirect_events","v":1,"ack_through":2}),
            ],
            daemon.join().unwrap(),
            "the ack is attempted once and its failure is swallowed"
        );
    }

    /// Both tests that observe `REDIRECT_DRAIN_IN_FLIGHT` hold this, since the
    /// flag is process-wide and the test binary runs tests in parallel.
    static IN_FLIGHT_FLAG_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Telemetry is never initialized in a test binary, so the gate in
    /// `drain_redirect_events` must keep `emit_worktree_ended` off the socket.
    #[cfg(unix)]
    #[test]
    fn emit_worktree_ended_with_telemetry_disabled_makes_no_connection() {
        let _serial = IN_FLIGHT_FLAG_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(!distill_telemetry::is_enabled());
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = tmp.path().join("control.sock");
        let daemon = fake_daemon(
            &sock,
            vec![r#"{"status":"ok","data":{"v":1,"redirect_next_seq":1}}"#],
        );
        REDIRECT_DRAIN_CONTROL_SOCK.with(|slot| *slot.borrow_mut() = Some(sock.clone()));
        let mut wt = report("grove-fuse", Vec::new(), Some("current"));
        wt.strategy_metadata = Some(serde_json::json!({"grove":{"transport":"fuse"}}));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let event = runtime.block_on(async {
            assert!(tokio::runtime::Handle::try_current().is_ok());
            assert!(!REDIRECT_DRAIN_IN_FLIGHT.load(Ordering::Acquire));
            emit_from(
                WorktreeLifecycle::Create,
                CloneOutcome::Success,
                true,
                Some("request"),
                None,
                Some(&wt),
                None,
            )
        });
        // Dropping the runtime waits for every blocking task, so a drain that
        // slipped past the gate has connected by the time the daemon stops.
        drop(runtime);
        assert_eq!(
            Some(sock.clone()),
            REDIRECT_DRAIN_CONTROL_SOCK.with(|slot| slot.borrow().clone()),
            "the override is read on this thread, not consumed elsewhere"
        );
        REDIRECT_DRAIN_CONTROL_SOCK.with(|slot| *slot.borrow_mut() = None);
        stop_fake_daemon(&sock);

        assert_eq!(Some(CloneTransport::Fuse), event.transport);
        assert!(daemon.join().unwrap().is_empty());
    }

    #[test]
    fn redirect_drain_guard_releases_on_drop_panic_and_unrun_closure() {
        let _serial = IN_FLIGHT_FLAG_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let held = RedirectDrainGuard::try_acquire().unwrap();
        assert!(RedirectDrainGuard::try_acquire().is_none());
        drop(held);
        assert!(!REDIRECT_DRAIN_IN_FLIGHT.load(Ordering::Acquire));

        let held = RedirectDrainGuard::try_acquire().unwrap();
        let unwound = std::panic::catch_unwind(move || {
            let _in_flight = held;
            panic!("drain failed");
        });
        assert!(unwound.is_err());
        assert!(!REDIRECT_DRAIN_IN_FLIGHT.load(Ordering::Acquire));

        // A runtime that is shutting down drops a blocking closure unrun.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        runtime.shutdown_background();
        let held = RedirectDrainGuard::try_acquire().unwrap();
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let ran_in_closure = std::sync::Arc::clone(&ran);
        drop(handle.spawn_blocking(move || {
            let _in_flight = held;
            ran_in_closure.store(true, Ordering::Release);
        }));
        assert!(!ran.load(Ordering::Acquire), "the closure must not run");
        assert!(!REDIRECT_DRAIN_IN_FLIGHT.load(Ordering::Acquire));
        let reacquired = RedirectDrainGuard::try_acquire().unwrap();
        drop(reacquired);
        assert!(!REDIRECT_DRAIN_IN_FLIGHT.load(Ordering::Acquire));
    }

    #[cfg(unix)]
    #[test]
    fn drain_on_empty_ring_is_a_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = tmp.path().join("control.sock");
        let daemon = fake_daemon(
            &sock,
            vec![r#"{"status":"ok","data":{"v":1,"redirect_next_seq":1}}"#],
        );
        take_logged_redirect_events_for_test();

        let report = drain_redirect_events_sync(&client_for(&sock));
        stop_fake_daemon(&sock);

        assert_eq!((0, 0), report);
        assert!(take_logged_redirect_events_for_test().is_empty());
        assert_eq!(
            vec![serde_json::json!({"op":"redirect_events","v":1,"ack_through":0})],
            daemon.join().unwrap(),
            "an empty ring must not be acked"
        );
    }

    #[cfg(unix)]
    #[test]
    fn drain_against_an_old_daemon_logs_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = tmp.path().join("control.sock");
        let daemon = fake_daemon(
            &sock,
            vec![
                r#"{"status":"err","data":{"v":1,"error":"invalid request json: unknown variant"}}"#,
            ],
        );
        take_logged_redirect_events_for_test();

        let report = drain_redirect_events_sync(&client_for(&sock));
        stop_fake_daemon(&sock);

        assert_eq!((0, 0), report);
        assert!(take_logged_redirect_events_for_test().is_empty());
        assert_eq!(
            vec![serde_json::json!({"op":"redirect_events","v":1,"ack_through":0})],
            daemon.join().unwrap()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worktree_ended_fuse_unavailable_maps_the_grove_skip_variant() {
        let wt = report(
            "copy",
            vec![ArmSkip::from_grove(
                WorktreeArm::GroveFuse,
                GroveSkip::FuseUnavailable,
            )],
            Some("unknown"),
        );
        let event = emit_from(
            WorktreeLifecycle::Create,
            CloneOutcome::Success,
            true,
            Some("request"),
            None,
            Some(&wt),
            None,
        );
        assert_eq!(
            event.fallback_reason,
            Some(CloneFallbackReason::FuseUnavailable)
        );
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("/dev/fuse"), "{text}");
    }
}
