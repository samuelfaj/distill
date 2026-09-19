// Modified for Distill by Samuel Fajreldines, 2026.
//! Wraps [`distill_workspace::session::git::build_restore_decision`] into the JSON shape `LoadSession` emits on `_meta.codeRestore`.
use distill_workspace::session::git::{
    CheckoutSessionOutcome, RestoreKind, build_restore_decision,
};
use serde_json::Value;
/// Builds the `codeRestore` JSON meta, or `None` when there was neither a checkout nor an applied archive.
/// The shared [`build_restore_decision`] makes the decision; this function only reshapes it into the wire JSON used by the non-worktree path.
pub(crate) fn build_code_restore_meta(
    target_sha: &str,
    outcome: &CheckoutSessionOutcome,
    kind: RestoreKind,
) -> Option<Value> {
    let decision = build_restore_decision(Some(target_sha), outcome, kind);
    let summary = decision.summary?;
    Some(serde_json::json!({
        "restored": decision.restored,
        "summary": summary,
        "degree": decision.degree,
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn outcome(
        checked_out: bool,
        stash_ref: Option<&str>,
        skipped: Option<&str>,
    ) -> CheckoutSessionOutcome {
        CheckoutSessionOutcome {
            checked_out,
            stash_ref: stash_ref.map(str::to_owned),
            stash_skipped_reason: skipped.map(str::to_owned),
        }
    }
    #[test]
    fn checkout_failed_emits_restored_false_meta() {
        let meta = build_code_restore_meta(
            "0123456789abcdef",
            &outcome(false, None, Some("MERGE_HEAD present")),
            RestoreKind::RegistryOff,
        )
        .unwrap();
        assert_eq!(meta.get("restored").and_then(|v| v.as_bool()), Some(false));
        assert!(meta.get("degree").is_some_and(|v| v.is_null()));
        let Some(s) = meta.get("summary").and_then(|v| v.as_str()) else {
            panic!("summary missing: {meta:?}");
        };
        assert!(s.contains("restore aborted"));
        assert!(s.contains("MERGE_HEAD present"));
    }
}
