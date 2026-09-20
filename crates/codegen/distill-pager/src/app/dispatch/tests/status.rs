// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for session status, sharing, privacy, and coding-data-sharing dispatchers.

use super::*;

/// Regression for the leader-mode turn-end race: this client is briefly Idle while the server still has queued prompts.
/// Idle here means `is_turn_running() == false` with `current_prompt_id` cleared; the server's queue is visible as a non-empty `shared_queue` mirror.
/// A newly-sent prompt must route to the server (immediate-send), not drain locally as a phantom running turn.
#[test]
fn send_while_idle_with_nonempty_shared_queue_routes_to_server() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    // Two prompts already queued on the server (as a broadcast would leave things): populate the authoritative map and mirror it into the agent
    app.push_optimistic_prompt_echo("test-session", "q1", "a", "prompt");
    app.push_optimistic_prompt_echo("test-session", "q2", "b", "prompt");
    {
        let snapshot = app.shared_prompt_queue("test-session").cloned().unwrap();
        let agent = app.agents.get_mut(&id).unwrap();
        // Turn-end window: locally Idle with no current prompt, but the server's queue (mirrored from the last broadcast) still has work
        agent.session.state = AgentState::Idle;
        agent.session.current_prompt_id = None;
        agent.shared_queue = snapshot;
        assert!(agent.session.pending_prompts.is_empty());
    }

    let effects = dispatch(Action::SendPrompt("c".into()), &mut app);

    // Routed to the server (immediate-send), keyed by a fresh prompt_id.
    let pid = effects
        .iter()
        .find_map(|e| match e {
            Effect::SendPrompt {
                text, prompt_id, ..
            } if text == "c" => Some(prompt_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected immediate SendPrompt for 'c', got {effects:?}"));
    // The dispatch did not start a local turn or adopt "c" as the running prompt
    let Some(agent) = app.agents.get(&id) else {
        panic!("expected agent {id:?}");
    };
    assert!(
        !agent.session.state.is_turn_running(),
        "must not promote 'c' to a local running turn"
    );
    assert!(
        agent.session.current_prompt_id.is_none(),
        "must not set current_prompt_id locally for a server-queued prompt"
    );
    // Echoed into the shared queue behind the existing entries (position 3)
    let q = app
        .shared_prompt_queue("test-session")
        .expect("optimistic echo present");
    assert_eq!(q.len(), 3, "c queued behind q1, q2");
    assert_eq!(q.last().map(|e| e.id.as_str()), Some(pid.as_str()));
    assert_eq!(q.last().map(|e| e.text.as_str()), Some("c"));
}

// coding data sharing dispatch tests
// The dispatcher mutates optimistically and rolls back on failure, matching the `set_yolo_mode` pattern minus its toasts
// Guards (ZDR, non-admin team) toast and short-circuit; they are the only paths that still speak up, because nothing else on screen would

/// Idle unchanged opt-in skips ACP and still acks: the only direction allowed to skip the write.
#[test]
fn set_coding_data_sharing_unchanged_opt_in_skips_acp_and_acks() {
    let mut app = test_app_with_agent();
    app.privacy_notice_rollout = true;
    app.coding_data_retention_opt_out = false;
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
        "idle unchanged opt-in must still ack: {effects:?}"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SetCodingDataSharing { .. })),
        "idle unchanged opt-in must NOT write ACP: {effects:?}"
    );
    assert!(
        app.agents
            .get(&AgentId(0))
            .is_some_and(|a| a.toast.is_none())
    );
    assert!(!app.coding_data_retention_opt_out);
    assert!(app.privacy_banner_acked.is_some());
    assert!(app.coding_data_pending_write.is_none());
    assert_eq!(app.coding_data_write_seq, 0);
}

/// ZDR teams are blocked from toggling. The blocked path
/// toasts (not scrollback) and short-circuits with no Effect.
#[test]
fn set_coding_data_sharing_blocked_by_zdr() {
    let mut app = test_app_with_agent();
    app.is_zdr = true;
    app.coding_data_retention_opt_out = false;
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert!(effects.is_empty(), "ZDR block must NOT emit Effect");
    assert!(
        app.privacy_banner_acked.is_none(),
        "ZDR block must not ack the banner"
    );
    assert!(app.coding_data_pending_write.is_none());
    let toast = read_toast(&app);
    assert!(
        toast.contains("Zero Data Retention"),
        "ZDR toast must surface the policy: {toast}",
    );
    assert!(
        toast.contains('\u{2717}'),
        "blocked toast uses ✗ glyph: {toast}"
    );
    // State unchanged: the user was blocked, so the optimistic mutation never happened
    assert!(
        !app.coding_data_retention_opt_out,
        "ZDR block must not mutate state",
    );
}

/// ZDR block fires even when the toggle would be a no-op.
/// Defense-in-depth: don't quietly accept a same-value toggle from a user the policy says shouldn't be touching this.
#[test]
fn set_coding_data_sharing_blocked_by_zdr_even_if_idempotent() {
    let mut app = test_app_with_agent();
    app.is_zdr = true;
    app.coding_data_retention_opt_out = false;
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    assert!(effects.is_empty());
    assert!(app.privacy_banner_acked.is_none());
    assert!(app.coding_data_pending_write.is_none());
    assert!(read_toast(&app).contains("Zero Data Retention"));
}

/// Non-admin team members are blocked from toggling (matches desktop).
/// The blocked path toasts and short-circuits.
#[test]
fn set_coding_data_sharing_blocked_non_admin() {
    let mut app = test_app_with_agent();
    app.team_name = Some("Acme".into());
    app.team_role = Some("Member".into());
    app.coding_data_retention_opt_out = false;
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert!(effects.is_empty());
    assert!(app.privacy_banner_acked.is_none());
    assert!(app.coding_data_pending_write.is_none());
    let toast = read_toast(&app);
    assert!(
        toast.contains("team admin"),
        "non-admin toast must mention team admin: {toast}",
    );
}

/// Admin team members can toggle.
#[test]
fn set_coding_data_sharing_allowed_for_admin() {
    let mut app = test_app_with_agent();
    app.team_name = Some("Acme".into());
    app.team_role = Some("Admin".into());
    app.coding_data_retention_opt_out = false; // currently opted-in
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
        "rollout-off admin opt-out must not ack: {effects:?}"
    );
    match effects
        .iter()
        .find(|e| matches!(e, Effect::SetCodingDataSharing { .. }))
    {
        Some(Effect::SetCodingDataSharing { opted_in, .. }) => {
            assert!(!*opted_in, "Effect must carry opted_in=false");
        }
        other => panic!("expected SetCodingDataSharing Effect, got {effects:?} ({other:?})"),
    }
    // Optimistic mutation already applied.
    assert!(
        app.coding_data_retention_opt_out,
        "admin-allowed dispatch must optimistically flip state",
    );
    assert!(app.privacy_banner_acked.is_none());
}

/// Non-idempotent dispatch emits one Effect and mutates state optimistically.
#[test]
fn set_coding_data_sharing_produces_effect_and_optimistic_mutation() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false; // currently opted-in
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
        "rollout-off changed opt-out must not ack: {effects:?}"
    );
    match effects
        .iter()
        .find(|e| matches!(e, Effect::SetCodingDataSharing { .. }))
    {
        Some(Effect::SetCodingDataSharing {
            agent_id,
            opted_in,
            seq,
        }) => {
            assert_eq!(*agent_id, AgentId(0));
            assert!(!*opted_in);
            assert_eq!(
                *seq, app.coding_data_write_seq,
                "the effect must carry the generation it was dispatched under",
            );
        }
        other => panic!("expected SetCodingDataSharing Effect, got {effects:?} ({other:?})"),
    }
    // Optimistic mutation applied.
    assert!(
        app.coding_data_retention_opt_out,
        "dispatch must optimistically mutate state",
    );
    assert!(app.privacy_banner_acked.is_none());
    assert_eq!(
        app.coding_data_pending_write,
        Some(PendingCodingDataWrite {
            opted_in: false,
            rollback_to_opted_in: true,
        })
    );
    assert!(
        app.agents
            .get(&AgentId(0))
            .is_some_and(|a| a.toast.is_none()),
        "changing this setting must not toast — the settings row is the feedback",
    );
}

