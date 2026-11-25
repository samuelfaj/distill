// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::create_test_actor;
use crate::session::concurrent_work::{REMINDER_TAG, install_for_test};
use crate::session::persistence::PersistenceMsg;
use chrono::Utc;
use distill_active_sessions::{ActiveSession, register_in};

const REPO: &str = "/repo/.git";

#[tokio::test(flavor = "current_thread")]
async fn user_turn_gets_the_reminder_and_autonomous_turn_only_heartbeats() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(50_000, 256_000, 85, gateway_tx, persistence_tx).await;
            let root = tempfile::tempdir().unwrap();
            let _guard = install_for_test(
                root.path(),
                &actor.session_info.id,
                std::path::Path::new("/repo"),
                REPO,
            );
            let mut peer = ActiveSession::new(
                agent_client_protocol::SessionId::new("peer"),
                std::process::id(),
                "/repo".into(),
            );
            peer.repo_root = Some(REPO.into());
            peer.set_objective_if_unset("migrate the billing tables");
            peer.record_edits(&["/repo/billing.rs".into()], Utc::now());
            register_in(root.path(), peer).unwrap();

            actor.inject_concurrent_work_reminder(None).await;
            assert_eq!(
                actor.chat_state_handle.get_conversation_len().await,
                0,
                "autonomous turns get no reminder"
            );

            actor
                .inject_concurrent_work_reminder(Some("add a login page"))
                .await;
            let conv = actor.chat_state_handle.get_conversation().await;
            assert_eq!(conv.len(), 1);
            let text = conv.first().unwrap().text_content();
            assert!(text.contains(&format!("<{REMINDER_TAG}>")), "{text}");
            assert!(text.contains("migrate the billing tables"), "{text}");
            assert!(text.contains("billing.rs"), "{text}");

            let own = distill_active_sessions::list_in(root.path())
                .unwrap()
                .into_iter()
                .find(|s| s.session_id == actor.session_info.id)
                .unwrap();
            assert_eq!(own.objective.as_deref(), Some("add a login page"));
        })
        .await;
}
