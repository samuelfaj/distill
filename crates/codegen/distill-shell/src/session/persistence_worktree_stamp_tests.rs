// Modified for Distill by Samuel Fajreldines, 2026.
use agent_client_protocol as acp;
use distill_test_support::EnvGuard;
use serial_test::serial;

use super::{
    ExplicitSessionIdentity, OaiCompatClient, Summary, default_model_id, new_with_explicit_dir,
};
use crate::session::info::Info;

fn worktree_cwd_under(home: &std::path::Path) -> String {
    let cwd = home
        .join("worktrees")
        .join("xai")
        .join("fix-bug")
        .join("src");
    std::fs::create_dir_all(&cwd).unwrap();
    cwd.to_string_lossy().into_owned()
}

#[test]
#[serial]
fn summary_new_stamps_kind_label_and_source_for_worktree_cwd() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let cwd = worktree_cwd_under(home.path());

    let summary = Summary::new(
        &Info {
            id: acp::SessionId::new("worktree-stamp"),
            cwd,
        },
        default_model_id(),
    )
    .unwrap();

    assert_eq!(summary.session_kind.as_deref(), Some("worktree"));
    assert_eq!(summary.worktree_label.as_deref(), Some("fix-bug"));
    assert!(summary.source_workspace_dir.is_none());
    assert!(!summary.is_hidden());
}

#[test]
#[serial]
fn summary_new_leaves_worktree_fields_unset_for_plain_cwd() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let plain_cwd = home.path().join("project");
    std::fs::create_dir_all(&plain_cwd).unwrap();

    let summary = Summary::new(
        &Info {
            id: acp::SessionId::new("plain-cwd"),
            cwd: plain_cwd.to_string_lossy().into_owned(),
        },
        default_model_id(),
    )
    .unwrap();

    assert!(summary.session_kind.is_none());
    assert!(summary.worktree_label.is_none());
    assert!(summary.source_workspace_dir.is_none());
}

#[tokio::test]
#[serial]
async fn new_with_explicit_dir_overrides_worktree_stamp_so_subagent_stays_hidden() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let cwd = worktree_cwd_under(home.path());
    let target_dir = home.path().join("child-session");

    let sampling_client = OaiCompatClient::new(distill_sampler::SamplerConfig::default()).unwrap();
    let _persistence = new_with_explicit_dir(
        &Info {
            id: acp::SessionId::new("subagent-in-worktree"),
            cwd,
        },
        target_dir.clone(),
        default_model_id(),
        sampling_client,
        "test-model".to_owned(),
        crate::session::persistence::ExplicitSessionOpen::New {
            identity: None,
            next_trace_turn: None,
        },
    )
    .await
    .unwrap();

    let summary: Summary =
        serde_json::from_slice(&std::fs::read(target_dir.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary.session_kind.as_deref(), Some("subagent"));
    assert!(summary.agent_id.is_none());
    assert!(summary.source_workspace_dir.is_none());
    assert!(summary.is_hidden());
}

#[tokio::test]
#[serial]
async fn new_with_explicit_dir_stores_requested_identity() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let target_dir = home.path().join("child-session");
    let agent_id =
        distill_message_delivery_core::AgentId::from_uuid_v7(uuid::Uuid::now_v7().to_string())
            .unwrap();
    let attempt_id = distill_message_delivery_core::AttemptId::mint(0x11);
    let persistence = new_with_explicit_dir(
        &Info {
            id: acp::SessionId::new(agent_id.to_string()),
            cwd: home.path().to_string_lossy().into_owned(),
        },
        target_dir.clone(),
        default_model_id(),
        OaiCompatClient::new(distill_sampler::SamplerConfig::default()).unwrap(),
        "test-model".to_owned(),
        crate::session::persistence::ExplicitSessionOpen::New {
            identity: Some(ExplicitSessionIdentity {
                agent_id: agent_id.clone(),
                attempt_id: attempt_id.clone(),
            }),
            next_trace_turn: Some(3),
        },
    )
    .await
    .unwrap();
    drop(persistence);

    let summary: Summary =
        serde_json::from_slice(&std::fs::read(target_dir.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary.agent_id.as_deref(), Some(agent_id.as_str()));
    assert_eq!(summary.attempt_id.as_deref(), Some(attempt_id.as_str()));
    assert_eq!(summary.next_trace_turn, 3);
}