/// `TaskResult::CodingDataSharingUpdated` re-anchors state to the server-confirmed value (defense-in-depth).
#[test]
fn coding_data_sharing_updated_re_anchors_state() {
    let mut app = test_app_with_agent();
    // Simulate post-optimistic state: opted-out.
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    // Server confirms opt-out (same as optimistic).
    let seq = app.coding_data_write_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: id,
            opted_in: false,
            seq,
        }),
        &mut app,
    );
    assert!(effects.is_empty(), "TaskResult arm must NOT emit Effect");
    // State re-anchored (was already true, stays true).
    assert!(app.coding_data_retention_opt_out);
    assert!(
        app.agents
            .get(&AgentId(0))
            .is_some_and(|a| a.toast.is_none()),
        "server confirmation must not toast",
    );
}

/// `TaskResult::CodingDataSharingUpdated` corrects the in-memory state if the server reshapes the boolean (e.g. policy override).
/// Pins the defense-in-depth re-anchor contract.
#[test]
fn coding_data_sharing_updated_corrects_state_if_server_disagrees() {
    let mut app = test_app_with_agent();
    // Optimistic mutation said "opt-out", but the server overrides to "opt-in" (e.g. policy that prevents opt-out).
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    let seq = app.coding_data_write_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: id,
            opted_in: true, // server says opted-in
            seq,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    // State corrected to match server.
    assert!(
        !app.coding_data_retention_opt_out,
        "server-confirmed opt-in must overwrite optimistic opt-out",
    );
}

/// `TaskResult::CodingDataSharingFailed` reverts the optimistic mutation and shows a failure toast.
/// Pins the rollback contract.
/// The failure toast uses the standardised "coding data sharing" wording.
#[test]
fn coding_data_sharing_failed_rolls_back_and_toasts_error() {
    let mut app = test_app_with_agent();
    // Post-optimistic state: the user picked opt-out from opt-in, then the ACP call failed
    app.coding_data_retention_opt_out = true;
    app.coding_data_pending_write = Some(PendingCodingDataWrite {
        opted_in: false,
        rollback_to_opted_in: true,
    });
    let id = AgentId(0);
    let seq = app.coding_data_write_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: "server error".into(),
            seq,
        }),
        &mut app,
    );
    assert!(effects.is_empty(), "rollback path must NOT emit Effect");
    // State reverted to pre-toggle (opted-in).
    assert!(
        !app.coding_data_retention_opt_out,
        "rollback must revert optimistic mutation",
    );
    // Failure toast surfaces the error using full label.
    let toast = read_toast(&app);
    assert!(
        toast.contains("coding data sharing"),
        "PR 9 R1: failure toast wording standardised to include 'coding data sharing' \
             (G2 Issue 2): {toast}",
    );
    assert!(toast.contains("server error"), "error in toast: {toast}");
    assert!(toast.contains('\u{2717}'), "failure toast uses ✗: {toast}");
}

/// `TaskResult::CodingDataSharingFailed` reverts in the other direction too (the pre-toggle state could have been either).
#[test]
fn coding_data_sharing_failed_rolls_back_to_opt_out() {
    let mut app = test_app_with_agent();
    // Post-optimistic: opted-in (the user picked opt-in from opt-out, the server failed)
    app.coding_data_retention_opt_out = false;
    app.coding_data_pending_write = Some(PendingCodingDataWrite {
        opted_in: true,
        rollback_to_opted_in: false,
    });
    let id = AgentId(0);
    let seq = app.coding_data_write_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: "network timeout".into(),
            seq,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    // Reverted to pre-toggle opt-out.
    assert!(
        app.coding_data_retention_opt_out,
        "rollback to opt-out must set state=true",
    );
}

/// Optimistic mutation refreshes any open settings modal.
/// Without this refresh, the modal indicator would stay at the pre-toggle value until a manual re-render.
#[test]
fn set_coding_data_sharing_refreshes_open_modal_snapshot() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false;
    // Open a settings modal (capture initial snapshot).
    let _ = dispatch(Action::OpenSettings, &mut app);
    // Verify snapshot reads opted-in.
    let agent_id = AgentId(0);
    {
        let state = match app
            .agents
            .get(&agent_id)
            .and_then(|a| a.active_modal.as_ref())
        {
            Some(crate::views::modal::ActiveModal::Settings { state }) => state,
            _ => panic!("expected Settings modal open after OpenSettings dispatch"),
        };
        assert!(
            !state.pager_snapshot.coding_data_sharing_opt_out,
            "initial snapshot must read opt_out=false (opted-in)",
        );
    }
    // Dispatch the toggle.
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    // Snapshot now reflects the optimistic mutation.
    let state = match app
        .agents
        .get(&agent_id)
        .and_then(|a| a.active_modal.as_ref())
    {
        Some(crate::views::modal::ActiveModal::Settings { state }) => state,
        _ => panic!("Settings modal must still be open after SetCodingDataSharing dispatch"),
    };
    assert!(
        state.pager_snapshot.coding_data_sharing_opt_out,
        "snapshot must refresh to reflect opt_out=true (opted-out) after dispatch",
    );
}

/// Rollback also refreshes the modal: the user sees the reverted value, not the stale optimistic one.
#[test]
fn coding_data_sharing_failed_refreshes_open_modal_snapshot() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false;
    let _ = dispatch(Action::OpenSettings, &mut app);
    // Optimistic flip.
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    // ACP failure.
    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "x".into(),
            seq,
        }),
        &mut app,
    );
    let state = match app
        .agents
        .get(&AgentId(0))
        .and_then(|a| a.active_modal.as_ref())
    {
        Some(crate::views::modal::ActiveModal::Settings { state }) => state,
        _ => panic!("Settings modal must still be open after rollback TaskResult"),
    };
    assert!(
        !state.pager_snapshot.coding_data_sharing_opt_out,
        "rollback must refresh snapshot back to opt_out=false (opted-in)",
    );
}

#[test]
fn set_coding_data_sharing_is_silent_in_both_directions() {
    for opted_in in [true, false] {
        let mut app = test_app_with_agent();
        app.coding_data_retention_opt_out = opted_in; // a real change either way
        let _ = dispatch(Action::SetCodingDataSharing { opted_in }, &mut app);
        assert!(
            app.agents
                .get(&AgentId(0))
                .is_some_and(|a| a.toast.is_none()),
            "opted_in={opted_in} must not toast, got {:?}",
            app.agents.get(&AgentId(0)).and_then(|a| a.toast.as_ref()),
        );
    }
}

/// The failure toast substitutes a generic placeholder when the error string is too long or contains control characters / newlines.
/// Pins the scrub contract.
#[test]
fn coding_data_sharing_failed_scrubs_long_error_messages() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    // A roughly 500-char error simulating a stack trace / HTML 502 page
    let huge_error = "a".repeat(500);
    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: huge_error.clone(),
            seq,
        }),
        &mut app,
    );
    let toast = read_toast(&app);
    assert!(
        !toast.contains(&huge_error),
        "long error MUST be scrubbed from the toast: {} chars",
        toast.len(),
    );
    assert!(
        toast.contains("see logs"),
        "scrubbed toast must point at the log for full details: {toast}",
    );
}

/// Control characters (CR/LF/NUL) in the error trigger the scrub path even on short strings.
/// This preserves the toast's single-line layout.
#[test]
fn coding_data_sharing_failed_scrubs_control_chars_in_error() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    // Short message with embedded newlines.
    let multiline = "line1\nline2\nline3".to_string();
    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: multiline.clone(),
            seq,
        }),
        &mut app,
    );
    let toast = read_toast(&app);
    assert!(
        !toast.contains('\n'),
        "newlines MUST be scrubbed from the toast (would break single-line layout): \
             {toast:?}",
    );
    assert!(
        toast.contains("see logs"),
        "control-char-scrubbed toast points at logs: {toast}",
    );
}

