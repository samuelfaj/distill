// Modified for Distill by Samuel Fajreldines, 2026.
//! Round-trip test against the live backend: pushes one session, then pulls it back.
//!
//! Run with: `cargo test -p distill-shell -- pull_smoke --ignored --nocapture`

#[cfg(test)]
mod tests {
    use crate::remote::client::BackendClient;
    use crate::session::storage::{JsonlStorageAdapter, StorageAdapter};
    use distill_login::GrokAuth;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn load_prod_auth() -> Option<GrokAuth> {
        let path = crate::util::distill_home::distill_home().join("auth.json");
        let contents = std::fs::read_to_string(&path).ok()?;
        let store: BTreeMap<String, GrokAuth> = serde_json::from_str(&contents).ok()?;
        let scope = distill_login::GrokComConfig::default().auth_scope();
        distill_login::lookup_auth(&store, &scope)
    }

    /// Full round trip through the real production `RemoteSync` path.
    /// Creates a `RemoteSync`, queues ACP notifications, flushes, and verifies the backend row.
    /// Then pulls the session back and verifies local hydration and the storage adapter load.
    #[tokio::test]
    #[ignore]
    async fn smoke_push_pull_round_trip() {
        use crate::remote::sync::RemoteSync;
        use crate::session::export::ExportedMetadata;
        use agent_client_protocol::{
            ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent,
        };

        let auth = load_prod_auth().expect("No auth.json — run `grok login`");
        let am = Arc::new(distill_login::AuthManager::new(
            &crate::util::distill_home::distill_home(),
            distill_login::GrokComConfig::default(),
        ));
        am.hot_swap(auth);
        let client = BackendClient::new().with_auth_manager(am.clone());

        let session_id = format!("test-rt-{}", uuid::Uuid::new_v4());
        let test_cwd = "/tmp/smoke-test".to_string();
        let test_title = "Push-Pull Round Trip Test";

        // PUSH via RemoteSync (real production path)
        let metadata = ExportedMetadata {
            title: Some(test_title.into()),
            cwd: test_cwd.clone(),
            model_id: Some("grok-3".into()),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            total_messages: None,
            parent_session_id: None,
            agent_id: None,
            session_kind: None,
            subagent_type: None,
            subagent_persona: None,
            subagent_role: None,
            fork_context_source: None,
            subagent_depth: None,
            title_is_manual: None,
        };

        let sync = RemoteSync::new(
            session_id.clone(),
            metadata,
            BackendClient::new().with_auth_manager(am.clone()),
        );

        let sid = agent_client_protocol::SessionId::new(Arc::from(session_id.as_str()));
        sync.queue(SessionNotification::new(
            sid.clone(),
            SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("Hello from smoke test — user".to_string()),
            ))),
        ));
        sync.queue(SessionNotification::new(
            sid.clone(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("Hello from smoke test — agent".to_string()),
            ))),
        ));
        sync.flush();
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

        // Verify backend has cwd, title, messages
        let loaded = client
            .load_session_data(&session_id)
            .await
            .expect("load after push failed");
        let remote = loaded.session.as_ref().expect("no session row");
        assert_eq!(remote.cwd.as_deref(), Some(test_cwd.as_str()));
        assert_eq!(remote.title.as_deref(), Some(test_title));
        assert!(loaded.messages.as_ref().map_or(0, |m| m.len()) >= 2);

        // PULL back to local
        let result = crate::remote::pull_session_to_local(&session_id, &client)
            .await
            .expect("pull failed");
        let pulled = match result {
            crate::remote::PullResult::Hydrated(info) => info,
            crate::remote::PullResult::NotFound => panic!("pull returned NotFound"),
        };
        assert_eq!(pulled.cwd, test_cwd);

        // Verify local storage loads
        let local_dir = crate::session::persistence::session_dir(&pulled);
        assert!(local_dir.join("summary.json").exists());
        assert!(local_dir.join("updates.jsonl").exists());

        let storage = JsonlStorageAdapter::default();
        let data = storage
            .load_session_without_updates(&pulled)
            .await
            .expect("storage load failed");
        assert_eq!(data.summary.session_summary, test_title);

        // Verify chat_history has both turns
        let chat =
            std::fs::read_to_string(local_dir.join("chat_history.jsonl")).unwrap_or_default();
        assert!(chat.contains("user"), "chat_history missing user turn");
        assert!(chat.contains("agent"), "chat_history missing agent turn");

        // Cleanup
        drop(sync);
        let _ = client.delete_session_data(&session_id).await;
        let _ = std::fs::remove_dir_all(&local_dir);
    }
}
