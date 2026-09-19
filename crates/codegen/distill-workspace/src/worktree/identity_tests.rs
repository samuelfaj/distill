// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::{Path, PathBuf};

use super::{WorktreeIdentity, worktree_identity_in};
use crate::LockedTestEnv;

struct WorktreesFixture {
    _env: LockedTestEnv,
    root: PathBuf,
    home: PathBuf,
    worktrees: PathBuf,
}

fn locked_worktrees_fixture(temp: &tempfile::TempDir) -> WorktreesFixture {
    let root = dunce::canonicalize(temp.path()).unwrap();
    let home = root.join("grok-home");
    let worktrees = home.join("worktrees");
    std::fs::create_dir_all(&worktrees).unwrap();
    let env = LockedTestEnv::lock().set("GROK_HOME", &home);
    WorktreesFixture {
        _env: env,
        root,
        home,
        worktrees,
    }
}

fn committed_repo(root: &Path) -> PathBuf {
    use distill_test_utils::git::{git_commit_all, init_git_repo};
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    std::fs::write(repo.join("tracked.txt"), "original").unwrap();
    git_commit_all(&repo, "initial");
    repo
}

fn git_worktree_add_detached(repo: &Path, worktree: &Path) {
    std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(["worktree", "add", "--detach"])
        .arg(worktree)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn nested_subdir_cwd_derives_label_from_second_component_after_prefix() {
    let temp = tempfile::TempDir::new().unwrap();
    let fixture = locked_worktrees_fixture(&temp);

    let nested = fixture
        .worktrees
        .join("xai")
        .join("fix-bug")
        .join("src")
        .join("deep");
    assert_eq!(
        worktree_identity_in(&fixture.worktrees, &nested.to_string_lossy()),
        Some(WorktreeIdentity {
            label: "fix-bug".to_owned(),
            source_workspace_dir: None,
        })
    );
}

#[test]
fn cwd_at_slug_level_or_at_worktrees_dir_itself_has_no_identity() {
    let worktrees = Path::new("/home/user/.grok/worktrees");
    assert_eq!(
        worktree_identity_in(worktrees, "/home/user/.grok/worktrees"),
        None
    );
    assert_eq!(
        worktree_identity_in(worktrees, "/home/user/.grok/worktrees/xai"),
        None
    );
}

#[test]
fn cwd_outside_worktrees_dir_has_no_identity() {
    let worktrees = Path::new("/home/user/.grok/worktrees");
    assert_eq!(
        worktree_identity_in(worktrees, "/home/user/projects/xai"),
        None
    );
}

#[test]
fn db_recorded_source_wins_over_git_discovery() {
    distill_test_utils::require_git!();
    let temp = tempfile::TempDir::new().unwrap();
    let fixture = locked_worktrees_fixture(&temp);
    let repo = committed_repo(&fixture.root);
    let wt = fixture.worktrees.join("repo").join("db-wins");
    git_worktree_add_detached(&repo, &wt);

    let db = distill_fast_worktree::WorktreeDb::open(&fixture.home).unwrap();
    db.register(&distill_fast_worktree::WorktreeRecord {
        id: "db-wins".to_owned(),
        path: wt.clone(),
        source_repo: "/db-source".into(),
        repo_name: "repo".to_owned(),
        kind: distill_fast_worktree::WorktreeKind::Session,
        creation_mode: "linked".to_owned(),
        git_ref: None,
        head_commit: None,
        session_id: None,
        creator_pid: None,
        created_at: 100,
        last_accessed_at: None,
        status: distill_fast_worktree::WorktreeStatus::Alive,
        metadata: Some(crate::worktree::build_label_metadata("db-wins", false)),
    })
    .unwrap();

    let identity = worktree_identity_in(&fixture.worktrees, &wt.to_string_lossy()).unwrap();
    assert_eq!(
        identity,
        WorktreeIdentity {
            label: "db-wins".to_owned(),
            source_workspace_dir: Some("/db-source".to_owned()),
        }
    );
}

#[test]
fn linked_worktree_without_db_record_derives_source_from_git() {
    distill_test_utils::require_git!();
    let temp = tempfile::TempDir::new().unwrap();
    let fixture = locked_worktrees_fixture(&temp);
    let repo = committed_repo(&fixture.root);
    let wt = fixture.worktrees.join("repo").join("no-db-row");
    git_worktree_add_detached(&repo, &wt);
    let nested = wt.join("src");
    std::fs::create_dir_all(&nested).unwrap();

    let identity = worktree_identity_in(&fixture.worktrees, &nested.to_string_lossy()).unwrap();
    assert_eq!(identity.label, "no-db-row");
    let source = identity.source_workspace_dir.expect("git-derived source");
    assert_eq!(
        dunce::canonicalize(source).unwrap(),
        dunce::canonicalize(&repo).unwrap()
    );
}

#[test]
fn plain_directory_under_worktrees_dir_does_not_inherit_enclosing_repo() {
    distill_test_utils::require_git!();
    let temp = tempfile::TempDir::new().unwrap();
    let fixture = locked_worktrees_fixture(&temp);
    distill_test_utils::git::init_git_repo(&fixture.root);
    std::fs::write(fixture.root.join("tracked.txt"), "x").unwrap();
    distill_test_utils::git::git_commit_all(&fixture.root, "initial");
    let not_a_repo = fixture.worktrees.join("repo").join("deleted-worktree");
    std::fs::create_dir_all(&not_a_repo).unwrap();

    assert_eq!(
        worktree_identity_in(&fixture.worktrees, &not_a_repo.to_string_lossy()),
        Some(WorktreeIdentity {
            label: "deleted-worktree".to_owned(),
            source_workspace_dir: None,
        })
    );
}

#[cfg(unix)]
#[test]
fn resolved_cwd_under_symlinked_worktrees_dir_still_derives_identity() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(temp.path()).unwrap();
    let real_home = root.join("real-home");
    let real_cwd = real_home
        .join("worktrees")
        .join("repo")
        .join("fix-bug")
        .join("src");
    std::fs::create_dir_all(&real_cwd).unwrap();
    let link_home = root.join("link-home");
    std::os::unix::fs::symlink(&real_home, &link_home).unwrap();
    let link_worktrees = link_home.join("worktrees");

    assert_eq!(
        worktree_identity_in(&link_worktrees, &real_cwd.to_string_lossy()),
        Some(WorktreeIdentity {
            label: "fix-bug".to_owned(),
            source_workspace_dir: None,
        })
    );
}