/// The scrub path preserves short, sanitised error messages verbatim.
/// The typical happy-path shell-side error string stays unscrubbed.
#[test]
fn coding_data_sharing_failed_preserves_short_clean_error_message() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    let short_clean = "network timeout".to_string();
    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: short_clean.clone(),
            seq,
        }),
        &mut app,
    );
    let toast = read_toast(&app);
    assert!(
        toast.contains(&short_clean),
        "short clean error must appear verbatim in the toast: {toast}",
    );
    assert!(
        !toast.contains("see logs"),
        "short clean error must NOT trigger the scrub fallback: {toast}",
    );
}

/// Direct unit test of the `scrub_error_for_toast` helper; pins the threshold and the fallback string against drift.
#[test]
fn scrub_error_for_toast_unit() {
    // Empty and short messages pass through
    assert_eq!(scrub_error_for_toast(""), "");
    assert_eq!(scrub_error_for_toast("ok"), "ok");
    assert_eq!(scrub_error_for_toast("network timeout"), "network timeout");
    // At-threshold (120 chars) still passes through.
    let len_120 = "x".repeat(120);
    assert_eq!(scrub_error_for_toast(&len_120), len_120);
    // Over-threshold (121 chars) triggers scrub.
    let len_121 = "x".repeat(121);
    assert_eq!(
        scrub_error_for_toast(&len_121),
        "server error (see logs for details)"
    );
    // Control chars trigger scrub even at short lengths.
    assert_eq!(
        scrub_error_for_toast("hi\nthere"),
        "server error (see logs for details)"
    );
    assert_eq!(
        scrub_error_for_toast("hi\rthere"),
        "server error (see logs for details)"
    );
    // Format-category (Cf) chars also trigger scrub: bidi overrides, zero-width joiner / space, BOM
    // This prevents Trojan-Source-style spoofing: a toast that reads as one thing while the bytes encode another via a RIGHT-TO-LEFT OVERRIDE
    assert_eq!(
        scrub_error_for_toast("opt\u{202E}-out"),
        "server error (see logs for details)",
        "RIGHT-TO-LEFT OVERRIDE (U+202E) must be scrubbed",
    );
    assert_eq!(
        scrub_error_for_toast("opt\u{200B}out"),
        "server error (see logs for details)",
        "ZERO WIDTH SPACE (U+200B) must be scrubbed",
    );
    assert_eq!(
        scrub_error_for_toast("\u{FEFF}leading BOM"),
        "server error (see logs for details)",
        "BOM (U+FEFF) must be scrubbed",
    );
    assert_eq!(
        scrub_error_for_toast("zwj\u{200D}joiner"),
        "server error (see logs for details)",
        "ZERO WIDTH JOINER (U+200D) must be scrubbed",
    );
}

/// Synthetic AgentId(0) when no agents (welcome banner Accept path).
#[test]
fn set_coding_data_sharing_no_agents_still_emits_effect() {
    let mut app = test_app_with_agent();
    app.agents.clear();
    app.active_view = ActiveView::Welcome;
    app.coding_data_retention_opt_out = true;
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    assert_eq!(effects.len(), 1, "no-agent path must still emit Effect");
    assert!(
        matches!(
            effects.first(),
            Some(Effect::SetCodingDataSharing { opted_in: true, .. })
        ),
        "changed opt-in must be the ACP write, not an early ack: {effects:?}"
    );
    assert!(
        !app.coding_data_retention_opt_out,
        "optimistic opt-in must apply without agents",
    );
    assert_eq!(app.coding_data_pending_opted_in(), Some(true));
    assert!(app.privacy_banner_acked.is_none());
}

fn privacy_banner_ready_app() -> AppView {
    let mut app = test_app_with_agent();
    app.active_view = ActiveView::Welcome;
    app.auth_state = AuthState::Done;
    app.trust_state = TrustState::Done;
    app.privacy_notice_rollout = true;
    app.privacy_banner_acked = None;
    app.privacy_banner_reshow_days = None;
    app.coding_data_pending_write = None;
    app.is_zdr = false;
    app.team_name = None;
    app.coding_data_retention_opt_out = true;
    app
}

#[test]
fn privacy_banner_should_show_respects_gates() {
    let mut app = privacy_banner_ready_app();
    assert!(app.privacy_banner_should_show());

    app.coding_data_retention_opt_out = false;
    assert!(!app.privacy_banner_should_show(), "already opted in");
    app.coding_data_retention_opt_out = true;

    app.is_zdr = true;
    assert!(!app.privacy_banner_should_show(), "enterprise ZDR");
    app.is_zdr = false;

    app.privacy_banner_acked = Some("2099-01-01T00:00:00Z".into());
    assert!(
        !app.privacy_banner_should_show(),
        "recently acked, no reshow"
    );

    app.privacy_banner_reshow_days = Some(30);
    app.privacy_banner_acked = Some("2020-01-01T00:00:00Z".into());
    assert!(
        app.privacy_banner_should_show(),
        "acked long ago + reshow_days"
    );

    app.privacy_notice_rollout = false;
    assert!(!app.privacy_banner_should_show(), "rollout off");
}

/// `[Opt in]` success: ACP confirmation acks the banner.
#[test]
fn privacy_banner_opt_in_success_acks() {
    let mut app = privacy_banner_ready_app();
    let effects = dispatch(Action::PrivacyBannerOptIn, &mut app);
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::SetCodingDataSharing { opted_in: true, .. })
    ));
    assert_eq!(app.coding_data_pending_opted_in(), Some(true));
    assert!(!app.coding_data_retention_opt_out);
    assert!(app.privacy_banner_acked.is_none());

    let seq = app.coding_data_write_seq;
    let ack_effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: AgentId(0),
            opted_in: true,
            seq,
        }),
        &mut app,
    );
    assert!(app.coding_data_pending_write.is_none());
    assert!(app.privacy_banner_acked.is_some());
    assert!(!app.privacy_banner_should_show());
    assert!(
        ack_effects
            .iter()
            .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
        "success must persist ack: {ack_effects:?}"
    );
}

/// `[Opt in]` failure: no ack; welcome toast carries the error.
#[test]
fn privacy_banner_opt_in_failure_no_ack_sets_welcome_toast() {
    let mut app = privacy_banner_ready_app();
    let effects = dispatch(Action::PrivacyBannerOptIn, &mut app);
    assert_eq!(effects.len(), 1);
    assert_eq!(app.coding_data_pending_opted_in(), Some(true));

    let seq = app.coding_data_write_seq;
    let fail_effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "server error".into(),
            seq,
        }),
        &mut app,
    );
    assert!(fail_effects.is_empty());
    assert!(app.coding_data_pending_write.is_none());
    assert!(app.privacy_banner_acked.is_none());
    assert!(
        app.coding_data_retention_opt_out,
        "rollback restores opt-out"
    );
    assert!(
        app.privacy_banner_should_show(),
        "failed [Opt in] must leave the banner eligible"
    );
    let toast = app
        .welcome_toast
        .as_ref()
        .map(|(m, _)| m.as_str())
        .unwrap_or("");
    assert!(
        toast.contains("coding data sharing"),
        "welcome toast on [Opt in] failure: {toast}"
    );
    assert!(toast.contains("server error"), "error in toast: {toast}");
}

/// `[Opt out]` while an `[Opt in]` ACP call is inflight must be a no-op:
/// an eager ack would survive the opt-in-failure rollback and hide the
/// banner forever.
#[test]
fn privacy_banner_opt_out_noop_while_opt_in_inflight() {
    let mut app = privacy_banner_ready_app();
    let _ = dispatch(Action::PrivacyBannerOptIn, &mut app);
    assert_eq!(app.coding_data_pending_opted_in(), Some(true));

    let effects = dispatch(Action::PrivacyBannerOptOut, &mut app);
    assert!(
        effects.is_empty(),
        "[Opt out] during an inflight [Opt in] must be a no-op: {effects:?}"
    );
    assert!(app.privacy_banner_acked.is_none(), "no ack while inflight");

    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "server error".into(),
            seq,
        }),
        &mut app,
    );
    assert!(
        app.privacy_banner_should_show(),
        "a failed [Opt in] must keep the banner even after a raced [Opt out]"
    );
}

