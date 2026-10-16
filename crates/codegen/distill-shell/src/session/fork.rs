// Modified for Distill by Samuel Fajreldines, 2026.
//! Forks a saved session to a new working directory with a new session ID.
//! This creates new session files but does not start the session.

use crate::remote::BackendClient;
const FORK_LOG: &str = "xai_fork";
use crate::session::export::ExportedMetadata;
use crate::session::info::Info;
use crate::session::storage::{CopySessionOptions, JsonlStorageAdapter, StorageAdapter};
use crate::session::visibility::SIDE_SESSION_KIND;
use crate::util::distill_home::distill_home;
use agent_client_protocol as acp;
use std::io;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSessionRequest {
    pub source_session_id: String,
    pub source_cwd: String,
    pub new_cwd: String,
    /// If None, a new ID will be auto-generated.
    #[serde(default)]
    pub new_session_id: Option<String>,
    /// If None, the source session's model will be used.
    #[serde(default)]
    pub new_model_id: Option<String>,
    #[serde(default)]
    pub target_prompt_index: Option<usize>,
    /// Override `session_kind` in the forked summary. Defaults to `"fork"`.
    /// Worktree forks set this to `"worktree"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<String>,
    /// The original workspace directory this worktree session was spawned from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_dir: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSessionResponse {
    pub new_session_id: String,
    pub chat_messages_copied: usize,
    pub updates_copied: usize,
    pub plan_state_copied: bool,
    pub new_cwd: String,
    pub parent_session_id: String,
    /// The model ID of the forked session (may differ from source if overridden)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_model_id: Option<String>,
}

/// Uses a plain UUIDv7 with no prefix or source embedding, so IDs stay a constant 36 chars no matter how many fork rounds occur.
fn generate_fork_session_id(_source_id: &str) -> String {
    uuid::Uuid::now_v7().to_string()
}

pub async fn fork_session(
    request: ForkSessionRequest,
    agent_id: &str,
    auth_manager: Option<std::sync::Arc<distill_login::AuthManager>>,
) -> io::Result<ForkSessionResponse> {
    let storage = JsonlStorageAdapter::with_root(distill_home());
    fork_session_with_storage(request, storage, agent_id, auth_manager).await
}

