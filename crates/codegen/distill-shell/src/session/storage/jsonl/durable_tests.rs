// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::sampling::ConversationItem;
use crate::session::info::Info;
use crate::session::persistence::default_model_id;
use crate::session::storage::{SessionUpdate, StorageAdapter};

fn info() -> Info {
    Info {
        id: acp::SessionId::new("durable-jsonl"),
        cwd: "/test".into(),
    }
}

fn update(info: &Info, text: String) -> SessionUpdate {
    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
        info.id.clone(),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
    )))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_and_durable_appends_keep_every_physical_line_parseable() {
    const N: usize = 100;
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    let adapter = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf());
    adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let ordinary = adapter.clone();
    let durable = adapter.clone();
    let info_a = info.clone();
    let info_b = info.clone();
    let ordinary = tokio::spawn(async move {
        for index in 0..N {
            ordinary
                .append_update(&info_a, &update(&info_a, format!("ordinary-{index}")))
                .await
                .unwrap();
        }
    });
    let durable = tokio::spawn(async move {
        for index in 0..N {
            durable
                .append_update_durable_commit_aware(
                    &info_b,
                    &update(&info_b, format!("durable-{index}")),
                )
                .await
                .unwrap();
        }
    });
    ordinary.await.unwrap();
    durable.await.unwrap();

    let bytes = std::fs::read(dir.path().join("updates.jsonl")).unwrap();
    let parsed = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice::<SessionUpdateEnvelope>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(parsed.len(), N * 2);
}

