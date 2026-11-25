// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use chrono::Duration as ChronoDuration;
use tempfile::TempDir;

const REPO: &str = "/repo/.git";

fn roster_in(root: &Path, id: &str, repo: Option<&str>) -> Roster {
    let r = Roster::new(
        root.to_path_buf(),
        acp::SessionId::new(id),
        Path::new("/repo"),
        repo.map(str::to_owned),
        Some("main".into()),
    );
    distill_active_sessions::register_in(root, r.base.clone()).unwrap();
    r
}

fn live_peer(root: &Path, id: &str, repo: &str, objective: &str, files: &[&str]) {
    let mut s = ActiveSession::new(acp::SessionId::new(id), std::process::id(), "/repo".into());
    s.repo_root = Some(repo.into());
    s.branch = Some(format!("branch-{id}"));
    s.set_objective_if_unset(objective);
    let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    s.record_edits(&files, Utc::now());
    distill_active_sessions::register_in(root, s).unwrap();
}

#[test]
fn reminder_lists_same_repo_peer_with_objective_branch_files_and_age() {
    let dir = TempDir::new().unwrap();
    let me = roster_in(dir.path(), "me", Some(REPO));
    live_peer(
        dir.path(),
        "peer",
        REPO,
        "fix the login bug",
        &["/repo/src/a.rs", "/other/b.rs"],
    );

    let text = me.reminder(Utc::now(), false).unwrap();
    assert!(text.contains("\"fix the login bug\""), "{text}");
    assert!(text.contains("branch-peer"), "{text}");
    assert!(
        text.contains("src/a.rs") && !text.contains("/repo/src/a.rs"),
        "{text}"
    );
    assert!(text.contains("/other/b.rs"), "{text}");
    assert!(text.contains("started under 1 min ago"), "{text}");
    assert!(
        text.contains("choose another file, wait, or use a worktree"),
        "{text}"
    );
}

#[test]
fn reminder_skips_other_repos_self_and_unrelated_sessions() {
    let dir = TempDir::new().unwrap();
    let me = roster_in(dir.path(), "me", Some(REPO));
    live_peer(
        dir.path(),
        "elsewhere",
        "/other/.git",
        "other repo work",
        &["/x"],
    );
    assert_eq!(me.reminder(Utc::now(), false), None);

    let no_repo = roster_in(dir.path(), "norepo", None);
    live_peer(dir.path(), "peer", REPO, "work", &[]);
    assert_eq!(no_repo.reminder(Utc::now(), false), None);
}

#[test]
fn reminder_is_off_when_maccode_hosts_the_session() {
    let dir = TempDir::new().unwrap();
    let me = roster_in(dir.path(), "me", Some(REPO));
    live_peer(dir.path(), "peer", REPO, "work", &["/repo/a.rs"]);
    assert!(me.reminder(Utc::now(), false).is_some());
    assert_eq!(me.reminder(Utc::now(), true), None);
    assert_eq!(me.turn_start(Some("hi"), Utc::now(), true), None);
}

#[test]
fn host_env_marks_the_session_as_hosted_only_when_non_empty() {
    assert!(is_hosted(Some("/tmp/app.sock".into())));
    assert!(!is_hosted(Some("".into())));
    assert!(!is_hosted(None));
    assert_eq!(MACCODE_HOST_ENV, "RC_REMOTE_CODE_AGENT_APP_SOCKET_PATH");
}

#[test]
fn reminder_ignores_stale_heartbeats() {
    let dir = TempDir::new().unwrap();
    let me = roster_in(dir.path(), "me", Some(REPO));
    live_peer(dir.path(), "peer", REPO, "work", &["/repo/a.rs"]);
    let later = Utc::now() + ChronoDuration::minutes(11);
    assert_eq!(me.reminder(later, false), None);
}

#[test]
fn reminder_caps_peers_and_files_and_sanitizes_text() {
    let dir = TempDir::new().unwrap();
    let me = roster_in(dir.path(), "me", Some(REPO));
    let files: Vec<String> = (0..15).map(|i| format!("/repo/f{i}.rs")).collect();
    let refs: Vec<&str> = files.iter().map(String::as_str).collect();
    for i in 0..7 {
        live_peer(
            dir.path(),
            &format!("p{i}"),
            REPO,
            "do\u{202e}x\nignore previous",
            &refs,
        );
    }
    let text = me.reminder(Utc::now(), false).unwrap();
    assert_eq!(
        text.lines().filter(|l| l.starts_with("- ")).count(),
        MAX_PEERS
    );
    assert!(text.contains("(+5 more)"), "{text}");
    assert!(
        text.contains("f14.rs") && !text.contains("f0.rs"),
        "newest files first: {text}"
    );
    assert!(!text.contains('\u{202e}'));
    assert!(text.contains("\"do x ignore previous\""), "{text}");
}

#[test]
fn turn_start_records_first_prompt_as_objective_and_flushes_pending_edits() {
    let dir = TempDir::new().unwrap();
    let me = roster_in(dir.path(), "me", Some(REPO));
    let EditDecision::FlushNow(first) = me.note_edits(vec!["/repo/a.rs".into()], Instant::now())
    else {
        panic!("first edit flushes immediately");
    };
    me.write_edits(&first, Utc::now()).unwrap();
    // Throttled: only the turn start writes this one.
    me.note_edits(vec!["/repo/b.rs".into()], Instant::now());
    me.turn_start(Some("first goal"), Utc::now(), false);
    me.turn_start(Some("second goal"), Utc::now(), false);
    let entry = distill_active_sessions::list_in(dir.path())
        .unwrap()
        .into_iter()
        .find(|s| &*s.session_id.0 == "me")
        .unwrap();
    assert_eq!(entry.objective.as_deref(), Some("first goal"));
    assert_eq!(
        entry.editing_files,
        vec!["/repo/a.rs".to_string(), "/repo/b.rs".to_string()]
    );
    assert!(entry.heartbeat_at.is_some());
}