/// [`fork_session`] against an explicit storage adapter (tests inject a temp root).
pub(crate) async fn fork_session_with_storage(
    request: ForkSessionRequest,
    storage: JsonlStorageAdapter,
    agent_id: &str,
    auth_manager: Option<std::sync::Arc<distill_login::AuthManager>>,
) -> io::Result<ForkSessionResponse> {
    let t0 = std::time::Instant::now();

    // Build source and target Info
    let source_info = Info {
        id: acp::SessionId::new(request.source_session_id.clone()),
        cwd: request.source_cwd.clone(),
    };

    let new_session_id = request
        .new_session_id
        .clone()
        .unwrap_or_else(|| generate_fork_session_id(&request.source_session_id));

    let target_info = Info {
        id: acp::SessionId::new(new_session_id.clone()),
        cwd: request.new_cwd.clone(),
    };

    // Idempotent re-fork: a prior attempt already created this child from the same parent.
    // Return the existing session instead of erroring or copying a duplicate.
    match storage.load_summary(&target_info).await {
        Ok(existing)
            if existing.parent_session_id.as_deref()
                == Some(request.source_session_id.as_str()) =>
        {
            return Ok(ForkSessionResponse {
                new_session_id,
                chat_messages_copied: existing.num_chat_messages,
                updates_copied: existing.num_messages,
                // The existing plan file is not re-read; the copy result is not recomputed.
                plan_state_copied: false,
                new_cwd: request.new_cwd,
                parent_session_id: request.source_session_id,
                new_model_id: request.new_model_id,
            });
        }
        Ok(existing) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "session {new_session_id} already exists (parent {:?})",
                    existing.parent_session_id
                ),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    // Copy session data with parent tracking.
    // Runs on the blocking thread pool so concurrent fork copies can execute truly in parallel
    // On a LocalSet, async copy_session_data serializes because the sync disk I/O blocks the single-threaded runtime
    let options = CopySessionOptions {
        mint_session_identity: true,
        parent_session_id: Some(request.source_session_id.clone()),
        new_model_id: request.new_model_id.clone(),
        target_prompt_index: request.target_prompt_index,
        session_kind: request.session_kind.clone(),
        source_workspace_dir: request.source_workspace_dir.clone(),
        prompt_display_cwd: request
            .source_workspace_dir
            .clone()
            .filter(|display| display != &request.new_cwd),
        skip_cwd_transform: request.session_kind.as_deref() == Some("worktree"),
        // Carry the parent's compaction segment archive into the fork so the child retains pre-compaction history
        // The live summary is already copied via chat_history.jsonl
        copy_compaction_segments: true,
        ..Default::default()
    };

    let result = tokio::task::spawn_blocking(move || {
        storage.copy_session_data_sync(&source_info, &target_info, options)
    })
    .await
    .map_err(|e| io::Error::other(format!("spawn_blocking panicked: {e}")))??;

    let copy_ms = t0.elapsed().as_millis() as u64;

    // Register the fork with the backend from a spawned task
    // The local fork works without it: all fork state is in session files on disk, and the backend learns of the session when the task completes
    // Spawning keeps the network round-trip (~200-400ms) off the critical path
    // Side chats are ephemeral and local-only; never register them with the backend.
    let skip_backend = request.session_kind.as_deref() == Some(SIDE_SESSION_KIND);
    if let Some(am) = auth_manager.filter(|_| !skip_backend) {
        let sid = new_session_id.clone();
        let cwd = request.new_cwd.clone();
        let parent = request.source_session_id.clone();
        let model = request.new_model_id.clone();
        let aid = agent_id.to_string();
        let session_agent_id = result.agent_id.clone();
        tokio::spawn(async move {
            if let Err(e) = sync_forked_session_to_backend(
                &sid,
                &cwd,
                parent,
                model,
                &aid,
                session_agent_id.as_deref(),
                am,
            )
            .await
            {
                tracing::warn!(
                    session_id = %sid,
                    error = %e,
                    "Failed to register forked session with backend (background)"
                );
            }
        });
    }

    let total_ms = t0.elapsed().as_millis() as u64;
    tracing::info!(
        target: FORK_LOG,
        session_id = %new_session_id,
        source_session = %request.source_session_id,
        copy_ms,
        total_ms,
        chat_copied = result.chat_messages_copied,
        updates_copied = result.updates_copied,
        "FORK_COPY: session data copied (backend sync spawned in background)"
    );

    Ok(ForkSessionResponse {
        new_session_id,
        chat_messages_copied: result.chat_messages_copied,
        updates_copied: result.updates_copied,
        plan_state_copied: result.plan_state_copied,
        new_cwd: request.new_cwd,
        parent_session_id: request.source_session_id,
        new_model_id: request.new_model_id,
    })
}

