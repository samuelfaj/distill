// Modified for Distill by Samuel Fajreldines, 2026.
use agent_client_protocol as acp;
use distill_test_support::EnvGuard;
use serial_test::serial;
use tempfile::TempDir;

use crate::session::info::Info;
use crate::session::persistence::{Summary, default_model_id};
use crate::session::storage::{JsonlStorageAdapter, StorageAdapter};

fn worktree_cwd_under(home: &std::path::Path) -> String {
    let cwd = home
        .join("worktrees")
        .join("xai")
        .join("fix-bug")
        .join("src");
    std::fs::create_dir_all(&cwd).unwrap();
    cwd.to_string_lossy().into_owned()
}

fn write_untagged_summary(session_dir: &std::path::Path, info: &Info) {
    std::fs::create_dir_all(session_dir).unwrap();
    let mut untagged = Summary::new(info, default_model_id()).unwrap();
    untagged.session_kind = None;
    untagged.worktree_label = None;
    untagged.source_workspace_dir = None;
    std::fs::write(
        session_dir.join("summary.json"),
        serde_json::to_vec_pretty(&untagged).unwrap(),
    )
    .unwrap();
}

fn set_summary_mtime(session_dir: &std::path::Path, mtime: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(session_dir.join("summary.json"))
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

fn read_summary_from(session_dir: &std::path::Path) -> Summary {
    serde_json::from_slice(&std::fs::read(session_dir.join("summary.json")).unwrap()).unwrap()
}

fn mark_summary_used(session_dir: &std::path::Path) {
    let mut summary = read_summary_from(session_dir);
    summary.num_messages = 1;
    std::fs::write(
        session_dir.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
#[serial]
async fn list_sessions_repairs_untagged_worktree_summary_in_rows_and_on_disk() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let info = Info {
        id: acp::SessionId::new("untagged-listed"),
        cwd: worktree_cwd_under(home.path()),
    };
    let adapter = JsonlStorageAdapter::with_root(home.path().to_path_buf());
    let session_dir = adapter.session_dir(&info);
    write_untagged_summary(&session_dir, &info);

    let listed = adapter.list_sessions(None).await.unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed.first().and_then(|x| x.session_kind.as_deref()),
        Some("worktree")
    );
    assert_eq!(
        listed.first().and_then(|x| x.worktree_label.as_deref()),
        Some("fix-bug")
    );
    let rows = crate::session::merge::merge(Vec::new(), listed, None, &[], 20);
    assert_eq!(
        rows.first().and_then(|x| x.session_kind.as_deref()),
        Some("worktree")
    );
    assert_eq!(
        rows.first().and_then(|x| x.worktree_label.as_deref()),
        Some("fix-bug")
    );
    let on_disk = read_summary_from(&session_dir);
    assert_eq!(on_disk.session_kind.as_deref(), Some("worktree"));
    assert_eq!(on_disk.worktree_label.as_deref(), Some("fix-bug"));
    assert!(on_disk.source_workspace_dir.is_none());
}

#[tokio::test]
#[serial]
async fn list_sessions_fills_missing_label_on_kinded_fork_without_changing_kind() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let info = Info {
        id: acp::SessionId::new("kinded-listed"),
        cwd: worktree_cwd_under(home.path()),
    };
    let adapter = JsonlStorageAdapter::with_root(home.path().to_path_buf());
    let session_dir = adapter.session_dir(&info);
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut kinded = Summary::new(&info, default_model_id()).unwrap();
    kinded.session_kind = Some("fork".to_owned());
    kinded.worktree_label = None;
    kinded.source_workspace_dir = Some("/home/user/repo".to_owned());
    std::fs::write(
        session_dir.join("summary.json"),
        serde_json::to_vec_pretty(&kinded).unwrap(),
    )
    .unwrap();

    let listed = adapter.list_sessions(None).await.unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed.first().and_then(|x| x.session_kind.as_deref()),
        Some("fork")
    );
    assert_eq!(
        listed.first().and_then(|x| x.worktree_label.as_deref()),
        Some("fix-bug")
    );
    assert_eq!(
        listed
            .first()
            .and_then(|x| x.source_workspace_dir.as_deref()),
        Some("/home/user/repo")
    );
    let rows = crate::session::merge::merge(Vec::new(), listed, None, &[], 20);
    assert_eq!(
        rows.first().and_then(|x| x.session_kind.as_deref()),
        Some("fork")
    );
    assert_eq!(
        rows.first().and_then(|x| x.worktree_label.as_deref()),
        Some("fix-bug")
    );
    let on_disk = read_summary_from(&session_dir);
    assert_eq!(on_disk.session_kind.as_deref(), Some("fork"));
    assert_eq!(on_disk.worktree_label.as_deref(), Some("fix-bug"));
    assert_eq!(
        on_disk.source_workspace_dir.as_deref(),
        Some("/home/user/repo")
    );
}

