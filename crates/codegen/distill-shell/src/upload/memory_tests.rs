//! Production-entry tests for the per-turn memory archive: blocking-pool
//! build, per-cwd in-flight sharing, flush-deadline detach, and manifest
//! terminal records.
use super::*;
use crate::upload::trace::tests::read_tar_gz_entries;
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(archive_build_fault)]
async fn memory_archive_build_archives_seeded_root_off_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("ws");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = tmp.path().join("memroot");
    let storage = crate::session::memory::MemoryStorage::new(&cwd, Some(&root));
    let sessions = storage.workspace_dir().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("2026-01-01-abc.md"), "# seeded").unwrap();
    let mut build =
        join_or_start_memory_archive_build(cwd.to_string_lossy().into_owned(), Some(root));
    let archive = await_memory_archive(&mut build).await.unwrap();
    let entries = read_tar_gz_entries(&archive);
    assert!(
        entries
            .iter()
            .any(|(n, d)| n == "workspace/sessions/2026-01-01-abc.md" && d == b"# seeded"),
        "seeded session log missing from archive: {:?}",
        entries.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}