/// Sync a forked session to the backend (for writeback mode).
async fn sync_forked_session_to_backend(
    session_id: &str,
    cwd: &str,
    parent_session_id: String,
    model_id: Option<String>,
    agent_id: &str,
    session_agent_id: Option<&str>,
    auth_manager: std::sync::Arc<distill_login::AuthManager>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = BackendClient::new().with_auth_manager(auth_manager);
    let metadata = ExportedMetadata {
        title: None, // The title is generated later when the session runs
        cwd: cwd.to_string(),
        model_id,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        updated_at: Some(chrono::Utc::now().to_rfc3339()),
        total_messages: Some(0),
        parent_session_id: Some(parent_session_id),
        agent_id: session_agent_id.map(str::to_owned),
        session_kind: None,
        subagent_type: None,
        subagent_persona: None,
        subagent_role: None,
        fork_context_source: None,
        subagent_depth: None,
        title_is_manual: None,
    };

    client
        .upsert_session(session_id, &metadata, agent_id)
        .await?;
    tracing::info!(
        session_id = %session_id,
        "Forked session registered with backend"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xai_update(
        session_id: &str,
        update: serde_json::Value,
    ) -> crate::session::storage::SessionUpdate {
        crate::session::storage::SessionUpdateEnvelope::from_value(serde_json::json!({
            "method": "_x.ai/session/update",
            "params": {
                "sessionId": session_id,
                "update": update,
            }
        }))
        .unwrap()
    }

    #[test]
    fn test_generate_fork_session_id_format() {
        let fork_id = generate_fork_session_id("abc123");

        assert_eq!(
            fork_id.len(),
            36,
            "Fork ID should be a standard UUID length"
        );
        assert!(
            uuid::Uuid::parse_str(&fork_id).is_ok(),
            "Fork ID should be a valid UUID"
        );
    }

    #[test]
    fn test_generate_fork_session_id_uniqueness() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..100 {
            let fork_id = generate_fork_session_id("any-source");
            assert_eq!(fork_id.len(), 36);
            ids.insert(fork_id);
        }

        assert_eq!(ids.len(), 100, "All generated IDs should be unique");
    }

    #[test]
    fn test_generate_fork_session_id_constant_length() {
        let id1 = generate_fork_session_id("019c43b5-c4ae-7190-b058-693e24669ba9");
        let id2 = generate_fork_session_id(&id1);
        let id3 = generate_fork_session_id(&id2);

        assert_eq!(id1.len(), 36);
        assert_eq!(id2.len(), 36);
        assert_eq!(id3.len(), 36);
    }

    #[test]
    fn test_fork_session_request_serialization() {
        let request = ForkSessionRequest {
            source_session_id: "abc123".to_string(),
            source_cwd: "/old/project".to_string(),
            new_cwd: "/new/project".to_string(),
            new_session_id: Some("custom-session-id".to_string()),
            new_model_id: Some("grok-3".to_string()),
            target_prompt_index: None,
            ..Default::default()
        };

        let json = serde_json::to_string(&request).unwrap();
        let deserialized: ForkSessionRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.source_session_id, "abc123");
        assert_eq!(deserialized.source_cwd, "/old/project");
        assert_eq!(deserialized.new_cwd, "/new/project");
        assert_eq!(
            deserialized.new_session_id,
            Some("custom-session-id".to_string())
        );
        assert_eq!(deserialized.new_model_id, Some("grok-3".to_string()));
    }

    #[test]
    fn test_fork_session_request_without_optional_fields() {
        let json = r#"{"sourceSessionId":"abc123","sourceCwd":"/old","newCwd":"/new"}"#;
        let deserialized: ForkSessionRequest = serde_json::from_str(json).unwrap();

        assert_eq!(deserialized.source_session_id, "abc123");
        assert_eq!(deserialized.new_session_id, None);
        assert_eq!(deserialized.new_model_id, None);
    }

    #[test]
    fn test_fork_session_response_serialization() {
        let response = ForkSessionResponse {
            new_session_id: "fork-abc123-12345678".to_string(),
            chat_messages_copied: 42,
            updates_copied: 156,
            plan_state_copied: true,
            new_cwd: "/new/project".to_string(),
            parent_session_id: "abc123".to_string(),
            new_model_id: Some("grok-3".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        let deserialized: ForkSessionResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.new_session_id, "fork-abc123-12345678");
        assert_eq!(deserialized.chat_messages_copied, 42);
        assert_eq!(deserialized.updates_copied, 156);
        assert!(deserialized.plan_state_copied);
        assert_eq!(deserialized.new_cwd, "/new/project");
        assert_eq!(deserialized.parent_session_id, "abc123");
        assert_eq!(deserialized.new_model_id, Some("grok-3".to_string()));
    }

    #[test]
    fn test_fork_session_response_without_model_override() {
        let response = ForkSessionResponse {
            new_session_id: "fork-abc123-12345678".to_string(),
            chat_messages_copied: 42,
            updates_copied: 156,
            plan_state_copied: true,
            new_cwd: "/new/project".to_string(),
            parent_session_id: "abc123".to_string(),
            new_model_id: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("new_model_id"));
    }

    #[tokio::test]
    async fn fork_request_kind_materializes_on_child() {
        use crate::session::persistence::default_model_id;
        use crate::session::storage::StorageAdapter;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
        let source = Info {
            id: acp::SessionId::new("parent-kind"),
            cwd: "/src".to_string(),
        };
        adapter
            .init_session(&source, default_model_id())
            .await
            .unwrap();

        for (wire, expected) in [
            (Some("headless"), "headless"),
            (Some("fork"), "fork"),
            (None, "fork"),
        ] {
            let mut body = serde_json::json!({
                "sourceSessionId": source.id.to_string(),
                "sourceCwd": source.cwd,
                "newCwd": "/dst",
                "newSessionId": format!("child-{expected}-{}", wire.unwrap_or("omit")),
            });
            if let Some(kind) = wire
                && let Some(obj) = body.as_object_mut()
            {
                obj.insert("sessionKind".into(), serde_json::Value::String(kind.into()));
            }
            let request: ForkSessionRequest = serde_json::from_value(body).unwrap();
            let target = Info {
                id: acp::SessionId::new(request.new_session_id.clone().unwrap()),
                cwd: request.new_cwd.clone(),
            };
            adapter
                .copy_session_data(
                    &source,
                    &target,
                    CopySessionOptions {
                        parent_session_id: Some(request.source_session_id),
                        session_kind: request.session_kind,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let loaded = adapter.load_session(&target).await.unwrap();
            assert_eq!(
                loaded.summary.session_kind.as_deref(),
                Some(expected),
                "wire sessionKind={wire:?}"
            );
        }
    }

    fn side_fork_request(source: &str, child: &str) -> ForkSessionRequest {
        ForkSessionRequest {
            source_session_id: source.into(),
            source_cwd: "/src".into(),
            new_cwd: "/dst".into(),
            new_session_id: Some(child.into()),
            session_kind: Some(SIDE_SESSION_KIND.into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn side_fork_copies_chat_but_not_parent_operational_updates() {
        use crate::sampling::ConversationItem;
        use crate::session::persistence::default_model_id;
        use crate::session::storage::SessionUpdate;
        use crate::session::storage::StorageAdapter;
        use crate::session::usage_file::{SessionUsageFile, UsageSummary};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
        let parent = Info {
            id: acp::SessionId::new("side-copy-parent"),
            cwd: "/src".into(),
        };
        adapter
            .init_session(&parent, default_model_id())
            .await
            .unwrap();
        let mut parent_usage = SessionUsageFile::new(parent.id.to_string());
        parent_usage.apply_turn(
            1,
            "parent-turn",
            &UsageSummary {
                input_tokens: 10,
                total_tokens: 10,
                model_calls: 1,
                turn_count: 1,
                ..Default::default()
            },
            None,
        );
        adapter.write_usage(&parent, &parent_usage).await.unwrap();
        adapter
            .append_chat_message(&parent, &ConversationItem::user("retain this context"))
            .await
            .unwrap();
        for update in [
            xai_update(
                "side-copy-parent",
                serde_json::json!({
                    "sessionUpdate": "subagent_spawned",
                    "subagent_id": "child",
                    "parent_session_id": "side-copy-parent",
                    "child_session_id": "child-session",
                    "subagent_type": "explore",
                    "description": "parent task",
                }),
            ),
            xai_update(
                "side-copy-parent",
                serde_json::json!({
                    "sessionUpdate": "background_tasks",
                    "tasks": [],
                }),
            ),
            xai_update(
                "side-copy-parent",
                serde_json::json!({
                    "sessionUpdate": "task_backgrounded",
                    "tool_call_id": "call-parent",
                    "task_id": "task-parent",
                    "command": "parent command",
                    "cwd": "/src",
                    "output_file": "/tmp/parent.log",
                }),
            ),
            xai_update(
                "side-copy-parent",
                serde_json::json!({
                    "sessionUpdate": "scheduled_task_created",
                    "task_id": "scheduled-parent",
                    "prompt": "parent scheduled prompt",
                    "human_schedule": "every minute",
                    "next_fire_at": null,
                }),
            ),
            xai_update(
                "side-copy-parent",
                serde_json::json!({
                    "sessionUpdate": "scheduled_task_fired",
                    "task_id": "scheduled-parent",
                    "prompt": "parent scheduled prompt",
                    "human_schedule": "every minute",
                    "next_fire_at": null,
                    "subagent_id": null,
                }),
            ),
            xai_update(
                "side-copy-parent",
                serde_json::json!({
                    "sessionUpdate": "scheduled_task_deleted",
                    "task_id": "scheduled-parent",
                    "reason": "user_cancelled",
                }),
            ),
            xai_update(
                "side-copy-parent",
                serde_json::json!({
                    "sessionUpdate": "session_recap",
                    "summary": "contextual recap",
                }),
            ),
        ] {
            adapter.append_update(&parent, &update).await.unwrap();
        }

        let side = fork_session_with_storage(
            side_fork_request("side-copy-parent", "side-copy-child"),
            adapter.clone(),
            "agent",
            None,
        )
        .await
        .unwrap();
        let side_info = Info {
            id: acp::SessionId::new(side.new_session_id),
            cwd: "/dst".into(),
        };
        let copied = adapter.load_session(&side_info).await.unwrap();
        assert!(adapter.read_usage(&side_info).await.unwrap().is_none());
        assert_eq!(copied.chat_history.len(), 1);
        assert!(matches!(
            &copied.chat_history[0],
            ConversationItem::User(user)
                if serde_json::to_string(&user.content)
                    .unwrap()
                    .contains("retain this context")
        ));
        assert_eq!(copied.updates.len(), 1, "keep contextual recap only");
        assert!(!copied.updates.iter().any(|update| matches!(
            update,
            SessionUpdate::Xai(n) if matches!(
                &n.update,
                crate::extensions::notification::SessionUpdate::ScheduledTaskCreated { task_id, .. }
                    | crate::extensions::notification::SessionUpdate::ScheduledTaskFired { task_id, .. }
                    | crate::extensions::notification::SessionUpdate::ScheduledTaskDeleted { task_id, .. }
                    if task_id == "scheduled-parent"
            )
        )));
        assert!(matches!(
            &copied.updates[0],
            SessionUpdate::Xai(n) if matches!(
                &n.update,
                crate::extensions::notification::SessionUpdate::SessionRecap { .. }
            )
        ));

        let own_task = xai_update(
            "side-copy-child",
            serde_json::json!({
                "sessionUpdate": "task_backgrounded",
                "tool_call_id": "call-side",
                "task_id": "task-side",
                "command": "side command",
                "cwd": "/dst",
                "output_file": "/tmp/side.log",
            }),
        );
        adapter.append_update(&side_info, &own_task).await.unwrap();
        let own_schedule = xai_update(
            "side-copy-child",
            serde_json::json!({
                "sessionUpdate": "scheduled_task_created",
                "task_id": "scheduled-side",
                "prompt": "side scheduled prompt",
                "human_schedule": "every hour",
                "next_fire_at": null,
            }),
        );
        adapter
            .append_update(&side_info, &own_schedule)
            .await
            .unwrap();
        let reloaded = adapter.load_session(&side_info).await.unwrap();
        assert!(reloaded.updates.iter().any(|update| matches!(
            update,
            SessionUpdate::Xai(n) if matches!(
                &n.update,
                crate::extensions::notification::SessionUpdate::ScheduledTaskCreated { task_id, .. }
                    if task_id == "scheduled-side"
            )
        )));
        assert!(reloaded.updates.iter().any(|update| matches!(
            update,
            SessionUpdate::Xai(n) if matches!(
                &n.update,
                crate::extensions::notification::SessionUpdate::TaskBackgrounded { task_id, .. }
                    if task_id == "task-side"
            )
        )));

        let regular = fork_session_with_storage(
            ForkSessionRequest {
                source_session_id: "side-copy-parent".into(),
                source_cwd: "/src".into(),
                new_cwd: "/dst".into(),
                new_session_id: Some("regular-copy-child".into()),
                ..Default::default()
            },
            adapter.clone(),
            "agent",
            None,
        )
        .await
        .unwrap();
        let regular_info = Info {
            id: acp::SessionId::new(regular.new_session_id),
            cwd: "/dst".into(),
        };
        let regular_copy = adapter.load_session(&regular_info).await.unwrap();
        assert!(adapter.read_usage(&regular_info).await.unwrap().is_some());
        assert!(
            regular_copy.updates.iter().any(|update| matches!(
                update,
                SessionUpdate::Xai(n) if matches!(
                    &n.update,
                    crate::extensions::notification::SessionUpdate::TaskBackgrounded { task_id, .. }
                        if task_id == "task-parent"
                )
            )),
            "ordinary fork must retain parent operational updates"
        );
    }

    #[tokio::test]
    async fn refork_with_same_id_returns_same_child() {
        use crate::session::persistence::default_model_id;
        use crate::session::storage::StorageAdapter;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
        let source = Info {
            id: acp::SessionId::new("side-parent"),
            cwd: "/src".into(),
        };
        adapter
            .init_session(&source, default_model_id())
            .await
            .unwrap();

        let first = fork_session_with_storage(
            side_fork_request("side-parent", "side-child"),
            adapter.clone(),
            "agent",
            None,
        )
        .await
        .unwrap();
        let second = fork_session_with_storage(
            side_fork_request("side-parent", "side-child"),
            adapter.clone(),
            "agent",
            None,
        )
        .await
        .unwrap();

        assert_eq!(first.new_session_id, second.new_session_id);
        assert_eq!(second.new_session_id, "side-child");
        assert_eq!(second.parent_session_id, "side-parent");
    }

    #[tokio::test]
    async fn refork_with_different_parent_conflicts() {
        use crate::session::persistence::default_model_id;
        use crate::session::storage::StorageAdapter;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
        for id in ["side-parent", "other-parent"] {
            adapter
                .init_session(
                    &Info {
                        id: acp::SessionId::new(id),
                        cwd: "/src".into(),
                    },
                    default_model_id(),
                )
                .await
                .unwrap();
        }

        fork_session_with_storage(
            side_fork_request("side-parent", "side-child"),
            adapter.clone(),
            "agent",
            None,
        )
        .await
        .unwrap();

        let err = fork_session_with_storage(
            side_fork_request("other-parent", "side-child"),
            adapter.clone(),
            "agent",
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[tokio::test]
    async fn side_fork_is_hidden_and_deleted_on_close() {
        use crate::session::persistence::default_model_id;
        use crate::session::storage::StorageAdapter;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
        let parent = Info {
            id: acp::SessionId::new("side-parent"),
            cwd: "/src".into(),
        };
        adapter
            .init_session(&parent, default_model_id())
            .await
            .unwrap();
        fork_session_with_storage(
            side_fork_request("side-parent", "side-child"),
            adapter.clone(),
            "agent",
            None,
        )
        .await
        .unwrap();

        let child = Info {
            id: acp::SessionId::new("side-child"),
            cwd: "/dst".into(),
        };

        // Hidden from listings.
        let listed = adapter.list_sessions(None).await.unwrap();
        assert!(
            listed.iter().all(|s| s.info.id.0.as_ref() != "side-child"),
            "side fork must not appear in list_sessions"
        );
        assert!(listed.iter().any(|s| s.info.id.0.as_ref() == "side-parent"));

        // Deleted on close; a non-side session is untouched.
        assert!(adapter.delete_if_kind_side(&child).await.unwrap());
        assert!(adapter.load_summary(&child).await.is_err());
        assert!(!adapter.delete_if_kind_side(&parent).await.unwrap());
        assert!(adapter.load_summary(&parent).await.is_ok());
    }
}