#[tokio::test]
#[serial]
async fn list_sessions_leaves_kinded_labeled_worktree_summary_untouched() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let info = Info {
        id: acp::SessionId::new("kinded-labeled"),
        cwd: worktree_cwd_under(home.path()),
    };
    let adapter = JsonlStorageAdapter::with_root(home.path().to_path_buf());
    let session_dir = adapter.session_dir(&info);
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut kinded = Summary::new(&info, default_model_id()).unwrap();
    kinded.session_kind = Some("fork".to_owned());
    kinded.worktree_label = Some("existing".to_owned());
    kinded.source_workspace_dir = Some("/home/user/repo".to_owned());
    std::fs::write(
        session_dir.join("summary.json"),
        serde_json::to_vec_pretty(&kinded).unwrap(),
    )
    .unwrap();
    let bytes_before = std::fs::read(session_dir.join("summary.json")).unwrap();

    let listed = adapter.list_sessions(None).await.unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed.first().and_then(|x| x.session_kind.as_deref()),
        Some("fork")
    );
    assert_eq!(
        listed.first().and_then(|x| x.worktree_label.as_deref()),
        Some("existing")
    );
    assert_eq!(
        std::fs::read(session_dir.join("summary.json")).unwrap(),
        bytes_before
    );
}

#[tokio::test]
#[serial]
async fn list_sessions_leaves_untagged_summary_outside_worktrees_untouched() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let plain_cwd = home.path().join("project");
    std::fs::create_dir_all(&plain_cwd).unwrap();
    let info = Info {
        id: acp::SessionId::new("plain-listed"),
        cwd: plain_cwd.to_string_lossy().into_owned(),
    };
    let adapter = JsonlStorageAdapter::with_root(home.path().to_path_buf());
    let session_dir = adapter.session_dir(&info);
    write_untagged_summary(&session_dir, &info);
    let bytes_before = std::fs::read(session_dir.join("summary.json")).unwrap();

    let listed = adapter.list_sessions(None).await.unwrap();

    assert_eq!(listed.len(), 1);
    assert!(listed.first().is_some_and(|x| x.session_kind.is_none()));
    assert!(listed.first().is_some_and(|x| x.worktree_label.is_none()));
    assert_eq!(
        std::fs::read(session_dir.join("summary.json")).unwrap(),
        bytes_before
    );
}

#[tokio::test]
#[serial]
async fn repair_keeps_summary_untagged_when_locked_write_fails() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let info = Info {
        id: acp::SessionId::new("unwritable-repair"),
        cwd: worktree_cwd_under(home.path()),
    };
    let mut summary = Summary::new(&info, default_model_id()).unwrap();
    summary.session_kind = None;
    summary.worktree_label = None;
    summary.source_workspace_dir = None;
    let missing_dir = home.path().join("does-not-exist");

    crate::session::storage::summary_write::repair_untagged_worktree_summary(
        &mut summary,
        &missing_dir.join("summary.json"),
        &missing_dir.join("summary.json.lock"),
    );

    assert!(summary.session_kind.is_none());
    assert!(summary.worktree_label.is_none());
}