/// Coalescing and the idle opt-in shortcut key on the pending write's choice, not the mirror a subscription check can
/// rewrite mid-flight. Otherwise a pending opt-in lands as the answer to Opt out, or Opt in acks with nothing sent.
#[test]
fn stale_mirror_does_not_coalesce_an_opposite_choice() {
    for pending in [true, false] {
        let mut app = privacy_banner_ready_app();
        app.coding_data_retention_opt_out = pending;
        let _ = dispatch(Action::SetCodingDataSharing { opted_in: pending }, &mut app);

        // Background refresh still carries the pre-write value
        let meta = serde_json::to_value(distill_login::AuthMeta {
            coding_data_retention_opt_out: pending,
            ..Default::default()
        })
        .unwrap();
        let _ = dispatch(
            Action::TaskComplete(TaskResult::CheckSubscriptionComplete {
                verify: None,
                meta: Some(meta),
            }),
            &mut app,
        );
        assert_eq!(
            app.coding_data_retention_opt_out, pending,
            "mirror rewritten"
        );
        assert_eq!(
            app.coding_data_pending_opted_in(),
            Some(pending),
            "pending untouched"
        );

        let effects = dispatch(
            Action::SetCodingDataSharing { opted_in: !pending },
            &mut app,
        );
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SetCodingDataSharing { opted_in, seq: 2, .. }] if *opted_in != pending
            ),
            "pending={pending}: the opposite choice must write: {effects:?}"
        );
        assert!(
            app.privacy_banner_acked.is_none(),
            "pending={pending}: no local ack"
        );
    }
}

/// Already-out opt-out, from the banner or Settings, still writes: the local "out" may be the unconfirmed fail-safe default.
#[test]
fn already_out_opt_out_writes_and_acks_on_success() {
    for from_banner in [true, false] {
        let action = if from_banner {
            Action::PrivacyBannerOptOut
        } else {
            Action::SetCodingDataSharing { opted_in: false }
        };
        let mut app = privacy_banner_ready_app();

        let effects = dispatch(action, &mut app);
        let action = if from_banner { "[Opt out]" } else { "Settings" };

        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SetCodingDataSharing {
                    opted_in: false,
                    seq: 1,
                    ..
                }]
            ),
            "{action}: already-out must still write, and only write: {effects:?}"
        );
        assert!(
            app.privacy_banner_acked.is_none(),
            "{action}: no ack before the reply"
        );

        let ack_effects = dispatch(
            Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
                agent_id: AgentId(0),
                opted_in: false,
                seq: 1,
            }),
            &mut app,
        );
        assert!(
            ack_effects
                .iter()
                .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
            "{action}: success must persist the ack: {ack_effects:?}"
        );
        assert!(!app.privacy_banner_should_show());
    }
}

/// A Settings opt-out must not raise the banner mid-write: the optimistic "out" is unconfirmed, and a banner
/// there would re-ask the choice the user just made, with buttons that no-op until the reply lands.
#[test]
fn settings_opt_out_does_not_reveal_banner_while_inflight() {
    let mut app = privacy_banner_ready_app();
    app.coding_data_retention_opt_out = false;

    let _ = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert!(
        !app.privacy_banner_should_show(),
        "the pending write already carries the user's answer"
    );

    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: AgentId(0),
            opted_in: false,
            seq: app.coding_data_write_seq,
        }),
        &mut app,
    );
    assert!(app.privacy_banner_acked.is_some(), "the reply acks");
    assert!(!app.privacy_banner_should_show());
}

/// The always-write opt-out starts with the unconfirmed fail-safe value as its rollback.
/// An auth-meta refresh mid-flight replaces it: reverting to the snapshot on failure would show an opt-out the server never made.
#[test]
fn opt_out_failure_keeps_a_mid_flight_auth_meta_value() {
    let mut app = privacy_banner_ready_app();
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    let seq = app.coding_data_write_seq;

    app.apply_auth_meta(&distill_login::AuthMeta {
        coding_data_retention_opt_out: false,
        ..Default::default()
    });

    let fail = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "server error".into(),
            seq,
        }),
        &mut app,
    );

    assert!(fail.is_empty());
    assert!(
        !app.coding_data_retention_opt_out,
        "the refresh is newer than the click-time snapshot"
    );
}

/// A superseded write's success becomes the rollback of the write that replaced it.
/// Otherwise: pending opt-in, refresh restores the old "out", user opts out, opt-in succeeds (dropped as stale), opt-out
/// fails and reverts to "out" while the server retains.
#[test]
fn superseded_opt_in_success_is_kept_when_later_opt_out_fails() {
    let mut app = privacy_banner_ready_app();
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    app.apply_auth_meta(&distill_login::AuthMeta {
        coding_data_retention_opt_out: true,
        ..Default::default()
    });
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert_eq!(app.coding_data_write_seq, 2);

    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: AgentId(0),
            opted_in: true,
            seq: 1,
        }),
        &mut app,
    );
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "server error".into(),
            seq: 2,
        }),
        &mut app,
    );

    assert!(
        !app.coding_data_retention_opt_out,
        "the failed opt-out must fall back to the opt-in the server confirmed"
    );
    assert!(app.coding_data_pending_write.is_none());
}

/// A refused opt-out must not dismiss the banner: the server is still retaining, and the user has to see that.
#[test]
fn privacy_banner_opt_out_failure_keeps_banner_and_toasts() {
    let mut app = privacy_banner_ready_app();
    let effects = dispatch(Action::PrivacyBannerOptOut, &mut app);
    assert_eq!(effects.len(), 1, "opt-out must write: {effects:?}");

    let fail_effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "team policy forbids opt-out".into(),
            seq: app.coding_data_write_seq,
        }),
        &mut app,
    );
    assert!(fail_effects.is_empty());
    assert!(app.privacy_banner_acked.is_none());
    assert!(
        app.privacy_banner_should_show(),
        "a refused [Opt out] must leave the banner up"
    );
    let toast = app
        .welcome_toast
        .as_ref()
        .map(|(m, _)| m.as_str())
        .unwrap_or("");
    assert!(
        toast.contains("team policy forbids opt-out"),
        "the refusal must reach the user: {toast}"
    );
}

/// Settings opt-out is write 1, the user opts in before it lands, and write 2 answers first.
/// Covered: write 2 succeeds and the stale write 1 reply (either kind) must not set the mirror or toast; both writes fail and
/// write 2 must fall back to the opt-in it inherited from write 1, not to write 1's optimistic out.
/// Not covered: write 2 fails, then write 1 succeeds — the pending write is already gone, so that success is dropped (deferred).
#[test]
fn stale_reply_after_newer_success_or_double_failure_keeps_opt_in() {
    let welcome_toast = |app: &AppView| app.welcome_toast.as_ref().map(|(m, _)| m.clone());
    for (newer_ok, stale_failed) in [(true, true), (true, false), (false, true)] {
        let mut app = privacy_banner_ready_app();
        app.coding_data_retention_opt_out = false;

        // Write 1: Settings opt-out from currently in.
        let write1 = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
        assert!(
            write1.iter().any(|e| matches!(
                e,
                Effect::SetCodingDataSharing {
                    opted_in: false,
                    ..
                }
            )),
            "write 1 must be a real opt-out: {write1:?}"
        );
        assert_eq!(app.coding_data_write_seq, 1);

        // Write 2: the user opts in from settings, and it answers first.
        let _ = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
        assert_eq!(app.coding_data_write_seq, 2);
        let newer_reply = if newer_ok {
            TaskResult::CodingDataSharingUpdated {
                agent_id: AgentId(0),
                opted_in: true,
                seq: 2,
            }
        } else {
            TaskResult::CodingDataSharingFailed {
                agent_id: AgentId(0),
                error: "server error".into(),
                seq: 2,
            }
        };
        let _ = dispatch(Action::TaskComplete(newer_reply), &mut app);
        assert!(
            !app.coding_data_retention_opt_out,
            "opted in either way (newer_ok={newer_ok})"
        );
        assert_eq!(app.privacy_banner_acked.is_some(), newer_ok);
        let toast_before = welcome_toast(&app);

        // Write 1 finally answers, either way it can.
        let stale_reply = if stale_failed {
            TaskResult::CodingDataSharingFailed {
                agent_id: AgentId(0),
                error: "network timeout".into(),
                seq: 1,
            }
        } else {
            TaskResult::CodingDataSharingUpdated {
                agent_id: AgentId(0),
                opted_in: false,
                seq: 1,
            }
        };
        let effects = dispatch(Action::TaskComplete(stale_reply), &mut app);

        assert!(effects.is_empty(), "stale reply must emit nothing");
        assert!(
            !app.coding_data_retention_opt_out,
            "stale reply must not undo the opt-in (newer_ok={newer_ok}, failed={stale_failed})"
        );
        assert_eq!(
            welcome_toast(&app),
            toast_before,
            "stale reply must not toast"
        );
        assert!(app.coding_data_pending_write.is_none());
    }
}