#[tokio::test]
async fn append_commit_is_reported_when_bookkeeping_fails() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    let adapter = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf());
    adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let summary = dir.path().join("summary.json");
    std::fs::remove_file(&summary).unwrap();
    std::fs::create_dir(&summary).unwrap();

    assert!(matches!(
        adapter
            .append_update_durable_commit_aware(&info, &update(&info, "committed".into()))
            .await,
        Err(crate::session::storage::AppendUpdateError::Committed(_))
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("updates.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn durable_append_reports_committed_when_file_barrier_fails() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    let adapter = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .with_file_sync_probe(|| Err(io::Error::other("file barrier failed")));
    adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    assert!(matches!(
        adapter
            .append_update_durable_commit_aware(&info, &update(&info, "terminal".into()))
            .await,
        Err(crate::session::storage::AppendUpdateError::Committed(_))
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("updates.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn chat_append_commit_is_reported_when_bookkeeping_fails() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    let adapter = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf());
    adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let summary = dir.path().join("summary.json");
    std::fs::remove_file(&summary).unwrap();
    std::fs::create_dir(&summary).unwrap();

    assert!(matches!(
        adapter
            .append_chat_message_commit_aware(&info, &ConversationItem::user("committed"))
            .await,
        Err(crate::session::storage::AppendChatError::Committed(_))
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("chat_history.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn directory_barrier_failure_is_retried_even_after_file_exists() {
    let mut attempts = 0;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("updates.jsonl");
    let mut flaky_parent = || {
        attempts += 1;
        if attempts == 1 {
            Err(io::Error::other("directory barrier failed"))
        } else {
            Ok(())
        }
    };
    assert!(
        JsonlStorageAdapter::append_jsonl_line_sync_with(
            &path,
            b"{\"record\":1}\n".to_vec(),
            AppendDurability::Durable,
            std::fs::File::sync_all,
            &mut flaky_parent,
        )
        .is_err()
    );
    JsonlStorageAdapter::append_jsonl_line_sync_with(
        &path,
        b"{\"record\":1}\n".to_vec(),
        AppendDurability::Durable,
        std::fs::File::sync_all,
        &mut flaky_parent,
    )
    .unwrap();
    assert_eq!(attempts, 2);
}

#[test]
fn file_barrier_error_after_write_is_committed_and_keeps_the_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("updates.jsonl");
    let error = JsonlStorageAdapter::append_jsonl_line_sync_with(
        &path,
        b"{\"record\":1}\n".to_vec(),
        AppendDurability::Durable,
        |_| Err(io::Error::other("file barrier failed")),
        || Ok(()),
    )
    .unwrap_err();
    assert!(
        matches!(error, AppendLineError::Committed(_)),
        "a file barrier failure after write_all must report the record as committed: {error}"
    );
    assert_eq!(error.to_string(), "file barrier failed");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"record\":1}\n",
        "the JSONL record must remain in the file so a later fsync can still make it durable"
    );
}

#[test]
fn cwd_switch_retry_after_post_append_barrier_failure_is_already_present() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chat_history.jsonl");
    let item = ConversationItem::working_directory_switch("moved", 3);
    let mut line = serde_json::to_vec(&item).unwrap();
    line.push(b'\n');

    assert!(matches!(
        JsonlStorageAdapter::append_cwd_switch_line_sync_with(
            &path,
            line.clone(),
            3,
            |_| Err(io::Error::other("file barrier failed")),
            || Ok(()),
        ),
        Err(crate::session::storage::AppendCwdSwitchError::Committed {
            acknowledgement: distill_chat_state::StrictAppendAck::Appended,
            ..
        })
    ));
    assert!(matches!(
        JsonlStorageAdapter::append_cwd_switch_line_sync_with(
            &path,
            line,
            3,
            |_| Ok(()),
            || Ok(()),
        )
        .unwrap(),
        distill_chat_state::StrictAppendAck::AlreadyPresent(item)
            if item.text_content() == "moved"
    ));
    assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn cwd_switch_retry_repairs_bookkeeping_without_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    let adapter = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf());
    adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let item = ConversationItem::working_directory_switch("moved", 4);

    let summary_path = dir.path().join("summary.json");
    let original_summary = std::fs::read(&summary_path).unwrap();
    std::fs::write(&summary_path, b"invalid summary").unwrap();
    assert!(matches!(
        adapter.append_cwd_switch_commit_aware(&info, &item).await,
        Err(crate::session::storage::AppendCwdSwitchError::Committed {
            acknowledgement: distill_chat_state::StrictAppendAck::Appended,
            ..
        })
    ));
    assert!(matches!(
        adapter
            .append_cwd_switch_commit_aware(
                &info,
                &ConversationItem::working_directory_switch("retry", 4),
            )
            .await,
        Err(crate::session::storage::AppendCwdSwitchError::Committed {
            acknowledgement: distill_chat_state::StrictAppendAck::AlreadyPresent(authoritative),
            ..
        }) if authoritative.text_content() == "moved"
    ));
    std::fs::write(&summary_path, original_summary).unwrap();
    assert_eq!(
        adapter.read_summary_sync(&info).unwrap().num_chat_messages,
        0
    );

    assert!(matches!(
        adapter
            .append_cwd_switch_commit_aware(&info, &item)
            .await
            .unwrap(),
        distill_chat_state::StrictAppendAck::AlreadyPresent(item)
            if item.text_content() == "moved"
    ));
    let summary = adapter.read_summary_sync(&info).unwrap();
    assert_eq!(summary.num_chat_messages, 1);
    assert_eq!(summary.cwd_switch_bookkeeping_generation, 4);

    assert!(matches!(
        adapter
            .append_cwd_switch_commit_aware(&info, &item)
            .await
            .unwrap(),
        distill_chat_state::StrictAppendAck::AlreadyPresent(item)
            if item.text_content() == "moved"
    ));
    let retried = adapter.read_summary_sync(&info).unwrap();
    assert_eq!(retried.num_chat_messages, 1);
    assert_eq!(retried.cwd_switch_bookkeeping_generation, 4);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("chat_history.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn cwd_switch_retained_by_history_replacement_is_not_recounted() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    let adapter = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf());
    adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let item = ConversationItem::working_directory_switch("retained", 6);

    adapter
        .replace_chat_history(&info, std::slice::from_ref(&item))
        .await
        .unwrap();
    let replaced = adapter.read_summary_sync(&info).unwrap();
    assert_eq!(replaced.num_chat_messages, 1);
    assert_eq!(replaced.cwd_switch_bookkeeping_generation, 6);

    assert!(matches!(
        adapter
            .append_cwd_switch_commit_aware(
                &info,
                &ConversationItem::working_directory_switch("retry", 6),
            )
            .await
            .unwrap(),
        distill_chat_state::StrictAppendAck::AlreadyPresent(authoritative)
            if authoritative.text_content() == "retained"
    ));
    let summary = adapter.read_summary_sync(&info).unwrap();
    assert_eq!(summary.num_chat_messages, 1);
    assert_eq!(summary.cwd_switch_bookkeeping_generation, 6);
    assert_eq!(
        adapter
            .read_chat_history_sync(adapter.chat_file(&info), CHAT_FORMAT_VERSION)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn cwd_switch_reappend_after_history_replacement_restores_message_count() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    let adapter = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf());
    adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let item = ConversationItem::working_directory_switch("moved", 7);

    assert!(matches!(
        adapter
            .append_cwd_switch_commit_aware(&info, &item)
            .await
            .unwrap(),
        distill_chat_state::StrictAppendAck::Appended
    ));
    adapter.replace_chat_history(&info, &[]).await.unwrap();
    let replaced = adapter.read_summary_sync(&info).unwrap();
    assert_eq!(replaced.cwd_switch_bookkeeping_generation, 7);
    assert_eq!(replaced.num_chat_messages, 0);

    assert!(matches!(
        adapter
            .append_cwd_switch_commit_aware(&info, &item)
            .await
            .unwrap(),
        distill_chat_state::StrictAppendAck::Appended
    ));
    let summary = adapter.read_summary_sync(&info).unwrap();
    assert_eq!(summary.cwd_switch_bookkeeping_generation, 7);
    assert_eq!(summary.num_chat_messages, 1);
    assert_eq!(
        adapter
            .read_chat_history_sync(adapter.chat_file(&info), CHAT_FORMAT_VERSION)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn init_session_retries_parent_sync_when_only_the_first_summary_exists() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    let parent_syncs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = parent_syncs.clone();
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .with_parent_sync_probe(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    assert_eq!(parent_syncs.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn init_session_does_not_ack_a_first_summary_whose_parent_sync_still_fails() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();

    let error = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .with_parent_sync_probe(|| Err(io::Error::other("directory barrier failed")))
        .init_session(&info, default_model_id())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "directory barrier failed");
}

#[tokio::test]
async fn init_session_skips_parent_sync_on_a_populated_resume() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    std::fs::write(dir.path().join("chat_history.jsonl"), b"").unwrap();

    let parent_syncs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = parent_syncs.clone();
    JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf())
        .with_parent_sync_probe(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    assert_eq!(parent_syncs.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn darwin_fullfsync_seam_reports_invalid_descriptor() {
    assert!(super::super::fullfsync_raw(-1).is_err());
}
