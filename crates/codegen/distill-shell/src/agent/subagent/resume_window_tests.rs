// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

fn policy(context_window: u64, auto_compact_threshold_percent: u8) -> ResumeWindowPolicy {
    ResumeWindowPolicy {
        context_window,
        auto_compact_threshold_percent,
    }
}

#[test]
fn fit_percent_clamps_to_min_of_prelude_reserve_and_threshold() {
    let tight = policy(1_000, 85);
    assert_eq!(tight.token_limit(), 850);
    assert!(tight.fits(850));
    assert!(!tight.fits(851));

    let loose = policy(1_000, 99);
    assert_eq!(loose.token_limit(), 950);
    assert!(loose.fits(950));
    assert!(!loose.fits(951));
}

#[test]
fn rejects_missing_windows_and_copies_without_headroom() {
    let window = policy(1_000, 85);
    assert!(!policy(0, 85).fits(1));
    assert!(!window.fits(851));
    assert!(!window.fits(1_000));
    assert!(!window.fits(1_001));
    assert!(window.fits(849));
    assert!(!window.over_auto_compact_threshold(849));
}

#[test]
fn over_auto_compact_threshold_uses_the_auto_compact_boundary() {
    let window = policy(1_000, 85);
    assert!(window.over_auto_compact_threshold(850));
    assert!(!window.over_auto_compact_threshold(849));
}

#[test]
fn budgeted_child_aborts_instead_of_arming_a_noop_compact() {
    let window = policy(1_000, 85);
    assert_eq!(
        window.plan_force_compact(900, false),
        ResumeForceCompact::Arm
    );
    assert_eq!(
        window.plan_force_compact(900, true),
        ResumeForceCompact::AbortBudgeted
    );
    assert_eq!(
        window.plan_force_compact(849, true),
        ResumeForceCompact::NotNeeded
    );
}

#[test]
fn arm_force_compact_stores_only_when_needed() {
    let force_compact = std::sync::atomic::AtomicBool::new(false);
    arm_force_compact(&force_compact, false);
    assert!(!force_compact.load(std::sync::atomic::Ordering::Relaxed));

    arm_force_compact(&force_compact, true);
    assert!(force_compact.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn resumed_prefix_keeps_only_the_system_head() {
    use distill_sampling_types::conversation::ConversationItem;

    let conversation = vec![
        ConversationItem::system("system"),
        ConversationItem::user("prior work"),
        ConversationItem::assistant("done"),
    ];
    assert_eq!(resume_inherited_prefix_len(&conversation), 1);
}