/// A double-click (or a stale frame's hit rect) must not send a second decline.
#[test]
fn privacy_banner_opt_out_is_idempotent() {
    let mut app = privacy_banner_ready_app();
    let _ = dispatch(Action::PrivacyBannerOptOut, &mut app);
    let again = dispatch(Action::PrivacyBannerOptOut, &mut app);
    assert!(
        again.is_empty(),
        "second dismissal must be inert: {again:?}"
    );
}

/// A duplicate Settings opt-out rides the pending write: a second write would supersede the first and drop its reply for nothing new.
#[test]
fn duplicate_settings_opt_out_rides_the_pending_write() {
    let mut app = privacy_banner_ready_app();
    app.coding_data_retention_opt_out = false;
    let first = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert_eq!(first.len(), 1, "changed opt-out must write: {first:?}");

    let again = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert!(
        again.is_empty(),
        "duplicate must not write or ack: {again:?}"
    );
    assert_eq!(app.coding_data_write_seq, 1);
    assert!(app.privacy_banner_acked.is_none());
}

/// Re-committing Opt in while the first write is inflight must not ack.
/// That ack would survive a later ACP failure and hide the banner.
#[test]
fn settings_opt_in_recommitted_while_inflight_does_not_ack() {
    let mut app = privacy_banner_ready_app();
    let first = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    assert!(
        first
            .iter()
            .any(|e| matches!(e, Effect::SetCodingDataSharing { opted_in: true, .. })),
        "first commit must write: {first:?}"
    );
    assert!(
        !first
            .iter()
            .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
        "first commit must not ack: {first:?}"
    );
    assert_eq!(app.coding_data_pending_opted_in(), Some(true));
    assert!(app.privacy_banner_acked.is_none());
    let seq = app.coding_data_write_seq;
    assert_eq!(seq, 1);

    let again = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    assert!(
        !again
            .iter()
            .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
        "re-commit while inflight must not ack: {again:?}"
    );
    assert!(
        !again
            .iter()
            .any(|e| matches!(e, Effect::SetCodingDataSharing { .. })),
        "re-commit while inflight must not write again: {again:?}"
    );
    assert_eq!(app.coding_data_pending_opted_in(), Some(true));
    assert_eq!(app.coding_data_write_seq, seq);
    assert!(app.privacy_banner_acked.is_none());

    let fail_effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "server error".into(),
            seq,
        }),
        &mut app,
    );
    assert!(fail_effects.is_empty());
    assert!(app.coding_data_pending_write.is_none());
    assert!(app.privacy_banner_acked.is_none());
    assert!(app.coding_data_retention_opt_out);
    assert!(app.privacy_banner_should_show());
}

/// A Settings pick before the notice is rolled out must not stamp an ack that would hide the banner when the cohort turns on.
#[test]
fn settings_choice_does_not_ack_when_rollout_off() {
    for opted_in in [true, false] {
        let mut app = test_app_with_agent();
        app.privacy_notice_rollout = false;
        app.coding_data_retention_opt_out = true;
        app.auth_state = AuthState::Done;
        app.trust_state = TrustState::Done;
        let effects = dispatch(Action::SetCodingDataSharing { opted_in }, &mut app);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
            "rollout-off must not persist ack (opted_in={opted_in}): {effects:?}"
        );
        assert!(
            app.privacy_banner_acked.is_none(),
            "rollout-off must not stamp ack (opted_in={opted_in})"
        );
        if opted_in {
            let seq = app.coding_data_write_seq;
            let ack_effects = dispatch(
                Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
                    agent_id: AgentId(0),
                    opted_in: true,
                    seq,
                }),
                &mut app,
            );
            assert!(
                !ack_effects
                    .iter()
                    .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
                "rollout-off opt-in success must not ack: {ack_effects:?}"
            );
            assert!(app.privacy_banner_acked.is_none());
        }
    }
}

#[test]
fn dispatch_rename_session_updates_display_name_locally() {
    let mut app = test_app_with_agent();
    let effects = dispatch_rename_session(&mut app, "renamed via slash".into());
    assert_eq!(effects.len(), 1);
    assert_eq!(
        app.agents
            .get(&AgentId(0))
            .and_then(|a| a.display_name.as_deref()),
        Some("renamed via slash"),
        "/rename must also update local display_name cache"
    );
    match effects.first() {
        Some(Effect::RenameSession { kind, .. }) => {
            assert_eq!(
                *kind,
                distill_shell::session::unified_list::SessionKind::Build,
                "build-lane /rename must send kind=build"
            );
        }
        other => panic!("expected RenameSession, got {other:?}"),
    }
}

#[test]
fn dispatch_rename_session_strips_controls_before_display_name_and_effect() {
    let mut app = test_app_with_agent();
    let effects =
        dispatch_rename_session(&mut app, "  Hello\u{1b}[31mWorld\u{07}\u{9b}C1  ".into());
    assert_eq!(
        app.agents
            .get(&AgentId(0))
            .and_then(|a| a.display_name.as_deref()),
        Some("Hello[31mWorldC1"),
        "optimistic display_name must match the shell strip (no OSC/CSI/BEL/C1)"
    );
    match effects.as_slice() {
        [Effect::RenameSession { title, .. }] => {
            assert_eq!(title, "Hello[31mWorldC1");
        }
        other => panic!("expected one RenameSession, got {other:?}"),
    }

    let mut app = test_app_with_agent();
    let effects = dispatch_rename_session(&mut app, "\u{1b}\u{07}\n\t".into());
    assert!(
        effects.is_empty(),
        "control-only title must not emit RenameSession: {effects:?}"
    );
    assert!(
        app.agents
            .get(&AgentId(0))
            .is_some_and(|a| a.display_name.is_none()),
        "control-only title must not paint a blank/dirty display_name"
    );
    assert!(
        last_system_text(&app, AgentId(0)).contains("title must not be blank"),
        "control-only title must surface the same failed-rename system block"
    );
}

#[test]
fn dispatch_rename_session_chat_kind_stamps_kind_chat() {
    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.chat_kind = true;
    agent.conversation_entry = true;
    let effects = dispatch_rename_session(&mut app, "chat rename".into());
    match effects.as_slice() {
        [Effect::RenameSession { kind, title, .. }] => {
            assert_eq!(title, "chat rename");
            assert_eq!(
                *kind,
                distill_shell::session::unified_list::SessionKind::Chat,
                "chat-lane /rename must send kind=chat"
            );
        }
        other => panic!("expected one RenameSession, got {other:?}"),
    }
}

#[test]
fn dispatch_rename_session_sticky_chat_local_build_stays_build() {
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    // `chat_kind` is the sticky `--chat` UI bit; `conversation_entry = false` marks a local-disk history bypass, not a conversation
    agent.chat_kind = true;
    agent.conversation_entry = false;
    let effects = dispatch_rename_session(&mut app, "local title".into());
    match effects.as_slice() {
        [Effect::RenameSession { kind, title, .. }] => {
            assert_eq!(title, "local title");
            assert_eq!(
                *kind,
                distill_shell::session::unified_list::SessionKind::Build,
                "history-bypass local build under sticky --chat must send kind=build"
            );
        }
        other => panic!("expected one RenameSession, got {other:?}"),
    }
}

