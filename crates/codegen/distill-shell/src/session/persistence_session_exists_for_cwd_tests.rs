// Modified for Distill by Samuel Fajreldines, 2026.
use super::{
    resolve_local_session_any_cwd_in_root, resolve_local_session_ids_any_cwd_in_root,
    session_exists_for_cwd_in_root, session_exists_in_root,
};
use std::fs;
use tempfile::TempDir;

#[test]
fn returns_true_when_session_exists_under_matching_cwd() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    let cwd = "/project/alpha";
    let session_id = "my-session";

    let encoded = crate::util::distill_home::encode_cwd_dirname(cwd);
    let dir = root.join(&encoded).join(session_id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("summary.json"), b"{}").unwrap();

    assert!(session_exists_for_cwd_in_root(session_id, cwd, &root));
}

#[test]
fn returns_false_when_session_absent_under_cwd() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    fs::create_dir_all(&root).unwrap();

    assert!(!session_exists_for_cwd_in_root(
        "missing",
        "/project/alpha",
        &root
    ));
}

/// Regression test for the cross-cwd false-positive.
/// Before the fix, `restore_if_not_local` used `session_exists_by_id` which scanned ALL cwd directories.
/// A session present only under cwd-A would cause it to skip remote restore when the user resumed from cwd-B.
#[test]
fn session_under_different_cwd_is_not_considered_present() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    let session_id = "cross-cwd-session";

    // Create the session only under cwd-A (a real session has a summary.json).
    let encoded_a = crate::util::distill_home::encode_cwd_dirname("/project/alpha");
    let dir_a = root.join(&encoded_a).join(session_id);
    fs::create_dir_all(&dir_a).unwrap();
    fs::write(dir_a.join("summary.json"), b"{}").unwrap();

    // Global scan (old behaviour) finds it; this is the incorrect check
    assert!(
        session_exists_in_root(session_id, &root),
        "global scan must find the session under cwd-A"
    );

    assert!(
        !session_exists_for_cwd_in_root(session_id, "/project/beta", &root),
        "cwd-specific check must return false for cwd-B; remote restore must not be skipped"
    );

    assert!(
        session_exists_for_cwd_in_root(session_id, "/project/alpha", &root),
        "cwd-specific check must return true for the matching cwd-A"
    );
}

#[test]
fn images_only_stub_is_not_a_session() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    let cwd = "/project/alpha";
    let session_id = "stub-session";

    let encoded = crate::util::distill_home::encode_cwd_dirname(cwd);
    let images = root.join(&encoded).join(session_id).join("images");
    fs::create_dir_all(&images).unwrap();
    fs::write(images.join("image-1.png"), b"png").unwrap();

    assert!(
        !session_exists_for_cwd_in_root(session_id, cwd, &root),
        "an images-only stub (no summary.json) must not be a resumable session"
    );
}

#[test]
fn resolve_local_session_any_cwd_skips_stub_and_finds_real() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    let session_id = "real-session";

    // Real session under cwd-A.
    let cwd_a = "/project/alpha";
    let encoded_a = crate::util::distill_home::encode_cwd_dirname(cwd_a);
    let dir_a = root.join(&encoded_a).join(session_id);
    fs::create_dir_all(&dir_a).unwrap();
    fs::write(dir_a.join("summary.json"), b"{}").unwrap();

    // Images-only stub for the SAME id under cwd-B.
    let cwd_b = "/project/beta";
    let encoded_b = crate::util::distill_home::encode_cwd_dirname(cwd_b);
    let images_b = root.join(&encoded_b).join(session_id).join("images");
    fs::create_dir_all(&images_b).unwrap();
    fs::write(images_b.join("image-1.png"), b"png").unwrap();

    assert_eq!(
        resolve_local_session_any_cwd_in_root(session_id, &root)
            .unwrap()
            .as_deref(),
        Some(cwd_a),
        "must anchor to the real session's cwd, not the stub's"
    );
}

#[test]
fn batch_resolver_loads_one_view_and_keeps_only_persisted_sessions() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    let encoded = crate::util::distill_home::encode_cwd_dirname("/project/alpha");
    let persisted = root.join(&encoded).join("persisted");
    fs::create_dir_all(&persisted).unwrap();
    fs::write(persisted.join("summary.json"), b"{}").unwrap();
    let stub = root.join(&encoded).join("stub").join("images");
    fs::create_dir_all(&stub).unwrap();
    fs::write(stub.join("image.png"), b"png").unwrap();

    let resolved =
        resolve_local_session_ids_any_cwd_in_root(&["persisted", "stub", "missing"], &root)
            .unwrap();

    assert_eq!(
        resolved,
        std::collections::HashSet::from(["persisted".into()])
    );
}

#[test]
fn find_summary_by_session_id_reads_cross_cwd_uuid() {
    use super::find_summary_by_session_id_in_root;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    let session_id = "019f870d-6976-7d73-a12a-52e9d4aebcd4";
    let cwd = "/project/elsewhere";
    let encoded = crate::util::distill_home::encode_cwd_dirname(cwd);
    let dir = root.join(&encoded).join(session_id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("summary.json"),
        serde_json::json!({
            "info": { "id": session_id, "cwd": cwd },
            "session_summary": "cross-cwd hit",
            "created_at": "2026-03-01T00:00:00Z",
            "updated_at": "2026-03-01T00:00:00Z",
            "num_messages": 2,
            "num_chat_messages": 1,
            "current_model_id": "test",
        })
        .to_string(),
    )
    .unwrap();

    let summary = find_summary_by_session_id_in_root(session_id, &root)
        .expect("CLI --resume finds this summary by id across cwds");
    assert_eq!(summary.info.id.0.as_ref(), session_id);
    assert_eq!(summary.info.cwd, cwd);
    assert_eq!(summary.session_summary, "cross-cwd hit");
}