#[tokio::test]
#[serial]
async fn repair_adopts_kinded_summary_when_mtime_restore_fails() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let info = Info {
        id: acp::SessionId::new("mtime-restore-miss"),
        cwd: worktree_cwd_under(home.path()),
    };
    let adapter = JsonlStorageAdapter::with_root(home.path().to_path_buf());
    let session_dir = adapter.session_dir(&info);
    write_untagged_summary(&session_dir, &info);
    let mut summary = read_summary_from(&session_dir);
    crate::session::storage::summary_write::fail_next_restore_summary_mtime();

    crate::session::storage::summary_write::repair_untagged_worktree_summary(
        &mut summary,
        &session_dir.join("summary.json"),
        &session_dir.join("summary.json.lock"),
    );

    assert_eq!(summary.session_kind.as_deref(), Some("worktree"));
    assert_eq!(summary.worktree_label.as_deref(), Some("fix-bug"));
    assert_eq!(
        read_summary_from(&session_dir).session_kind.as_deref(),
        Some("worktree")
    );
}

#[tokio::test]
#[serial]
async fn init_session_load_backfills_worktree_identity_on_untagged_summary() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let info = Info {
        id: acp::SessionId::new("legacy-worktree-session"),
        cwd: worktree_cwd_under(home.path()),
    };
    let session_dir = home.path().join("session");
    write_untagged_summary(&session_dir, &info);

    let adapter = JsonlStorageAdapter::with_explicit_session_dir(session_dir.clone());
    let loaded = adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    assert_eq!(loaded.session_kind.as_deref(), Some("worktree"));
    assert_eq!(loaded.worktree_label.as_deref(), Some("fix-bug"));
    assert!(loaded.source_workspace_dir.is_none());
    let on_disk: Summary =
        serde_json::from_slice(&std::fs::read(session_dir.join("summary.json")).unwrap()).unwrap();
    assert_eq!(on_disk.session_kind.as_deref(), Some("worktree"));
    assert_eq!(on_disk.worktree_label.as_deref(), Some("fix-bug"));
    assert!(on_disk.source_workspace_dir.is_none());
}

#[tokio::test]
#[serial]
async fn init_session_load_leaves_untagged_summary_outside_worktrees_untouched() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let plain_cwd = home.path().join("project");
    std::fs::create_dir_all(&plain_cwd).unwrap();
    let info = Info {
        id: acp::SessionId::new("plain-cwd-session"),
        cwd: plain_cwd.to_string_lossy().into_owned(),
    };
    let session_dir = home.path().join("session");
    write_untagged_summary(&session_dir, &info);
    let bytes_before = std::fs::read(session_dir.join("summary.json")).unwrap();

    let adapter = JsonlStorageAdapter::with_explicit_session_dir(session_dir.clone());
    let loaded = adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    assert!(loaded.session_kind.is_none());
    assert!(loaded.worktree_label.is_none());
    assert_eq!(
        std::fs::read(session_dir.join("summary.json")).unwrap(),
        bytes_before
    );
}

#[tokio::test]
#[serial]
async fn init_session_load_fills_missing_label_on_kinded_fork_without_changing_kind() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let info = Info {
        id: acp::SessionId::new("kinded-load"),
        cwd: worktree_cwd_under(home.path()),
    };
    let session_dir = home.path().join("session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut kinded = Summary::new(&info, default_model_id()).unwrap();
    kinded.session_kind = Some("fork".to_owned());
    kinded.worktree_label = None;
    kinded.source_workspace_dir = Some("/home/user/repo".to_owned());
    std::fs::write(
        session_dir.join("summary.json"),
        serde_json::to_vec_pretty(&kinded).unwrap(),
    )
    .unwrap();

    let adapter = JsonlStorageAdapter::with_explicit_session_dir(session_dir.clone());
    let loaded = adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    assert_eq!(loaded.session_kind.as_deref(), Some("fork"));
    assert_eq!(loaded.worktree_label.as_deref(), Some("fix-bug"));
    assert_eq!(
        loaded.source_workspace_dir.as_deref(),
        Some("/home/user/repo")
    );
    let on_disk = read_summary_from(&session_dir);
    assert_eq!(on_disk.session_kind.as_deref(), Some("fork"));
    assert_eq!(on_disk.worktree_label.as_deref(), Some("fix-bug"));
    assert_eq!(
        on_disk.source_workspace_dir.as_deref(),
        Some("/home/user/repo")
    );
}

