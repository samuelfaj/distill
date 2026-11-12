// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

const BUDGET: Duration = Duration::from_secs(30);

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new(crate::util::subprocess::git_bin())
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository with one commit, then a pre-turn state the user left behind:
/// an unstaged edit, a staged edit and an untracked file.
fn dirty_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git_ok(root, &["init", "-q"]);
    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
    git_ok(root, &["add", "."]);
    git_ok(root, &["commit", "-qm", "base"]);
    std::fs::write(root.join("a.rs"), "fn a() { /* user's unstaged edit */ }\n").unwrap();
    std::fs::write(root.join("b.rs"), "fn b() { /* user's staged edit */ }\n").unwrap();
    git_ok(root, &["add", "b.rs"]);
    std::fs::write(root.join("notes.txt"), "user's untracked notes\n").unwrap();
    dir
}

/// Everything the user owns in the repository: the index bytes, HEAD, every
/// ref and the status listing.
fn user_state(root: &Path) -> (Vec<u8>, String, String, String) {
    (
        std::fs::read(root.join(".git/index")).unwrap(),
        git_ok(root, &["rev-parse", "HEAD"]),
        git_ok(root, &["for-each-ref"]),
        git_ok(root, &["status", "--porcelain=v1", "--untracked-files=all"]),
    )
}

/// The review must judge this turn's work only: changes the user made before
/// the turn (unstaged, staged or untracked) stay out of the patch, while a
/// tracked edit, a new file and a deletion made during the turn are in it.
#[tokio::test]
async fn a_dirty_tree_yields_only_the_turns_changes() {
    let dir = dirty_repo();
    let root = dir.path();
    let snapshot = capture_turn_snapshot(root, BUDGET).await;
    assert!(matches!(snapshot, TurnSnapshot::Tree { .. }), "{snapshot:?}");

    // The turn: a shell-style edit, a new file and a deletion.
    std::fs::write(root.join("a.rs"), "fn a() { /* user's unstaged edit */ turn(); }\n").unwrap();
    std::fs::write(root.join("new.rs"), "fn new() {}\n").unwrap();
    std::fs::remove_file(root.join("b.rs")).unwrap();

    let patch = capture_turn_patch(root, &snapshot).await.unwrap();
    assert!(!patch.may_include_earlier);
    assert_eq!(patch.changes.changed_files, vec!["a.rs", "b.rs", "new.rs"]);
    let diff = &patch.changes.diff;
    assert!(diff.contains("+fn a() { /* user's unstaged edit */ turn(); }"));
    assert!(
        diff.contains("-fn a() { /* user's unstaged edit */ }"),
        "the base is the pre-turn tree, not HEAD: {diff}"
    );
    assert!(!diff.contains("-fn a() {}"), "HEAD content must not appear: {diff}");
    assert!(diff.contains("-fn b() { /* user's staged edit */ }"));
    assert!(diff.contains("+fn new() {}"));
    assert!(!diff.contains("notes.txt"), "pre-turn untracked files are not the turn's");
}

/// Snapshot and patch capture never touch what the user owns: the index file
/// stays byte-identical, and HEAD, refs and status do not move.
#[tokio::test]
async fn capture_leaves_the_users_index_and_refs_unchanged() {
    let dir = dirty_repo();
    let root = dir.path();
    let before = user_state(root);
    let snapshot = capture_turn_snapshot(root, BUDGET).await;
    assert_eq!(user_state(root), before);
    std::fs::write(root.join("turn.rs"), "fn turn() {}\n").unwrap();
    let after_edit = user_state(root);
    capture_turn_patch(root, &snapshot).await.unwrap();
    assert_eq!(user_state(root), after_edit);
    assert!(git_ok(root, &["stash", "list"]).is_empty());
}

/// An untracked file that existed before the turn and was not touched is not
/// part of the patch; one created during the turn is, with its content.
#[tokio::test]
async fn only_untracked_files_new_since_the_snapshot_count() {
    let dir = dirty_repo();
    let root = dir.path();
    let snapshot = capture_turn_snapshot(root, BUDGET).await;
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/fresh.rs"), "pub fn fresh() {}\n").unwrap();
    let patch = capture_turn_patch(root, &snapshot).await.unwrap();
    assert_eq!(patch.changes.changed_files, vec!["src/fresh.rs"]);
    assert!(patch.changes.diff.contains("+pub fn fresh() {}"));
}

/// The session may run in a subdirectory; the snapshot covers the whole
/// repository so a change outside that directory is not missed.
#[tokio::test]
async fn a_subdirectory_session_sees_the_whole_repository() {
    let dir = dirty_repo();
    let root = dir.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    let snapshot = capture_turn_snapshot(&root.join("sub"), BUDGET).await;
    std::fs::write(root.join("top.rs"), "fn top() {}\n").unwrap();
    let patch = capture_turn_patch(&root.join("sub"), &snapshot).await.unwrap();
    assert_eq!(patch.changes.changed_files, vec!["top.rs"]);
}

/// Without a snapshot tree the patch is taken against HEAD and says so, so the
/// reviewer does not blame the turn for earlier hunks.
#[tokio::test]
async fn a_missing_snapshot_falls_back_to_head_and_says_so() {
    let dir = dirty_repo();
    let root = dir.path();
    let untracked = match capture_turn_snapshot(root, BUDGET).await {
        TurnSnapshot::Tree { untracked, .. } => untracked,
        other => panic!("{other:?}"),
    };
    std::fs::write(root.join("turn.rs"), "fn turn() {}\n").unwrap();
    let fallback = TurnSnapshot::Unavailable {
        untracked: Some(untracked),
    };
    let patch = capture_turn_patch(root, &fallback).await.unwrap();
    assert!(patch.may_include_earlier);
    assert!(patch.changes.diff.starts_with(EARLIER_CHANGES_NOTE));
    assert_eq!(
        patch.changes.changed_files,
        vec!["a.rs", "b.rs", "turn.rs"],
        "earlier tracked edits are included, earlier untracked files are not"
    );
}

/// A repository without any commit still yields a patch.
#[tokio::test]
async fn a_repository_without_commits_has_a_patch() {
    let dir = tempfile::tempdir().unwrap();
    git_ok(dir.path(), &["init", "-q"]);
    let snapshot = capture_turn_snapshot(dir.path(), BUDGET).await;
    std::fs::write(dir.path().join("first.rs"), "fn first() {}\n").unwrap();
    let patch = capture_turn_patch(dir.path(), &snapshot).await.unwrap();
    assert_eq!(patch.changes.changed_files, vec!["first.rs"]);
}

/// Outside a repository there is nothing to diff: the snapshot says so and
/// the patch capture refuses rather than inventing one.
#[tokio::test]
async fn outside_a_repository_there_is_no_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot = capture_turn_snapshot(dir.path(), BUDGET).await;
    assert_eq!(snapshot, TurnSnapshot::NoRepository);
    assert!(capture_turn_patch(dir.path(), &snapshot).await.is_err());
}

/// An untouched tree gives an empty patch with no changed files.
#[tokio::test]
async fn an_untouched_tree_has_an_empty_patch() {
    let dir = dirty_repo();
    let snapshot = capture_turn_snapshot(dir.path(), BUDGET).await;
    let patch = capture_turn_patch(dir.path(), &snapshot).await.unwrap();
    assert!(patch.changes.changed_files.is_empty());
    assert!(patch.changes.diff.is_empty());
}