#[cfg(unix)]
#[test]
fn standalone_clone_behind_symlinked_worktrees_dir_reports_no_source() {
    distill_test_utils::require_git!();
    let temp = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(temp.path()).unwrap();
    let real_home = root.join("real-home");
    let standalone = real_home.join("worktrees").join("repo").join("standalone");
    std::fs::create_dir_all(&standalone).unwrap();
    distill_test_utils::git::init_git_repo(&standalone);
    std::fs::write(standalone.join("tracked.txt"), "x").unwrap();
    distill_test_utils::git::git_commit_all(&standalone, "initial");
    let link_home = root.join("link-home");
    std::os::unix::fs::symlink(&real_home, &link_home).unwrap();
    let _env = LockedTestEnv::lock().set("GROK_HOME", &link_home);

    let link_worktrees = link_home.join("worktrees");
    let link_cwd = link_worktrees.join("repo").join("standalone");
    assert_eq!(
        worktree_identity_in(&link_worktrees, &link_cwd.to_string_lossy()),
        Some(WorktreeIdentity {
            label: "standalone".to_owned(),
            source_workspace_dir: None,
        })
    );
}

#[test]
fn standalone_clone_with_source_marker_and_no_db_derives_marker_source() {
    distill_test_utils::require_git!();
    let temp = tempfile::TempDir::new().unwrap();
    let fixture = locked_worktrees_fixture(&temp);
    let standalone = fixture.worktrees.join("repo").join("standalone");
    std::fs::create_dir_all(&standalone).unwrap();
    distill_test_utils::git::init_git_repo(&standalone);
    std::fs::write(standalone.join("tracked.txt"), "x").unwrap();
    distill_test_utils::git::git_commit_all(&standalone, "initial");
    std::fs::write(
        standalone.join(".git").join("grok-worktree-source"),
        "/marker-source\n",
    )
    .unwrap();

    assert_eq!(
        worktree_identity_in(&fixture.worktrees, &standalone.to_string_lossy()),
        Some(WorktreeIdentity {
            label: "standalone".to_owned(),
            source_workspace_dir: Some("/marker-source".to_owned()),
        })
    );
}

#[test]
fn standalone_git_dir_without_db_record_keeps_label_but_no_source() {
    distill_test_utils::require_git!();
    let temp = tempfile::TempDir::new().unwrap();
    let fixture = locked_worktrees_fixture(&temp);
    let standalone = fixture.worktrees.join("repo").join("standalone");
    std::fs::create_dir_all(&standalone).unwrap();
    distill_test_utils::git::init_git_repo(&standalone);
    std::fs::write(standalone.join("tracked.txt"), "x").unwrap();
    distill_test_utils::git::git_commit_all(&standalone, "initial");

    assert_eq!(
        worktree_identity_in(&fixture.worktrees, &standalone.to_string_lossy()),
        Some(WorktreeIdentity {
            label: "standalone".to_owned(),
            source_workspace_dir: None,
        })
    );
}