#[test]
fn rename_session_request_serializes_camel_case_kind() {
    use crate::app::actions::RenameSessionRequest;
    use distill_shell::session::unified_list::SessionKind;

    let build = serde_json::to_value(RenameSessionRequest::for_rename(
        "sid".into(),
        "T".into(),
        "/repo".into(),
        SessionKind::Build,
    ))
    .unwrap();
    assert_eq!(
        build,
        serde_json::json!({
            "sessionId": "sid",
            "title": "T",
            "cwd": "/repo",
            "kind": "build",
        })
    );

    let chat = serde_json::to_value(RenameSessionRequest::for_rename(
        "cid".into(),
        "Chat".into(),
        "/tmp".into(),
        SessionKind::Chat,
    ))
    .unwrap();
    assert_eq!(
        chat,
        serde_json::json!({
            "sessionId": "cid",
            "title": "Chat",
            "cwd": "/tmp",
            "kind": "chat",
        })
    );

    let unpin = serde_json::to_value(RenameSessionRequest::for_reset(
        "sid".into(),
        "/repo".into(),
        SessionKind::Build,
    ))
    .unwrap();
    assert_eq!(
        unpin,
        serde_json::json!({
            "sessionId": "sid",
            "title": "",
            "cwd": "/repo",
            "kind": "build",
            "resetToAuto": true,
        }),
        "unpin must send empty title + resetToAuto so old shells reject blank"
    );
}

#[test]
fn dispatch_reset_session_title_clears_titles_and_emits_effect() {
    let mut app = test_app_with_agent();
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.display_name = Some("Manual".into());
        // Post-rename both caches hold the pin (fan-out / resume).
        agent.generated_session_title = Some("Manual".into());
    }
    let effects = dispatch_reset_session_title(&mut app);
    let Some(agent) = app.agents.get(&AgentId(0)) else {
        panic!("expected agent 0");
    };
    assert!(
        agent.display_name.is_none(),
        "optimistic unpin must clear display_name"
    );
    assert!(
        agent.generated_session_title.is_none(),
        "optimistic unpin must clear generated_session_title when it matches the pin"
    );
    assert_ne!(
        crate::views::session_title::entry_title(agent),
        "Manual",
        "dashboard/tab entry_title must not stay the manual pin"
    );
    match effects.as_slice() {
        [
            Effect::ResetSessionTitle {
                agent_id,
                session_id,
                cwd,
                kind,
                previous_display_name,
                previous_generated_title,
            },
        ] => {
            assert_eq!(*agent_id, AgentId(0));
            assert_eq!(session_id.0.as_ref(), "test-session");
            assert_eq!(cwd, std::path::Path::new("/tmp"));
            assert_eq!(
                *kind,
                distill_shell::session::unified_list::SessionKind::Build
            );
            assert_eq!(previous_display_name.as_deref(), Some("Manual"));
            assert_eq!(previous_generated_title.as_deref(), Some("Manual"));
        }
        other => panic!("expected ResetSessionTitle, got {other:?}"),
    }
}

#[test]
fn dispatch_reset_session_title_never_manual_keeps_generated_title() {
    let mut app = test_app_with_agent();
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.display_name = None;
        agent.generated_session_title = Some("Auto".into());
    }
    let effects = dispatch_reset_session_title(&mut app);
    let Some(agent) = app.agents.get(&AgentId(0)) else {
        panic!("expected agent 0");
    };
    assert!(agent.display_name.is_none());
    assert_eq!(agent.generated_session_title.as_deref(), Some("Auto"));
    assert_eq!(
        crate::views::session_title::entry_title(agent),
        "Auto",
        "already-auto unpin must stay a UI no-op"
    );
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ResetSessionTitle {
                kind: distill_shell::session::unified_list::SessionKind::Build,
                ..
            }]
        ),
        "got {effects:?}"
    );
}

#[test]
fn dispatch_reset_session_title_sticky_chat_local_build_stays_build() {
    let mut app = test_app_with_agent();
    app.chat_mode = true;
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.chat_kind = true;
        agent.conversation_entry = false;
        agent.display_name = Some("Manual".into());
        agent.generated_session_title = Some("Auto".into());
    }
    let effects = dispatch_reset_session_title(&mut app);
    match effects.as_slice() {
        [Effect::ResetSessionTitle { kind, .. }] => {
            assert_eq!(
                *kind,
                distill_shell::session::unified_list::SessionKind::Build,
                "history-bypass local build under sticky --chat must unpin as build"
            );
        }
        other => panic!("expected ResetSessionTitle, got {other:?}"),
    }
    assert!(
        app.agents
            .get(&AgentId(0))
            .is_some_and(|a| a.display_name.is_none())
    );
    assert_eq!(
        app.agents
            .get(&AgentId(0))
            .and_then(|a| a.generated_session_title.as_deref()),
        Some("Auto")
    );
}

#[test]
fn dispatch_reset_session_title_refuses_chat_kind() {
    let mut app = test_app_with_agent();
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.chat_kind = true;
        agent.conversation_entry = true;
        agent.display_name = Some("Chat title".into());
        agent.generated_session_title = Some("Kept".into());
    }
    let Some(agent) = app.agents.get(&AgentId(0)) else {
        panic!("expected agent 0");
    };
    let scrollback_len_before = agent.scrollback.len();
    let effects = dispatch_reset_session_title(&mut app);
    assert!(
        effects.is_empty(),
        "chat-kind unpin must not emit an effect, got {effects:?}"
    );
    let Some(agent) = app.agents.get(&AgentId(0)) else {
        panic!("expected agent 0");
    };
    assert_eq!(agent.display_name.as_deref(), Some("Chat title"));
    assert_eq!(agent.generated_session_title.as_deref(), Some("Kept"));
    assert_eq!(agent.scrollback.len(), scrollback_len_before + 1);
    let last = agent
        .scrollback
        .entry(agent.scrollback.len() - 1)
        .expect("last entry");
    let text = match &last.block {
        crate::scrollback::block::RenderBlock::System(b) => b.text.clone(),
        other => panic!("expected System block, got {other:?}"),
    };
    assert!(
        text.contains("Chat conversations have no auto-title to restore"),
        "got: {text:?}"
    );
}

/// `ConfirmResetSetting { choice: Reset }` on a shared Bool target restores the Settings modal.
/// It also fires the typed `Action::SetCompactMode(default)` via recursive dispatch; the `Effect::PersistSetting` is the observable signal.
/// Also asserts the ui_snapshot was refreshed to the new (post-reset) value (symmetric with the Cancel test's snapshot assertion).
#[test]
fn dispatch_confirm_reset_setting_reset_dispatches_typed_setter_for_shared_bool() {
    use crate::settings::SettingValue;
    use crate::views::modal::{ActiveModal, ResetSettingsResult};
    let mut app = test_app_with_agent();
    // Flip compact_mode to true so we can observe the reset back to its default (false)
    let _ = dispatch(Action::SetCompactMode(true), &mut app);
    assert!(app.current_ui.compact_mode);

    setup_reset_confirm_open(&mut app, "compact_mode");

    let effects = dispatch(
        Action::ConfirmResetSetting {
            choice: ResetSettingsResult::Reset,
        },
        &mut app,
    );

    // Recursive dispatch into Action::SetCompactMode(false) emits the persist effect
    assert_eq!(effects.len(), 1);
    match effects.first() {
        Some(Effect::PersistSetting { key, value, .. }) => {
            assert_eq!(*key, "compact_mode");
            assert_eq!(value, &SettingValue::Bool(false));
        }
        other => panic!("expected PersistSetting, got {other:?}"),
    }
    // In-memory state is reset to the default.
    assert!(!app.current_ui.compact_mode);
    // The modal is restored and ui_snapshot reflects the new value (symmetric with the Cancel test)
    let agent = app.agents.get(&AgentId(0)).expect("agent must exist");
    match &agent.active_modal {
        Some(ActiveModal::Settings { state }) => {
            assert!(
                !state.ui_snapshot.compact_mode,
                "ui_snapshot must reflect the post-reset value"
            );
        }
        _ => panic!("Reset branch must restore the Settings modal"),
    }
}

