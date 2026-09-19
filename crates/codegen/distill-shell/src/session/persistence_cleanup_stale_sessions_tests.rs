use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use filetime::FileTime;
use tempfile::TempDir;

use super::{CleanupLevel, CleanupStats, cleanup_stale_sessions_inner, mark_session_live};

const TTL_DAYS: u32 = 30;

fn days_ago(days: u64) -> FileTime {
    FileTime::from_system_time(SystemTime::now() - Duration::from_secs(days * 86_400))
}

/// Writes a small file (creating parents) and backdates it to `age`.
fn write(path: &Path, age: FileTime) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"x").unwrap();
    filetime::set_file_mtime(path, age).unwrap();
}

/// `sessions/<cwd>/<id>/` with `summary.json` and `updates.jsonl` at `age`.
fn write_session(root: &Path, cwd: &str, id: &str, age: FileTime) -> PathBuf {
    let dir = root.join(cwd).join(id);
    write(&dir.join("summary.json"), age);
    write(&dir.join("updates.jsonl"), age);
    dir
}

fn sweep(root: &Path, live: &Path) -> CleanupStats {
    cleanup_stale_sessions_inner(root, TTL_DAYS, live, CleanupLevel::SessionsRoot)
}

fn no_live() -> PathBuf {
    PathBuf::from("/nonexistent/live-session")
}

#[test]
fn active_session_keeps_backdated_write_once_artifacts() {
    let tmp = TempDir::new().unwrap();
    let session = write_session(tmp.path(), "cwd", "s1", days_ago(0));
    let artifacts = [
        "compaction_checkpoints/a.json",
        "compaction_requests/r.json",
        "prompts/prompt_1.txt",
        "tool_definitions.json",
        "summary.json.lock",
        "subagents/child/summary.json",
    ];
    for rel in artifacts {
        write(&session.join(rel), days_ago(60));
    }

    let stats = sweep(tmp.path(), &no_live());

    assert_eq!(CleanupStats::default(), stats);
    for rel in artifacts {
        assert!(session.join(rel).is_file(), "{rel} must survive");
    }
}

#[test]
fn active_session_sweeps_only_backdated_blob_files() {
    let tmp = TempDir::new().unwrap();
    let session = write_session(tmp.path(), "cwd", "s1", days_ago(0));
    let old_blobs = [
        "images/1.jpg",
        "videos/1.mp4",
        "downloads/1.pdf",
        "terminal/t.log",
    ];
    for rel in old_blobs {
        write(&session.join(rel), days_ago(60));
    }
    write(&session.join("images/2.jpg"), days_ago(0));

    let stats = sweep(tmp.path(), &no_live());

    assert_eq!(
        CleanupStats {
            files_deleted: 4,
            ..CleanupStats::default()
        },
        stats
    );
    for rel in old_blobs {
        assert!(!session.join(rel).exists(), "{rel} must be swept");
    }
    assert!(session.join("images/2.jpg").is_file());
    assert!(session.join("images").is_dir());
}

#[test]
fn idle_session_is_removed_whole() {
    let tmp = TempDir::new().unwrap();
    let idle = write_session(tmp.path(), "cwd", "idle", days_ago(40));
    write(&idle.join("compaction_checkpoints/a.json"), days_ago(40));
    let fresh = write_session(tmp.path(), "cwd", "fresh", days_ago(0));
    let summary_rewritten = write_session(tmp.path(), "cwd", "summary-rewritten", days_ago(40));
    write(&summary_rewritten.join("summary.json"), days_ago(1));

    let stats = sweep(tmp.path(), &no_live());

    assert_eq!(
        CleanupStats {
            sessions_removed: 1,
            ..CleanupStats::default()
        },
        stats
    );
    assert!(!idle.exists());
    assert!(fresh.join("updates.jsonl").is_file());
    assert!(summary_rewritten.join("updates.jsonl").is_file());
}

#[test]
fn live_session_dir_is_never_removed_but_its_stale_blobs_are_pruned() {
    let tmp = TempDir::new().unwrap();
    let live = write_session(tmp.path(), "cwd", "live", days_ago(40));
    write(&live.join("compaction_checkpoints/a.json"), days_ago(40));
    write(&live.join("images/1.jpg"), days_ago(40));
    write(&live.join("images/2.jpg"), days_ago(0));

    let stats = sweep(tmp.path(), &live);

    assert_eq!(
        CleanupStats {
            files_deleted: 1,
            ..CleanupStats::default()
        },
        stats
    );
    assert!(live.join("summary.json").is_file());
    assert!(live.join("updates.jsonl").is_file());
    assert!(live.join("compaction_checkpoints/a.json").is_file());
    assert!(!live.join("images/1.jpg").exists());
    assert!(live.join("images/2.jpg").is_file());
    assert!(live.join("images").is_dir());
}