#[tokio::test]
#[serial]
async fn list_sessions_recent_fills_missing_label_on_kinded_fork_without_changing_kind() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let info = Info {
        id: acp::SessionId::new("kinded-recent"),
        cwd: worktree_cwd_under(home.path()),
    };
    let adapter = JsonlStorageAdapter::with_root(home.path().to_path_buf());
    let session_dir = adapter.session_dir(&info);
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut kinded = Summary::new(&info, default_model_id()).unwrap();
    kinded.session_kind = Some("fork".to_owned());
    kinded.worktree_label = None;
    kinded.source_workspace_dir = Some("/home/user/repo".to_owned());
    std::fs::write(
        session_dir.join("summary.json"),
        serde_json::to_vec_pretty(&kinded).unwrap(),
    )
    .unwrap();

    let listed = adapter.list_sessions_recent(10).await.unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed.first().and_then(|x| x.session_kind.as_deref()),
        Some("fork")
    );
    assert_eq!(
        listed.first().and_then(|x| x.worktree_label.as_deref()),
        Some("fix-bug")
    );
    assert_eq!(
        listed
            .first()
            .and_then(|x| x.source_workspace_dir.as_deref()),
        Some("/home/user/repo")
    );
    let on_disk = read_summary_from(&session_dir);
    assert_eq!(on_disk.session_kind.as_deref(), Some("fork"));
    assert_eq!(on_disk.worktree_label.as_deref(), Some("fix-bug"));
}

#[tokio::test]
#[serial]
async fn list_sessions_recent_repairs_untagged_worktree_summary_in_rows_and_on_disk() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let info = Info {
        id: acp::SessionId::new("untagged-recent"),
        cwd: worktree_cwd_under(home.path()),
    };
    let adapter = JsonlStorageAdapter::with_root(home.path().to_path_buf());
    let session_dir = adapter.session_dir(&info);
    write_untagged_summary(&session_dir, &info);
    // Recent list omits empty untitled rows, so this must be a used row to observe the heal.
    mark_summary_used(&session_dir);

    let listed = adapter.list_sessions_recent(10).await.unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed.first().and_then(|x| x.session_kind.as_deref()),
        Some("worktree")
    );
    assert_eq!(
        listed.first().and_then(|x| x.worktree_label.as_deref()),
        Some("fix-bug")
    );
    let on_disk = read_summary_from(&session_dir);
    assert_eq!(on_disk.session_kind.as_deref(), Some("worktree"));
    assert_eq!(on_disk.worktree_label.as_deref(), Some("fix-bug"));
    assert!(on_disk.source_workspace_dir.is_none());
}

#[tokio::test]
#[serial]
async fn list_sessions_heal_does_not_evict_recent_sessions_from_mtime_window() {
    let home = TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let adapter = JsonlStorageAdapter::with_root(home.path().to_path_buf());
    let cwd = worktree_cwd_under(home.path());
    let now = std::time::SystemTime::now();
    let stale = now - std::time::Duration::from_secs(10 * 24 * 3600);

    for i in 0..3 {
        let info = Info {
            id: acp::SessionId::new(format!("old-untagged-{i}")),
            cwd: cwd.clone(),
        };
        let session_dir = adapter.session_dir(&info);
        write_untagged_summary(&session_dir, &info);
        set_summary_mtime(&session_dir, stale);
    }
    let recent = Info {
        id: acp::SessionId::new("recent-untagged"),
        cwd: cwd.clone(),
    };
    let recent_dir = adapter.session_dir(&recent);
    write_untagged_summary(&recent_dir, &recent);
    // Used so the mtime assertion is not the husk filter dropping an empty row.
    mark_summary_used(&recent_dir);
    set_summary_mtime(&recent_dir, now);

    let listed = adapter.list_sessions(None).await.unwrap();
    assert_eq!(listed.len(), 4);
    assert!(
        listed
            .iter()
            .all(|s| s.session_kind.as_deref() == Some("worktree"))
    );

    let window = adapter.list_sessions_recent(1).await.unwrap();
    assert_eq!(window.len(), 1);
    assert_eq!(
        window.first().map(|w| w.info.id.0.to_string()).as_deref(),
        Some("recent-untagged")
    );
}