/// `ConfirmResetSetting { choice: Reset }` on a shared Enum target (`theme`) dispatches `Action::SetTheme(default)` via recursive dispatch.
/// Verifies the action_for_reset Enum arm.
#[test]
fn dispatch_confirm_reset_setting_reset_dispatches_typed_setter_for_shared_enum() {
    use crate::settings::SettingValue;
    use crate::views::modal::ResetSettingsResult;
    // SetTheme mutates the global theme cache, so serialize with the other theme tests via the theme test lock
    with_theme_test_env(|| {
        let mut app = test_app_with_agent();
        // Flip theme to a non-default first.
        let _ = dispatch(Action::SetTheme("tokyonight".to_string()), &mut app);
        assert_eq!(app.current_ui.theme.as_deref(), Some("tokyonight"));

        setup_reset_confirm_open(&mut app, "theme");

        let effects = dispatch(
            Action::ConfirmResetSetting {
                choice: ResetSettingsResult::Reset,
            },
            &mut app,
        );

        // Reset dispatches SetTheme("groknight"), the registered default
        assert_eq!(effects.len(), 1);
        match effects.first() {
            Some(Effect::PersistSetting { key, value, .. }) => {
                assert_eq!(*key, "theme");
                assert_eq!(value, &SettingValue::Enum("groknight"));
            }
            other => panic!("expected PersistSetting, got {other:?}"),
        }
        assert_eq!(app.current_ui.theme.as_deref(), Some("groknight"));
    });
}

fn seed_scrolled_up(app: &mut AppView) {
    let sb = &mut app.agents.get_mut(&AgentId(0)).unwrap().scrollback;
    for i in 0..40 {
        sb.push_block(RenderBlock::agent_message(format!("seed {i}")));
    }
    sb.prepare_layout(80, 8);
    sb.goto_top();
}

fn current_usage_nonce(app: &AppView) -> u64 {
    let Some(agent) = app.agents.get(&AgentId(0)) else {
        panic!("expected agent 0");
    };
    match agent.active_modal.as_ref() {
        Some(crate::views::modal::ActiveModal::UsageInfo { state }) => state.fetch_nonce,
        _ => 0,
    }
}

fn complete_session_usage(app: &mut AppView) {
    let nonce = current_usage_nonce(app);
    dispatch(
        Action::TaskComplete(TaskResult::SessionUsageComplete {
            agent_id: AgentId(0),
            session_id: "test-session".to_string().into(),
            usage: Box::default(),
            nonce,
        }),
        app,
    );
}

fn context_info_response() -> distill_shell::session::SessionInfoResponse {
    use distill_shell::session::acp_types::{ContextInfo, SessionInfoData};

    distill_shell::session::SessionInfoResponse {
        session_id: "test-session".to_string(),
        cwd: "/tmp/test".to_string(),
        data: SessionInfoData {
            agent_name: None,
            model: Some("Distill".to_string()),
            model_display_name: None,
            resolved_model_id: None,
            model_fingerprint: None,
            show_model_fingerprint: false,
            api_backend: None,
            conversation_id: None,
            turns: 0,
            turn_index: 0,
            context: ContextInfo::default(),
        },
    }
}

#[test]
fn stale_context_info_results_do_not_update_replaced_session() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let before = agent_scrollback_len(&app);
    app.agents
        .get_mut(&id)
        .unwrap()
        .bind_session_id("replacement".into());

    dispatch(
        Action::TaskComplete(TaskResult::ContextInfoComplete {
            agent_id: id,
            session_id: "test-session".into(),
            info: Box::new(context_info_response()),
            nonce: Default::default(),
        }),
        &mut app,
    );
    dispatch(
        Action::TaskComplete(TaskResult::ContextInfoFailed {
            agent_id: id,
            session_id: "test-session".into(),
            error: "request failed".to_string(),
            nonce: Default::default(),
        }),
        &mut app,
    );

    assert_eq!(agent_scrollback_len(&app), before);
}

#[test]
fn session_usage_page_flips_info_to_top() {
    crate::appearance::cache::set_page_flip_on_send(true);
    let mut app = test_app_with_agent();
    // Scrollback flow is minimal-only.
    app.screen_mode = crate::app::ScreenMode::Minimal;
    app.usage_visible = false;
    seed_scrolled_up(&mut app);
    complete_session_usage(&mut app);
    let sb = &mut app.agents.get_mut(&AgentId(0)).unwrap().scrollback;
    sb.prepare_layout(80, 8);
    assert!(sb.is_follow_preserve_scroll());
    let pinned = sb.scroll_offset();
    sb.scroll_to_entry_top(sb.len() - 1);
    assert_eq!(sb.scroll_offset(), pinned);
}

#[test]
fn session_usage_keeps_scroll_when_page_flip_off() {
    let prev = crate::appearance::cache::load_page_flip_on_send();
    crate::appearance::cache::set_page_flip_on_send(false);
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    app.usage_visible = false;
    seed_scrolled_up(&mut app);
    complete_session_usage(&mut app);
    assert_eq!(
        app.agents
            .get(&AgentId(0))
            .map(|a| a.scrollback.scroll_offset()),
        Some(0)
    );
    crate::appearance::cache::set_page_flip_on_send(prev);
}

#[test]
fn show_usage_on_welcome_screen_is_noop() {
    let mut app = test_app();
    let effects = dispatch(Action::ShowUsage, &mut app);
    assert!(
        effects.is_empty(),
        "ShowUsage with no active agent should be a no-op"
    );
}

#[test]
fn show_usage_with_redirect_url_fetches_session_only() {
    // Redirect link is deferred until SessionUsageComplete (see billing tests).
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    app.usage_billing_redirect_url = Some("https://billing.example.com/me".to_string());
    let before = agent_scrollback_len(&app);
    let effects = dispatch(Action::ShowUsage, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::FetchSessionUsage { agent_id, .. }] if *agent_id == AgentId(0)
        ),
        "got: {effects:?}"
    );
    assert_eq!(agent_scrollback_len(&app), before);
}

#[test]
fn minimal_update_notice_commits_a_system_block() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    commit_minimal_update_notice(&mut app, "9.9.9");
    assert_eq!(agent_scrollback_len(&app), before + 1);
    let text = last_system_text(&app, AgentId(0));
    assert!(text.contains("Update available: v9.9.9"), "got: {text:?}");
    assert!(text.contains("Restart to apply."), "got: {text:?}");
}

#[test]
fn minimal_update_notice_no_active_agent_is_noop() {
    let mut app = test_app();
    // Must not panic and must not require an agent.
    commit_minimal_update_notice(&mut app, "9.9.9");
}

/// `/tutorial` (and the palette entry) open the overlay; dispatching again while open toggles it closed.
/// No side effects either way.
#[test]
fn open_tutorial_toggles_overlay_without_effects() {
    let mut app = test_app();
    let effects = dispatch(Action::OpenTutorial, &mut app);
    assert!(app.tutorial.is_some(), "tutorial opens");
    assert!(effects.is_empty(), "open emits nothing, got: {effects:?}");

    let effects = dispatch(Action::OpenTutorial, &mut app);
    assert!(app.tutorial.is_none(), "toggle closes");
    assert!(effects.is_empty(), "close emits nothing, got: {effects:?}");
}

#[test]
fn onboarding_completion_waits_for_persistence_and_keeps_failed_flow_open() {
    let mut app = test_app();
    dispatch(Action::OpenOnboarding, &mut app);
    let effects = dispatch(Action::CompleteOnboarding, &mut app);
    assert!(matches!(
        effects.as_slice(),
        [Effect::PersistSetting {
            key: "onboarding_completed",
            value: crate::settings::SettingValue::Bool(true),
            ..
        }]
    ));
    assert!(!app.current_ui.onboarding_completed);
    assert!(app.onboarding.is_some());

    dispatch(
        Action::TaskComplete(TaskResult::SettingPersistFailed {
            key: "onboarding_completed",
            rollback_value: crate::settings::SettingValue::Bool(false),
            error: "disk full".to_owned(),
        }),
        &mut app,
    );
    assert!(!app.current_ui.onboarding_completed);
    assert!(app.onboarding.is_some());

    dispatch(
        Action::TaskComplete(TaskResult::SettingPersisted {
            key: "onboarding_completed",
            value: crate::settings::SettingValue::Bool(true),
        }),
        &mut app,
    );
    assert!(app.current_ui.onboarding_completed);
    assert!(app.onboarding.is_none());
}

