// Modified for Distill by Samuel Fajreldines, 2026.
//! `SessionActor::persist_tool_definitions_artifact` against a real session dir: an unchanged toolset never rewrites,
//! and a failed write leaves the recorded hash alone so the next iteration retries.

use std::path::Path;

use super::support::create_test_actor;
use super::*;
use crate::session::tool_definitions_artifact::{
    TOOL_DEFINITIONS_FILENAME, load_tool_definitions_from_dir,
};

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: Some(format!("Use {name}.")),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }
}

fn artifact_names(session_dir: &Path) -> Vec<String> {
    load_tool_definitions_from_dir(session_dir)
        .expect("artifact loads")
        .into_iter()
        .map(|definition| definition.function.name)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn unchanged_toolset_skips_rewrite_and_failed_write_is_retried() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            actor.session_info.id = acp::SessionId::new(format!("tool-definitions-{unique}"));
            let session_dir = crate::session::persistence::session_dir(&actor.session_info);
            let path = session_dir.join(TOOL_DEFINITIONS_FILENAME);

            let first = [spec("read_file")];
            actor.persist_tool_definitions_artifact(&first).await;
            assert_eq!(vec!["read_file".to_owned()], artifact_names(&session_dir));

            // Backdated, so an unwanted rewrite shows even on a coarse-mtime filesystem
            let backdated = filetime::FileTime::from_unix_time(1_000_000, 0);
            filetime::set_file_mtime(&path, backdated).expect("backdate artifact");
            actor.persist_tool_definitions_artifact(&first).await;
            let metadata = std::fs::metadata(&path).expect("artifact metadata");
            assert_eq!(
                backdated,
                filetime::FileTime::from_last_modification_time(&metadata)
            );

            // A directory at the target fails the rename; the retry only writes if that failure recorded no hash
            std::fs::remove_file(&path).expect("remove artifact");
            std::fs::create_dir(&path).expect("occupy artifact path");
            let second = [spec("read_file"), spec("grep")];
            actor.persist_tool_definitions_artifact(&second).await;
            assert!(path.is_dir(), "a failed write must leave the target alone");
            std::fs::remove_dir(&path).expect("free artifact path");
            actor.persist_tool_definitions_artifact(&second).await;
            assert_eq!(
                vec!["read_file".to_owned(), "grep".to_owned()],
                artifact_names(&session_dir)
            );

            std::fs::remove_dir_all(&session_dir).expect("remove session dir");
        })
        .await;
}