/// Another process's sweep sees only mtimes, so an attach must bump one before it loads.
#[test]
fn mark_session_live_keeps_an_idle_session_out_of_a_foreign_sweep() {
    let tmp = TempDir::new().unwrap();
    let attaching = write_session(tmp.path(), "cwd", "attaching", days_ago(40));
    write(
        &attaching.join("compaction_checkpoints/a.json"),
        days_ago(40),
    );
    let summary_before = std::fs::read(attaching.join("summary.json")).unwrap();
    let no_summary = tmp.path().join("cwd").join("no-summary");
    write(&no_summary.join("summary.json.lock"), days_ago(40));

    mark_session_live(&attaching);
    mark_session_live(&no_summary);
    mark_session_live(&tmp.path().join("cwd").join("not-created-yet"));
    let stats = sweep(tmp.path(), &no_live());

    assert_eq!(
        CleanupStats {
            sessions_removed: 1,
            ..CleanupStats::default()
        },
        stats
    );
    assert!(attaching.join("updates.jsonl").is_file());
    assert!(attaching.join("compaction_checkpoints/a.json").is_file());
    assert_eq!(
        summary_before,
        std::fs::read(attaching.join("summary.json")).unwrap()
    );
    assert!(!no_summary.exists());
}

#[test]
fn stub_dir_follows_newest_file_or_dir_mtime() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("cwd");
    let lock_only = cwd.join("lock-only");
    write(&lock_only.join("summary.json.lock"), days_ago(40));
    let old_empty = cwd.join("old-empty");
    std::fs::create_dir_all(&old_empty).unwrap();
    filetime::set_file_mtime(&old_empty, days_ago(40)).unwrap();
    let fresh_empty = cwd.join("fresh-empty");
    std::fs::create_dir_all(&fresh_empty).unwrap();

    let stats = sweep(tmp.path(), &no_live());

    assert_eq!(
        CleanupStats {
            sessions_removed: 2,
            ..CleanupStats::default()
        },
        stats
    );
    assert!(!lock_only.exists());
    assert!(!old_empty.exists());
    assert!(fresh_empty.is_dir());
}

#[test]
fn dot_entries_are_kept_while_stray_files_follow_the_mtime_rule() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("cwd");
    write(&cwd.join(".cwd"), days_ago(60));
    write(&tmp.path().join(".index/old.bin"), days_ago(60));
    write(&cwd.join(".hidden/old.bin"), days_ago(60));
    write_session(tmp.path(), "cwd", "fresh", days_ago(0));
    write(&cwd.join("prompt_history.jsonl"), days_ago(60));

    let stats = sweep(tmp.path(), &no_live());

    assert_eq!(
        CleanupStats {
            files_deleted: 1,
            ..CleanupStats::default()
        },
        stats
    );
    assert!(cwd.join(".cwd").is_file());
    assert!(tmp.path().join(".index/old.bin").is_file());
    assert!(cwd.join(".hidden/old.bin").is_file());
    assert!(!cwd.join("prompt_history.jsonl").exists());
}

#[test]
fn emptied_cwd_dir_is_removed_only_after_a_session_removal() {
    let tmp = TempDir::new().unwrap();
    write_session(tmp.path(), "emptied", "idle", days_ago(40));
    let untouched = tmp.path().join("untouched");
    std::fs::create_dir_all(&untouched).unwrap();
    write_session(tmp.path(), "kept", "fresh", days_ago(0));
    let hashed = tmp.path().join("hashed");
    write(&hashed.join(".cwd"), days_ago(60));
    write_session(tmp.path(), "hashed", "idle", days_ago(40));

    let stats = sweep(tmp.path(), &no_live());

    assert_eq!(
        CleanupStats {
            dirs_removed: 1,
            sessions_removed: 2,
            ..CleanupStats::default()
        },
        stats
    );
    assert!(!tmp.path().join("emptied").exists());
    assert!(untouched.is_dir());
    assert!(tmp.path().join("kept/fresh/summary.json").is_file());
    assert!(!hashed.join("idle").exists());
    assert!(hashed.join(".cwd").is_file());
}

#[cfg(unix)]
#[test]
fn symlinked_session_dir_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = write_session(outside.path(), "cwd", "target", days_ago(40));
    let cwd = tmp.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::os::unix::fs::symlink(&target, cwd.join("link")).unwrap();

    let stats = sweep(tmp.path(), &no_live());

    assert_eq!(CleanupStats::default(), stats);
    assert!(target.join("summary.json").is_file());
    assert!(cwd.join("link").symlink_metadata().is_ok());
}