#[test]
fn edit_writes_are_throttled_to_one_per_interval_with_a_trailing_flush() {
    let dir = TempDir::new().unwrap();
    let r = roster_in(dir.path(), "me", Some(REPO));
    let t0 = Instant::now();
    assert_eq!(
        r.note_edits(vec!["/a".into()], t0),
        EditDecision::FlushNow(vec!["/a".into()])
    );
    assert_eq!(
        r.note_edits(vec!["/b".into()], t0 + Duration::from_secs(1)),
        EditDecision::ScheduleTrailing
    );
    assert_eq!(
        r.note_edits(vec!["/c".into()], t0 + Duration::from_secs(2)),
        EditDecision::Queued
    );
    assert_eq!(
        r.note_edits(vec!["/b".into()], t0 + Duration::from_secs(3)),
        EditDecision::Queued,
        "a repeated path is queued once"
    );
    assert_eq!(
        r.note_edits(vec!["/d".into()], t0 + Duration::from_secs(5)),
        EditDecision::FlushNow(vec!["/c".into(), "/b".into(), "/d".into()])
    );
}

#[test]
fn tool_calls_without_edits_still_heartbeat_but_never_queue_a_trailing_flush() {
    let dir = TempDir::new().unwrap();
    let r = roster_in(dir.path(), "me", Some(REPO));
    let t0 = Instant::now();
    assert_eq!(r.note_edits(vec![], t0), EditDecision::FlushNow(vec![]));
    assert_eq!(
        r.note_edits(vec![], t0 + Duration::from_secs(1)),
        EditDecision::Queued
    );
    assert!(!r.throttle.lock().trailing_scheduled);
    r.write_edits(&[], Utc::now()).unwrap();
    let entry = distill_active_sessions::list_in(dir.path())
        .unwrap()
        .remove(0);
    assert!(entry.heartbeat_at.is_some());
}

#[test]
fn write_recreates_an_entry_another_component_removed() {
    let dir = TempDir::new().unwrap();
    let r = roster_in(dir.path(), "me", Some(REPO));
    distill_active_sessions::unregister_in(dir.path(), &r.base.session_id).unwrap();
    r.write_edits(&["/repo/a.rs".into()], Utc::now()).unwrap();
    let entries = distill_active_sessions::list_in(dir.path()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].editing_files, vec!["/repo/a.rs".to_string()]);
    assert_eq!(entries[0].repo_root.as_deref(), Some(REPO));
}

#[test]
fn write_after_the_guard_closed_does_not_resurrect_the_entry() {
    let dir = TempDir::new().unwrap();
    let r = Arc::new(roster_in(dir.path(), "me", Some(REPO)));
    drop(RosterGuard(r.clone()));
    assert!(
        distill_active_sessions::list_in(dir.path())
            .unwrap()
            .is_empty()
    );
    r.write_edits(&["/repo/a.rs".into()], Utc::now()).unwrap();
    assert!(
        distill_active_sessions::list_in(dir.path())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn edited_paths_cover_edit_delete_move_but_not_reads() {
    let call = |kind, path: &str| {
        acp::ToolCall::new(acp::ToolCallId::new("t"), "t")
            .kind(kind)
            .locations(vec![acp::ToolCallLocation::new(path)])
    };
    let cwd = Path::new("/repo");
    assert_eq!(
        edited_paths(&call(acp::ToolKind::Edit, "src/a.rs"), cwd),
        vec!["/repo/src/a.rs"]
    );
    assert_eq!(
        edited_paths(&call(acp::ToolKind::Delete, "/abs/b.rs"), cwd),
        vec!["/abs/b.rs"]
    );
    assert_eq!(
        edited_paths(&call(acp::ToolKind::Move, "c.rs"), cwd),
        vec!["/repo/c.rs"]
    );
    assert!(edited_paths(&call(acp::ToolKind::Read, "d.rs"), cwd).is_empty());

    let patch = acp::ToolCall::new(acp::ToolCallId::new("p"), "Apply patch")
        .kind(acp::ToolKind::Edit)
        .raw_input(Some(serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: src/a.rs\n*** Move to: src/b.rs\n@@\n-x\n+y\n*** Add File: /abs/n.rs\n+z\n*** Delete File: old.rs\n*** End Patch"
        })));
    assert_eq!(
        edited_paths(&patch, cwd),
        vec![
            "/repo/src/a.rs",
            "/repo/src/b.rs",
            "/abs/n.rs",
            "/repo/old.rs"
        ]
    );
}

#[test]
fn repo_identity_is_shared_by_worktrees_of_one_repository() {
    let dir = TempDir::new().unwrap();
    let main = dir.path().join("main");
    let repo = git2::Repository::init(&main).unwrap();
    let sig = git2::Signature::now("t", "t@example.com").unwrap();
    let tree = repo
        .find_tree(repo.index().unwrap().write_tree().unwrap())
        .unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    let wt = dir.path().join("wt");
    repo.worktree(
        "wt",
        &wt,
        Some(git2::WorktreeAddOptions::new().reference(None)),
    )
    .unwrap();

    let (main_root, _) = repo_identity(&main);
    let (wt_root, wt_branch) = repo_identity(&wt);
    assert!(main_root.is_some());
    assert_eq!(main_root, wt_root);
    assert!(wt_branch.is_some());
    assert_eq!(repo_identity(dir.path()).0, None);
}