#[test]
fn reopening_completed_onboarding_finishes_without_a_second_persist() {
    let mut app = test_app();
    app.current_ui.onboarding_completed = true;
    dispatch(Action::OpenOnboarding, &mut app);
    assert!(app.onboarding.is_some());

    let effects = dispatch(Action::CompleteOnboarding, &mut app);

    assert!(
        effects.is_empty(),
        "completed onboarding must not persist twice"
    );
    assert!(
        app.onboarding.is_none(),
        "reopened flow must close on Finish"
    );
    assert!(app.onboarding_resume.is_none());
}

#[test]
fn onboarding_selection_status_tracks_persisted_results() {
    let mut app = test_app();
    dispatch(Action::OpenOnboarding, &mut app);
    app.onboarding
        .as_mut()
        .expect("onboarding opened")
        .set_primary_model_pending();
    dispatch(
        Action::TaskComplete(TaskResult::SettingPersisted {
            key: "default_model",
            value: crate::settings::SettingValue::String("model-a".to_owned()),
        }),
        &mut app,
    );
    let state = app.onboarding.as_ref().unwrap();
    assert!(!state.status_is_error);
    assert!(state.status.as_deref().unwrap().contains("saved"));

    app.onboarding.as_mut().unwrap().set_worker_model_pending();
    dispatch(
        Action::TaskComplete(TaskResult::SettingPersistFailedBestEffort {
            key: "tier_light",
            error: "disk full".to_owned(),
        }),
        &mut app,
    );
    let state = app.onboarding.as_ref().unwrap();
    assert!(state.status_is_error);
    assert!(state.status.as_deref().unwrap().contains("not saved"));
}

fn usage_modal_state(app: &AppView) -> &crate::views::usage_modal::UsageInfoModalState {
    match app
        .agents
        .get(&AgentId(0))
        .and_then(|a| a.active_modal.as_ref())
    {
        Some(crate::views::modal::ActiveModal::UsageInfo { state }) => state,
        _ => panic!("expected the usage modal to be open"),
    }
}

#[test]
fn show_usage_opens_modal_on_usage_limit_tab_with_fetches() {
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::ShowUsage, &mut app);
    let state = usage_modal_state(&app);
    assert_eq!(
        state.active_tab,
        crate::views::usage_modal::UsageInfoTab::UsageLimit
    );
    assert_eq!(state.ctx.session_id.as_deref(), Some("test-session"));
    assert!(state.billing_loading);
    assert!(
        matches!(
            effects.as_slice(),
            [
                Effect::ShowContextInfo { .. },
                Effect::ShowSessionInfo { .. },
                Effect::FetchSessionUsage { .. },
                Effect::FetchBilling { silent: true, .. },
            ]
        ),
        "got: {effects:?}"
    );
}

#[test]
fn show_context_info_retabs_open_modal_without_refetching() {
    let mut app = test_app_with_agent();
    dispatch(Action::ShowUsage, &mut app);
    let effects = dispatch(Action::ShowContextInfo, &mut app);
    assert!(effects.is_empty(), "got: {effects:?}");
    assert_eq!(
        usage_modal_state(&app).active_tab,
        crate::views::usage_modal::UsageInfoTab::ContextUsage
    );
}

#[test]
fn show_session_info_opens_modal_on_session_tab() {
    let mut app = test_app_with_agent();
    dispatch(Action::ShowSessionInfo, &mut app);
    assert_eq!(
        usage_modal_state(&app).active_tab,
        crate::views::usage_modal::UsageInfoTab::SessionInfo
    );
}

#[test]
fn usage_results_populate_open_modal_not_scrollback() {
    let mut app = test_app_with_agent();
    dispatch(Action::ShowUsage, &mut app);
    let before = agent_scrollback_len(&app);

    let nonce = current_usage_nonce(&app);
    complete_session_usage(&mut app);
    dispatch(
        Action::TaskComplete(TaskResult::SessionInfoComplete {
            agent_id: AgentId(0),
            session_id: "test-session".into(),
            info: Box::new(context_info_response()),
            text: "  Session ID: test-session".to_string(),
            fields: vec![crate::views::usage_modal::SessionInfoField {
                label: "Session ID",
                value: "test-session".to_string(),
                compact: false,
            }],
            nonce,
        }),
        &mut app,
    );
    dispatch(
        Action::TaskComplete(TaskResult::ContextInfoComplete {
            agent_id: AgentId(0),
            session_id: "test-session".into(),
            info: Box::new(context_info_response()),
            nonce,
        }),
        &mut app,
    );

    assert_eq!(agent_scrollback_len(&app), before);
    let state = usage_modal_state(&app);
    assert!(state.session_usage_text.is_some());
    let fields = state
        .session_fields
        .as_ref()
        .expect("session fields populated");
    assert_eq!(fields.len(), 1);
    let Some(field) = fields.first() else {
        panic!("expected a session field: {fields:?}");
    };
    assert_eq!(field.value, "test-session");
    assert!(state.context.is_some());
}

#[test]
fn usage_results_without_open_modal_are_dropped_in_full_mode() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    complete_session_usage(&mut app);
    dispatch(
        Action::TaskComplete(TaskResult::SessionInfoFailed {
            agent_id: AgentId(0),
            session_id: "test-session".into(),
            error: "boom".to_string(),
            nonce: Default::default(),
        }),
        &mut app,
    );
    dispatch(
        Action::TaskComplete(TaskResult::ContextInfoFailed {
            agent_id: AgentId(0),
            session_id: "test-session".into(),
            error: "boom".to_string(),
            nonce: Default::default(),
        }),
        &mut app,
    );
    assert_eq!(agent_scrollback_len(&app), before);
}

#[test]
fn reply_from_previous_modal_open_is_dropped() {
    let mut app = test_app_with_agent();
    dispatch(Action::ShowUsage, &mut app);
    let old_nonce = current_usage_nonce(&app);
    // Close and reopen on the same session: a new fetch generation.
    app.agents.get_mut(&AgentId(0)).unwrap().active_modal = None;
    dispatch(Action::ShowUsage, &mut app);
    assert_ne!(current_usage_nonce(&app), old_nonce);
    // The first open's reply lands late; it must not populate the modal
    dispatch(
        Action::TaskComplete(TaskResult::SessionInfoComplete {
            agent_id: AgentId(0),
            session_id: "test-session".into(),
            info: Box::new(context_info_response()),
            text: "  Session ID: from-old-open".to_string(),
            fields: vec![crate::views::usage_modal::SessionInfoField {
                label: "Session ID",
                value: "from-old-open".to_string(),
                compact: false,
            }],
            nonce: old_nonce,
        }),
        &mut app,
    );
    assert!(usage_modal_state(&app).session_fields.is_none());
}

#[test]
fn stale_session_info_does_not_populate_modal() {
    let mut app = test_app_with_agent();
    dispatch(Action::ShowSessionInfo, &mut app);
    let nonce = current_usage_nonce(&app);
    dispatch(
        Action::TaskComplete(TaskResult::SessionInfoComplete {
            agent_id: AgentId(0),
            session_id: "old-session".into(),
            info: Box::new(context_info_response()),
            text: "  Session ID: old-session".to_string(),
            fields: vec![crate::views::usage_modal::SessionInfoField {
                label: "Session ID",
                value: "old-session".to_string(),
                compact: false,
            }],
            nonce,
        }),
        &mut app,
    );
    assert!(usage_modal_state(&app).session_fields.is_none());
}

#[test]
fn fetch_failures_surface_in_open_modal() {
    let mut app = test_app_with_agent();
    dispatch(Action::ShowUsage, &mut app);
    let nonce = current_usage_nonce(&app);
    dispatch(
        Action::TaskComplete(TaskResult::SessionInfoFailed {
            agent_id: AgentId(0),
            session_id: "test-session".into(),
            error: "info boom".to_string(),
            nonce,
        }),
        &mut app,
    );
    dispatch(
        Action::TaskComplete(TaskResult::ContextInfoFailed {
            agent_id: AgentId(0),
            session_id: "test-session".into(),
            error: "ctx boom".to_string(),
            nonce,
        }),
        &mut app,
    );
    let state = usage_modal_state(&app);
    assert_eq!(state.session_error.as_deref(), Some("info boom"));
    assert_eq!(state.context_error.as_deref(), Some("ctx boom"));
}
