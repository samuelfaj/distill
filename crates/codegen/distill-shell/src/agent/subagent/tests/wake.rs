// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

#[derive(Clone)]
struct RunShellChildTestRunner {
    contexts: std::sync::Arc<parking_lot::Mutex<std::collections::VecDeque<SubagentSpawnContext>>>,
    complete_first: std::sync::Arc<std::sync::atomic::AtomicBool>,
    gateway: GatewaySender,
}

impl RunShellChildTestRunner {
    fn new(
        contexts: impl IntoIterator<Item = SubagentSpawnContext>,
        complete_first: bool,
        gateway: GatewaySender,
    ) -> Self {
        Self {
            contexts: std::sync::Arc::new(parking_lot::Mutex::new(contexts.into_iter().collect())),
            complete_first: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(complete_first)),
            gateway,
        }
    }
}

impl distill_tools::implementations::distill::task::coordinator::ChildRunner
    for RunShellChildTestRunner
{
    type Control = ShellChildRuntime;
    type RootControl = distill_tools::implementations::distill::task::root_control::NoRootControl;
    type CompletionData = ShellCompletionData;
    type RunFuture = distill_tools::implementations::distill::task::coordinator::LocalBoxFuture<
        ChildRunOutput<ShellCompletionData>,
    >;
    type ValidateFuture =
        distill_tools::implementations::distill::task::coordinator::LocalBoxFuture<
            SubagentValidateTypeOutcome,
        >;
    type DescribeFuture =
        distill_tools::implementations::distill::task::coordinator::LocalBoxFuture<
            SubagentDescribeOutcome,
        >;

    fn run(
        &self,
        run: distill_tools::implementations::distill::task::coordinator::ChildRunRequest<
            Self::Control,
        >,
    ) -> Self::RunFuture {
        if self
            .complete_first
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Box::pin(std::future::ready(ChildRunOutput {
                result: SubagentResult {
                    success: true,
                    output: std::sync::Arc::from("prior output"),
                    subagent_id: run.request.id.clone(),
                    child_session_id: run.request.id,
                    tool_calls: 2,
                    turns: 1,
                    duration_ms: 7,
                    ..Default::default()
                },
                completion_data: ShellCompletionData::default(),
                snapshot_ref: Some("refs/grok/subagents/prior".to_owned()),
            }));
        }
        let ctx = self.contexts.lock().pop_front().expect("run context");
        let gateway = self.gateway.clone();
        let completion_data = ShellCompletionData::from_context(&ctx, run.attempt_id.clone(), None);
        Box::pin(async move { run_shell_child(run, ctx, completion_data, gateway, None).await })
    }

    fn validate_type(
        &self,
        _subagent_type: String,
        _parent_session_id: String,
    ) -> Self::ValidateFuture {
        Box::pin(std::future::ready(SubagentValidateTypeOutcome::Ok))
    }

    fn describe_type(
        &self,
        _subagent_type: String,
        _harness_agent_type: Option<String>,
        _parent_session_id: String,
    ) -> Self::DescribeFuture {
        Box::pin(std::future::ready(SubagentDescribeOutcome::Unavailable))
    }

    fn supports_wake(&self) -> bool {
        true
    }

    fn on_completed(
        &self,
        completion: ChildCompletion<Self::CompletionData>,
        terminal_published: Box<dyn FnOnce() + Send>,
    ) {
        present_child_completion(completion, &self.gateway, false);
        terminal_published();
    }
}

fn prior_wake_meta(id: &str, model_id: &str) -> SubagentMeta {
    SubagentMeta {
        subagent_id: id.to_owned(),
        attempt_id: Some("at1.prior".to_owned()),
        parent_session_id: "setup-parent".to_owned(),
        child_session_id: id.to_owned(),
        subagent_type: "general-purpose".to_owned(),
        description: "prior description".to_owned(),
        prompt: "prior prompt".to_owned(),
        status: "completed".to_owned(),
        started_at: chrono::Utc::now(),
        completed_at: Some(chrono::Utc::now()),
        duration_ms: Some(7),
        tool_calls: Some(2),
        turns: Some(1),
        error: None,
        effective_context_source: Some("new".to_owned()),
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: None,
        child_cwd: Some("/tmp".to_owned()),
        worktree_path: None,
        snapshot_ref: Some("refs/grok/subagents/prior".to_owned()),
        effective_model_id: Some(model_id.to_owned()),
        effort_auto: None,
        model_routing_locked: None,
    }
}

async fn assert_wake_setup_failure_preserves_prior_durable_state(
    build_failure: impl FnOnce(std::path::PathBuf) -> SubagentSetupFailure,
) {
    use crate::session::storage::StorageAdapter;
    use distill_sampling_types::conversation::ConversationItem;
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };

    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.sampling_config.model = "test".to_owned();
    ctx.model_id = acp::ModelId::new("test");
    ctx.parent_agent_name = Some("general-purpose".to_owned());
    let meta_dir = tempfile::tempdir().expect("meta dir");
    let id = uuid::Uuid::now_v7().to_string();
    let prior_meta = prior_wake_meta(&id, "test");
    assert!(write_subagent_meta(meta_dir.path(), &prior_meta));
    assert!(write_subagent_output(meta_dir.path(), "prior output"));
    ctx.setup_failure = Some(build_failure(meta_dir.path().to_path_buf()));
    ctx.parent_session_id = "setup-parent".into();
    ctx.parent_cwd = std::path::PathBuf::from("/tmp");
    let child_info = SessionInfo {
        id: acp::SessionId::new(id.clone()),
        cwd: "/tmp".to_owned(),
    };
    let storage = crate::session::storage::jsonl::JsonlStorageAdapter::with_root(
        crate::util::distill_home::distill_home(),
    );
    storage
        .init_session(&child_info, acp::ModelId::new("test"))
        .await
        .expect("session");
    storage
        .append_chat_message(&child_info, &ConversationItem::system("prior system"))
        .await
        .expect("system message");
    storage
        .append_chat_message(&child_info, &ConversationItem::assistant("prior work"))
        .await
        .expect("assistant message");
    storage
        .update_current_model(&child_info, &acp::ModelId::new("prior-model"))
        .await
        .expect("prior model");
    ctx.model_id = acp::ModelId::new("rejected-wake-model");
    ctx.sampling_config.model = "rejected-wake-model".to_owned();
    let child_session_dir = crate::session::persistence::session_dir(&child_info);
    let prior_transcript =
        std::fs::read(child_session_dir.join("chat_history.jsonl")).expect("prior transcript");
    let prior_summary_bytes =
        std::fs::read(child_session_dir.join("summary.json")).expect("prior summary");
    let prior_summary: crate::session::persistence::Summary =
        serde_json::from_slice(&prior_summary_bytes).expect("parse prior summary");
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    ctx.parent_cmd_tx = Some(parent_cmd_tx);
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let (command_tx, command_rx) = SubagentCoordinator::<RunShellChildTestRunner>::channel();
    let coordinator = tokio::task::spawn_local(
        SubagentCoordinator::from_channel(
            command_rx,
            RunShellChildTestRunner::new([ctx], true, gateway),
            CoordinatorConfig::default(),
        )
        .run(),
    );
    let backend = ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
    assert!(
        backend
            .spawn(auto_wake_test_request(&id), None)
            .await
            .expect("prior spawn")
            .success
    );
    while parent_cmd_rx.try_recv().is_ok() {}
    while gateway_rx.try_recv().is_ok() {}
    assert_eq!(
        backend
            .send_active_message(
                ActiveAgentMessageRequest::try_new(&id, "continue").expect("wake request")
            )
            .await,
        ActiveAgentMessageOutcome::NotActiveOrFinalizing
    );
    let restored = backend
        .query(&id, false, None)
        .await
        .expect("restored prior snapshot");
    assert!(matches!(
        restored.status,
        SubagentSnapshotStatus::Completed { ref output, .. } if output == "prior output"
    ));
    drop(backend);
    coordinator.await.expect("coordinator");

    let restored_meta: SubagentMeta = serde_json::from_str(
        &std::fs::read_to_string(meta_dir.path().join("meta.json")).expect("meta"),
    )
    .expect("metadata");
    assert_eq!(prior_meta.attempt_id, restored_meta.attempt_id);
    assert_eq!(prior_meta.status, restored_meta.status);
    assert_eq!(prior_meta.completed_at, restored_meta.completed_at);
    assert_eq!(prior_meta.snapshot_ref, restored_meta.snapshot_ref);
    assert_eq!(prior_meta.error, restored_meta.error);
    assert_eq!(
        read_subagent_output(meta_dir.path()).as_deref(),
        Some("prior output")
    );
    assert_eq!(
        std::fs::read(child_session_dir.join("chat_history.jsonl"))
            .expect("transcript after rejected wake"),
        prior_transcript,
    );
    let restored_summary_bytes =
        std::fs::read(child_session_dir.join("summary.json")).expect("restored summary");
    let restored_summary: crate::session::persistence::Summary =
        serde_json::from_slice(&restored_summary_bytes).expect("parse restored summary");
    assert_eq!(restored_summary_bytes, prior_summary_bytes);
    assert_eq!(
        restored_summary.current_model_id,
        prior_summary.current_model_id
    );
    assert_eq!(
        restored_summary.next_trace_turn,
        prior_summary.next_trace_turn
    );
    assert_eq!(restored_summary.attempt_id, prior_summary.attempt_id);
    assert!(parent_cmd_rx.try_recv().is_err());
    assert!(gateway_rx.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn wake_setup_failures_preserve_prior_durable_state_and_lifecycle() {
    tokio::task::LocalSet::new()
        .run_until(async {
            assert_wake_setup_failure_preserves_prior_durable_state(|meta_dir| {
                SubagentSetupFailure::SamplingClient { meta_dir }
            })
            .await;
            let blocker = tempfile::NamedTempFile::new().expect("blocker");
            assert_wake_setup_failure_preserves_prior_durable_state(|meta_dir| {
                SubagentSetupFailure::Persistence {
                    meta_dir,
                    persistence_dir: blocker.path().to_path_buf(),
                }
            })
            .await;
        })
        .await;
}

fn configure_completion_harness(
    ctx: &mut SubagentSpawnContext,
    server: &distill_test_support::MockInferenceServer,
    harness: RunShellChildHarnessConfig,
) {
    ctx.run_shell_child_harness = Some(harness);
    ctx.parent_session_id = "setup-parent".into();
    ctx.sampling_config.base_url = server.url();
    ctx.sampling_config.model = "test-model".into();
    ctx.sampling_config.api_backend = crate::sampling::ApiBackend::Responses;
    ctx.model_id = acp::ModelId::new("test-model");
}

fn resume_policy_model_entry(base_url: &str) -> crate::agent::config::ModelEntry {
    let mut entry = crate::agent::config::ModelEntry::fallback(
        "vendor/pinned-wire",
        &crate::agent::config::EndpointsConfig::default(),
    );
    entry.info.base_url = base_url.to_owned();
    entry.info.api_backend = crate::sampling::ApiBackend::Responses;
    entry.info.model = "vendor/pinned-wire".to_owned();
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::High);
    entry.info.reasoning_efforts = vec![
        distill_sampling_types::ReasoningEffortOption {
            id: "low".to_owned(),
            value: distill_sampling_types::ReasoningEffort::Low,
            label: "Low".to_owned(),
            description: Some("short routine call".to_owned()),
            default: false,
        },
        distill_sampling_types::ReasoningEffortOption {
            id: "high".to_owned(),
            value: distill_sampling_types::ReasoningEffort::High,
            label: "High".to_owned(),
            description: Some("deep reasoning call".to_owned()),
            default: true,
        },
    ];
    entry.api_key = Some("test-resume-policy-key".to_owned());
    entry
}

#[test]
fn resumed_child_uses_persisted_jev_policy_over_parent_context() {
    std::thread::Builder::new()
        .name("subagent-policy-resume-test".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(async {
                use distill_tools::implementations::distill::task::backend::{
                    ChannelBackend, SubagentBackend,
                };
                use distill_tools::implementations::distill::task::coordinator::{
                    CoordinatorConfig, SubagentCoordinator,
                };

                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async {
                        let meta_dir = tempfile::tempdir().expect("meta dir");
                        let server = distill_test_support::MockInferenceServer::start()
                            .await
                            .expect("mock server");
                        server.set_response("completed output");
                        let id = uuid::Uuid::now_v7().to_string();
                        let mut ordinary_entry = resume_policy_model_entry(&server.url());
                        ordinary_entry.info.model = "test-model".to_owned();

                        let mut ordinary_ctx = ctx_with_toggle(HashMap::new());
                        configure_completion_harness(
                            &mut ordinary_ctx,
                            &server,
                            RunShellChildHarnessConfig::new(
                                meta_dir.path().to_path_buf(),
                                InitialAttemptBehavior::Normal,
                            ),
                        );
                        ordinary_ctx.model_id = acp::ModelId::new("ordinary-key");
                        ordinary_ctx.sampling_config.reasoning_effort =
                            Some(distill_sampling_types::ReasoningEffort::Low);
                        ordinary_ctx
                            .available_models
                            .insert("ordinary-key".to_owned(), ordinary_entry.clone());
                        ordinary_ctx
                            .models_manager
                            .insert_test_entry("ordinary-key", ordinary_entry.clone());
                        ordinary_ctx.parent_effort_auto = true;
                        ordinary_ctx.auto_wake_enabled = false;

                        let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
                        ordinary_ctx.parent_cmd_tx = Some(parent_cmd_tx.clone());
                        let usage_ack =
                            tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));

                        let mut wake_ctx = ctx_with_toggle(HashMap::new());
                        configure_completion_harness(
                            &mut wake_ctx,
                            &server,
                            RunShellChildHarnessConfig::new(
                                meta_dir.path().to_path_buf(),
                                InitialAttemptBehavior::Normal,
                            ),
                        );
                        wake_ctx.model_id = acp::ModelId::new("ordinary-key");
                        wake_ctx.sampling_config.reasoning_effort =
                            Some(distill_sampling_types::ReasoningEffort::High);
                        wake_ctx
                            .available_models
                            .insert("ordinary-key".to_owned(), ordinary_entry.clone());
                        wake_ctx
                            .models_manager
                            .insert_test_entry("ordinary-key", ordinary_entry);
                        // Make the durable source, rather than the new parent snapshot,
                        // observable on resume.
                        wake_ctx.parent_effort_auto = false;
                        wake_ctx.auto_wake_enabled = false;
                        wake_ctx.parent_cmd_tx = Some(parent_cmd_tx);

                        let (gateway, _gateway_rx) = test_gateway_with_receiver();
                        let (command_tx, command_rx) =
                            SubagentCoordinator::<RunShellChildTestRunner>::channel();
                        let coordinator = tokio::task::spawn_local(
                            SubagentCoordinator::from_channel(
                                command_rx,
                                RunShellChildTestRunner::new(
                                    [ordinary_ctx, wake_ctx],
                                    false,
                                    gateway,
                                ),
                                CoordinatorConfig::default(),
                            )
                            .run(),
                        );
                        let backend =
                            ChannelBackend::for_coordinator_session(command_tx, "setup-parent");

                        let mut request = auto_wake_test_request(&id);
                        request.fork_context = true;
                        request.runtime_overrides.reasoning_effort = Some("auto".to_owned());
                        let ordinary = backend
                            .spawn(request, None)
                            .await
                            .expect("ordinary spawn");
                        assert!(
                            ordinary.success,
                            "ordinary spawn failed: {:?}",
                            ordinary.error
                        );
                        let created: SubagentMeta = serde_json::from_str(
                            &std::fs::read_to_string(meta_dir.path().join("meta.json"))
                                .expect("created metadata"),
                        )
                        .expect("parse created metadata");
                        assert_eq!(
                            created.effort_auto,
                            Some(true),
                            "child creation must persist the parent's auto policy"
                        );
                        assert_eq!(
                            created.effective_model_id.as_deref(),
                            Some("ordinary-key"),
                            "ordinary child must persist the catalog key, not the wire model"
                        );
                        assert_eq!(
                            created.model_routing_locked,
                            Some(false),
                            "an ordinary auto child must not invent a model pin"
                        );

                        assert!(matches!(
                            backend
                                .send_active_message(
                                    ActiveAgentMessageRequest::try_new(&id, "continue")
                                        .expect("wake request")
                                )
                                .await,
                            ActiveAgentMessageOutcome::Accepted { .. }
                        ));
                        let resumed = backend
                            .query(&id, true, Some(5_000))
                            .await
                            .expect("resumed completion");
                        assert!(matches!(
                            resumed.status,
                            SubagentSnapshotStatus::Completed { .. }
                        ));
                        let resumed_meta: SubagentMeta = serde_json::from_str(
                            &std::fs::read_to_string(meta_dir.path().join("meta.json"))
                                .expect("resumed metadata"),
                        )
                        .expect("parse resumed metadata");
                        assert_eq!(
                            resumed_meta.effort_auto,
                            Some(true),
                            "resume must retain the source policy even when the new context is manual"
                        );
                        assert_eq!(
                            resumed_meta.effective_model_id.as_deref(),
                            Some("ordinary-key"),
                            "resume must retain the canonical catalog key"
                        );
                        assert_eq!(
                            resumed_meta.model_routing_locked,
                            Some(false),
                            "ordinary auto resume must remain eligible for worker routing"
                        );
                        let requests: Vec<_> = server
                            .request_bodies()
                            .into_iter()
                            .filter(|body| body.get("model").is_some())
                            .collect();
                        assert!(
                            requests.len() >= 2,
                            "fork creation and wake must both dispatch: {requests:?}"
                        );
                        for request in [requests.first().unwrap(), requests.last().unwrap()] {
                            assert_eq!(
                                request
                                    .pointer("/reasoning/effort")
                                    .and_then(|value| value.as_str()),
                                Some("low"),
                                "the parent's live numeric effort must survive definition/wake defaults"
                            );
                        }

                        drop(backend);
                        coordinator.await.expect("coordinator");
                        usage_ack.abort();
                    })
                    .await;
            });
        })
        .expect("spawn policy test thread")
        .join()
        .expect("policy test thread");
}

#[test]
fn explicit_model_auto_resume_keeps_catalog_identity_and_wire_pin() {
    std::thread::Builder::new()
        .name("subagent-canonical-model-resume-test".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(async {
                use distill_tools::implementations::distill::task::backend::{
                    ChannelBackend, SubagentBackend,
                };
                use distill_tools::implementations::distill::task::coordinator::{
                    CoordinatorConfig, SubagentCoordinator,
                };

                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async {
                        let meta_dir = tempfile::tempdir().expect("meta dir");
                        let server = distill_test_support::MockInferenceServer::start()
                            .await
                            .expect("mock server");
                        server.set_response("completed output");
                        let id = uuid::Uuid::now_v7().to_string();
                        let entry = resume_policy_model_entry(&server.url());
                        let mut agent_definition =
                            distill_agent::config::AgentDefinition::general_purpose();
                        agent_definition.name = "resume-policy".to_owned();
                        agent_definition.effort = Some(distill_agent::config::Effort::High);
                        let mut agent_config = crate::agent::config::Config::default();
                        agent_config.cli_agents = vec![agent_definition];

                        let mut ordinary_ctx = ctx_with_toggle(HashMap::new());
                        configure_completion_harness(
                            &mut ordinary_ctx,
                            &server,
                            RunShellChildHarnessConfig::new(
                                meta_dir.path().to_path_buf(),
                                InitialAttemptBehavior::Normal,
                            ),
                        );
                        ordinary_ctx.agent_config = Some(agent_config.clone());
                        ordinary_ctx.parent_effort_auto = false;
                        // The catalog and definition both advertise High, but the
                        // live session's numeric fallback is explicitly Low.
                        ordinary_ctx.sampling_config.reasoning_effort =
                            Some(distill_sampling_types::ReasoningEffort::Low);
                        ordinary_ctx
                            .available_models
                            .insert("pinned-key".to_owned(), entry.clone());
                        ordinary_ctx
                            .models_manager
                            .insert_test_entry("pinned-key", entry.clone());

                        let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
                        ordinary_ctx.parent_cmd_tx = Some(parent_cmd_tx.clone());
                        let usage_ack =
                            tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));

                        let mut wake_ctx = ctx_with_toggle(HashMap::new());
                        configure_completion_harness(
                            &mut wake_ctx,
                            &server,
                            RunShellChildHarnessConfig::new(
                                meta_dir.path().to_path_buf(),
                                InitialAttemptBehavior::Normal,
                            ),
                        );
                        wake_ctx.agent_config = Some(agent_config);
                        wake_ctx.parent_effort_auto = false;
                        // A wake context with a different baseline must not
                        // replace the durable Low fallback.
                        wake_ctx.sampling_config.reasoning_effort =
                            Some(distill_sampling_types::ReasoningEffort::High);
                        wake_ctx.parent_cmd_tx = Some(parent_cmd_tx);
                        wake_ctx
                            .available_models
                            .insert("pinned-key".to_owned(), entry.clone());
                        wake_ctx
                            .models_manager
                            .insert_test_entry("pinned-key", entry);

                        let (gateway, _gateway_rx) = test_gateway_with_receiver();
                        let (command_tx, command_rx) =
                            SubagentCoordinator::<RunShellChildTestRunner>::channel();
                        let coordinator = tokio::task::spawn_local(
                            SubagentCoordinator::from_channel(
                                command_rx,
                                RunShellChildTestRunner::new(
                                    [ordinary_ctx, wake_ctx],
                                    false,
                                    gateway,
                                ),
                                CoordinatorConfig::default(),
                            )
                            .run(),
                        );
                        let backend =
                            ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
                        let mut request = auto_wake_test_request(&id);
                        request.subagent_type = "resume-policy".to_owned();
                        request.runtime_overrides.model = Some("pinned-key".to_owned());
                        request.runtime_overrides.reasoning_effort = Some("auto".to_owned());
                        let ordinary = backend.spawn(request, None).await.expect("ordinary spawn");
                        assert!(
                            ordinary.success,
                            "explicit model/auto spawn failed: {:?}",
                            ordinary.error
                        );
                        let created: SubagentMeta = serde_json::from_str(
                            &std::fs::read_to_string(meta_dir.path().join("meta.json"))
                                .expect("created metadata"),
                        )
                        .expect("parse created metadata");
                        assert_eq!(created.effective_model_id.as_deref(), Some("pinned-key"));
                        assert_eq!(created.effort_auto, Some(true));
                        assert_eq!(created.model_routing_locked, Some(true));

                        assert!(matches!(
                            backend
                                .send_active_message(
                                    ActiveAgentMessageRequest::try_new(&id, "continue")
                                        .expect("wake request")
                                )
                                .await,
                            ActiveAgentMessageOutcome::Accepted { .. }
                        ));
                        let resumed = backend
                            .query(&id, true, Some(5_000))
                            .await
                            .expect("resumed completion");
                        assert!(matches!(
                            resumed.status,
                            SubagentSnapshotStatus::Completed { .. }
                        ));
                        let resumed_meta: SubagentMeta = serde_json::from_str(
                            &std::fs::read_to_string(meta_dir.path().join("meta.json"))
                                .expect("resumed metadata"),
                        )
                        .expect("parse resumed metadata");
                        assert_eq!(
                            resumed_meta.effective_model_id.as_deref(),
                            Some("pinned-key")
                        );
                        assert_eq!(resumed_meta.effort_auto, Some(true));
                        assert_eq!(resumed_meta.model_routing_locked, Some(true));

                        let requests: Vec<_> = server
                            .request_bodies()
                            .into_iter()
                            .filter(|body| body.get("model").is_some())
                            .collect();
                        assert!(
                            requests.len() >= 2,
                            "create and resume must both dispatch: {requests:?}"
                        );
                        assert_eq!(requests[0]["model"], "vendor/pinned-wire");
                        assert_eq!(
                            requests.last().and_then(|body| body.get("model")),
                            Some(&serde_json::json!("vendor/pinned-wire"))
                        );
                        for request in [requests.first().unwrap(), requests.last().unwrap()] {
                            assert_eq!(
                                request
                                    .pointer("/reasoning/effort")
                                    .and_then(|value| value.as_str()),
                                Some("low"),
                                "auto fallback must retain the explicitly seeded base effort when no decision is available"
                            );
                        }

                        drop(backend);
                        coordinator.await.expect("coordinator");
                        usage_ack.abort();
                    })
                    .await;
            });
        })
        .expect("spawn canonical model resume test thread")
        .join()
        .expect("canonical model resume test thread");
}

async fn acknowledge_parent_usage(mut parent_cmd_rx: mpsc::UnboundedReceiver<SessionCommand>) {
    while let Some(command) = parent_cmd_rx.recv().await {
        if let SessionCommand::RecordSubagentUsage { respond_to, .. } = command {
            let _ = respond_to.send(());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unpublished_wake_completion_preserves_prior_durable_state_and_worktree() {
    distill_test_utils::require_git!();
    use crate::session::storage::StorageAdapter;
    use distill_test_utils::git::{run_git, seed_repo_with_remote};
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let temp = tempfile::TempDir::new().expect("tempdir");
            let (repo, _remote) = seed_repo_with_remote(temp.path());
            let id = uuid::Uuid::now_v7().to_string();
            let worktree = temp.path().join("wake-worktree");
            distill_fast_worktree::WorktreeBuilder::new(&repo, &worktree)
                .create()
                .expect("worktree");
            let meta_dir = temp.path().join("meta");
            let mut prior_meta = prior_wake_meta(&id, "test-model");
            prior_meta.child_cwd = Some(worktree.to_string_lossy().into_owned());
            prior_meta.worktree_path = Some(worktree.to_string_lossy().into_owned());
            prior_meta.snapshot_ref = Some("refs/grok/subagents/prior".to_owned());
            let prior_head = run_git(&repo, &["rev-parse", "HEAD"]);
            let prior_ref = prior_meta.snapshot_ref.as_deref().expect("snapshot ref");
            run_git(&repo, &["update-ref", prior_ref, &prior_head]);
            assert!(write_subagent_meta(&meta_dir, &prior_meta));
            assert!(write_subagent_output(&meta_dir, "prior output"));

            let child_info = SessionInfo {
                id: acp::SessionId::new(id.clone()),
                cwd: worktree.to_string_lossy().into_owned(),
            };
            let storage = crate::session::storage::jsonl::JsonlStorageAdapter::with_root(
                crate::util::distill_home::distill_home(),
            );
            storage
                .init_session(&child_info, acp::ModelId::new("test-model"))
                .await
                .expect("session");
            storage
                .append_chat_message(
                    &child_info,
                    &distill_sampling_types::conversation::ConversationItem::system("prior system"),
                )
                .await
                .expect("system message");
            storage
                .append_chat_message(
                    &child_info,
                    &distill_sampling_types::conversation::ConversationItem::assistant(
                        "prior work",
                    ),
                )
                .await
                .expect("assistant message");

            let server = distill_test_support::MockInferenceServer::start()
                .await
                .expect("mock server");
            let mut ctx = ctx_with_toggle(HashMap::new());
            let completion_harness = RunShellChildHarnessConfig::new(
                meta_dir.clone(),
                InitialAttemptBehavior::CompleteBeforeAdmission,
            );
            configure_completion_harness(&mut ctx, &server, completion_harness.clone());
            ctx.parent_cwd = repo.clone();
            let mut config = crate::agent::config::Config::default();
            config.feature_values.insert(
                crate::agent::config::Feature::SubagentWorktreeSnapshot,
                true,
            );
            ctx.agent_config = Some(config);
            let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
            ctx.parent_cmd_tx = Some(parent_cmd_tx);
            let usage_ack = tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));
            let (gateway, mut gateway_rx) = test_gateway_with_receiver();
            let (command_tx, command_rx) =
                SubagentCoordinator::<RunShellChildTestRunner>::channel();
            let coordinator = tokio::task::spawn_local(
                SubagentCoordinator::from_channel(
                    command_rx,
                    RunShellChildTestRunner::new([ctx], true, gateway),
                    CoordinatorConfig::default(),
                )
                .run(),
            );
            let backend = ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
            assert!(
                backend
                    .spawn(auto_wake_test_request(&id), None)
                    .await
                    .expect("prior spawn")
                    .success
            );
            while gateway_rx.try_recv().is_ok() {}

            assert_eq!(
                backend
                    .send_active_message(
                        ActiveAgentMessageRequest::try_new(&id, "continue").expect("wake request")
                    )
                    .await,
                ActiveAgentMessageOutcome::NotActiveOrFinalizing
            );
            let restored = backend
                .query(&id, false, None)
                .await
                .expect("restored prior snapshot");
            assert!(matches!(
                restored.status,
                SubagentSnapshotStatus::Completed { ref output, .. }
                    if output == "prior output"
            ));
            drop(backend);
            coordinator.await.expect("coordinator");
            usage_ack.abort();

            let restored_meta: SubagentMeta = serde_json::from_str(
                &std::fs::read_to_string(meta_dir.join("meta.json")).expect("meta"),
            )
            .expect("metadata");
            assert_eq!(restored_meta.attempt_id, prior_meta.attempt_id);
            assert_eq!(restored_meta.status, prior_meta.status);
            assert_eq!(restored_meta.completed_at, prior_meta.completed_at);
            assert_eq!(restored_meta.snapshot_ref, prior_meta.snapshot_ref);
            assert_eq!(run_git(&repo, &["rev-parse", prior_ref]), prior_head);
            assert_eq!(
                read_subagent_output(&meta_dir).as_deref(),
                Some("prior output")
            );
            assert!(worktree.is_dir());
            assert!(
                std::iter::from_fn(|| gateway_rx.try_recv().ok()).all(|message| {
                    !matches!(
                        message,
                        distill_acp_lib::AcpClientMessage::ExtNotification(args)
                            if args.request.params.get().contains("subagent_spawned")
                    )
                }),
                "unpublished wake must not emit SubagentSpawned"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_spawn_with_failed_metadata_write_persists_output_and_disposes_worktree() {
    distill_test_utils::require_git!();
    use distill_test_utils::git::seed_repo_with_remote;
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let temp = tempfile::TempDir::new().expect("tempdir");
            let (repo, _remote) = seed_repo_with_remote(temp.path());
            let meta_dir = temp.path().join("meta");
            let server = distill_test_support::MockInferenceServer::start()
                .await
                .expect("mock server");
            server.set_response("ordinary output");
            let id = uuid::Uuid::now_v7().to_string();
            let mut ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(
                &mut ctx,
                &server,
                RunShellChildHarnessConfig::new(meta_dir.clone(), InitialAttemptBehavior::Normal),
            );
            ctx.parent_cwd = repo;
            ctx.fail_start_metadata_write = true;
            let mut config = crate::agent::config::Config::default();
            config.feature_values.insert(
                crate::agent::config::Feature::SubagentWorktreeSnapshot,
                true,
            );
            ctx.agent_config = Some(config);
            let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
            ctx.parent_cmd_tx = Some(parent_cmd_tx);
            let usage_ack = tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));
            let (gateway, _gateway_rx) = test_gateway_with_receiver();
            let (command_tx, command_rx) =
                SubagentCoordinator::<RunShellChildTestRunner>::channel();
            let coordinator = tokio::task::spawn_local(
                SubagentCoordinator::from_channel(
                    command_rx,
                    RunShellChildTestRunner::new([ctx], false, gateway),
                    CoordinatorConfig::default(),
                )
                .run(),
            );
            let backend = ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
            let mut request = auto_wake_test_request(&id);
            request.runtime_overrides.isolation =
                Some(distill_tool_types::SubagentIsolationMode::Worktree);
            let result = backend.spawn(request, None).await.expect("ordinary spawn");
            assert!(result.success);
            let output = result.output.to_string();
            assert_eq!(
                read_subagent_output(&meta_dir).as_deref(),
                Some(output.as_str())
            );
            let persisted: SubagentMeta = serde_json::from_str(
                &std::fs::read_to_string(meta_dir.join("meta.json")).expect("completion meta"),
            )
            .expect("metadata");
            assert_eq!(persisted.status, "completed");
            let worktree = persisted.worktree_path.as_deref().expect("worktree path");
            assert!(!std::path::Path::new(worktree).exists());
            assert!(persisted.snapshot_ref.is_some());

            drop(backend);
            coordinator.await.expect("coordinator");
            usage_ack.abort();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_spawn_disposes_worktree_when_only_remote_settings_enable_snapshot() {
    distill_test_utils::require_git!();
    use distill_test_utils::git::seed_repo_with_remote;
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let temp = tempfile::TempDir::new().expect("tempdir");
            let (repo, _remote) = seed_repo_with_remote(temp.path());
            let meta_dir = temp.path().join("meta");
            let server = distill_test_support::MockInferenceServer::start()
                .await
                .expect("mock server");
            server.set_response("ordinary output");
            let id = uuid::Uuid::now_v7().to_string();
            let mut ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(
                &mut ctx,
                &server,
                RunShellChildHarnessConfig::new(meta_dir.clone(), InitialAttemptBehavior::Normal),
            );
            ctx.parent_cwd = repo;
            ctx.remote_settings = Some(crate::util::config::RemoteSettings {
                subagent_worktree_snapshot_enabled: Some(true),
                ..Default::default()
            });
            let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
            ctx.parent_cmd_tx = Some(parent_cmd_tx);
            let usage_ack = tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));
            let (gateway, _gateway_rx) = test_gateway_with_receiver();
            let (command_tx, command_rx) =
                SubagentCoordinator::<RunShellChildTestRunner>::channel();
            let coordinator = tokio::task::spawn_local(
                SubagentCoordinator::from_channel(
                    command_rx,
                    RunShellChildTestRunner::new([ctx], false, gateway),
                    CoordinatorConfig::default(),
                )
                .run(),
            );
            let backend = ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
            let mut request = auto_wake_test_request(&id);
            request.runtime_overrides.isolation =
                Some(distill_tool_types::SubagentIsolationMode::Worktree);
            let result = backend.spawn(request, None).await.expect("ordinary spawn");
            assert!(result.success);
            let persisted: SubagentMeta = serde_json::from_str(
                &std::fs::read_to_string(meta_dir.join("meta.json")).expect("completion meta"),
            )
            .expect("metadata");
            assert_eq!(persisted.status, "completed");
            let worktree = persisted.worktree_path.as_deref().expect("worktree path");
            assert!(
                !std::path::Path::new(worktree).exists(),
                "remote subagent_worktree_snapshot_enabled must reach the post-spawn dispose gate"
            );
            assert!(persisted.snapshot_ref.is_some());

            drop(backend);
            coordinator.await.expect("coordinator");
            usage_ack.abort();
        })
        .await;
}

/// The child's actor no longer binds itself; `run_shell_child` binds it once the actor is up,
/// ahead of the first turn, and teardown releases it.
#[tokio::test(flavor = "current_thread")]
async fn ordinary_spawn_binds_the_child_workspace_session_before_its_first_turn() {
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let temp = tempfile::TempDir::new().expect("tempdir");
            let meta_dir = temp.path().join("meta");
            let server = distill_test_support::MockInferenceServer::start()
                .await
                .expect("mock server");
            server.set_response("bound output");
            // Park the child's first turn at the model so its binding is observable mid-turn.
            server.hold_agent_completions();
            let id = uuid::Uuid::now_v7().to_string();
            let mut ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(
                &mut ctx,
                &server,
                RunShellChildHarnessConfig::new(meta_dir.clone(), InitialAttemptBehavior::Normal),
            );
            ctx.parent_cwd = temp.path().to_path_buf();
            let parent_chat = spawn_test_parent_chat_state("test-model");
            parent_chat.replace_conversation(vec![
                distill_sampling_types::conversation::ConversationItem::system(
                    "parent system",
                ),
                distill_sampling_types::conversation::ConversationItem::user(
                    "UNRELATED_PARENT_HISTORY_MARKER",
                ),
            ]);
            ctx.parent_chat_state = Some(parent_chat);
            let workspace_ops = ctx.workspace_ops.clone();
            let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
            ctx.parent_cmd_tx = Some(parent_cmd_tx);
            let usage_ack = tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));
            let (gateway, _gateway_rx) = test_gateway_with_receiver();
            let (command_tx, command_rx) =
                SubagentCoordinator::<RunShellChildTestRunner>::channel();
            let coordinator = tokio::task::spawn_local(
                SubagentCoordinator::from_channel(
                    command_rx,
                    RunShellChildTestRunner::new([ctx], false, gateway),
                    CoordinatorConfig::default(),
                )
                .run(),
            );
            let backend = ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
            let mut request = auto_wake_test_request(&id);
            request.runtime_overrides.model_override_provenance =
                distill_tools::implementations::distill::task::types::ModelOverrideProvenance::Tool;
            request.prompt =
                "OBJECTIVE required objective INSTRUCTIONS required instructions EVIDENCE required evidence PATH_HANDLE required path handle"
                    .to_owned();
            let spawned =
                tokio::task::spawn_local(async move { backend.spawn(request, None).await });
            let first_turn_at_model = async {
                while !server.requests().into_iter().any(|request| {
                    request
                        .header("x-grok-turn-idx")
                        .is_some_and(|value| !value.is_empty())
                }) {
                    tokio::task::yield_now().await;
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(30), first_turn_at_model)
                .await
                .expect("the child's first turn reaches the model");
            let workspace = workspace_ops
                .workspace_handle()
                .expect("local workspace ops");
            assert!(
                workspace.session(&id).is_some(),
                "the child's toolset is bound before its first turn dispatches"
            );
            server.release_agent_completions();
            let result = spawned.await.expect("spawn task").expect("ordinary spawn");
            assert!(result.success);
            let child_request = server
                .requests()
                .into_iter()
                .find_map(|request| {
                    let is_foreground = request
                        .header("x-grok-turn-idx")
                        .is_some_and(|value| !value.is_empty());
                    let body = request.body?;
                    (is_foreground
                        && serde_json::to_string(&body)
                            .is_ok_and(|text| text.contains("OBJECTIVE")))
                    .then_some(body)
                })
                .expect("the fresh child's request reaches the model");
            let child_request_text =
                serde_json::to_string(&child_request).expect("serialize child request");
            for marker in ["OBJECTIVE", "INSTRUCTIONS", "EVIDENCE", "PATH_HANDLE"] {
                assert!(
                    child_request_text.contains(marker),
                    "child request missing supplied handoff marker {marker}"
                );
            }
            assert!(!child_request_text.contains("UNRELATED_PARENT_HISTORY_MARKER"));
            assert!(
                workspace.session(&id).is_none(),
                "teardown releases the child's binding"
            );

            coordinator.await.expect("coordinator");
            usage_ack.abort();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unacked_wake_start_and_abort_fail_closed_without_parking_runner() {
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let meta_dir = tempfile::tempdir().expect("meta dir");
            let server = distill_test_support::MockInferenceServer::start()
                .await
                .expect("mock server");
            server.set_response("completed output");
            let id = uuid::Uuid::now_v7().to_string();
            let mut ordinary_ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(
                &mut ordinary_ctx,
                &server,
                RunShellChildHarnessConfig::new(
                    meta_dir.path().to_path_buf(),
                    InitialAttemptBehavior::Normal,
                ),
            );
            let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
            ordinary_ctx.parent_cmd_tx = Some(parent_cmd_tx.clone());
            let usage_ack = tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));
            let mut wake_ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(
                &mut wake_ctx,
                &server,
                RunShellChildHarnessConfig::new(
                    meta_dir.path().to_path_buf(),
                    InitialAttemptBehavior::Normal,
                )
                .hold_wake_flush_acks(),
            );
            wake_ctx.parent_cmd_tx = Some(parent_cmd_tx);
            let child_cwd = ordinary_ctx.parent_cwd.to_string_lossy().into_owned();
            let (gateway, mut gateway_rx) = test_gateway_with_receiver();
            let (command_tx, command_rx) =
                SubagentCoordinator::<RunShellChildTestRunner>::channel();
            let coordinator = tokio::task::spawn_local(
                SubagentCoordinator::from_channel(
                    command_rx,
                    RunShellChildTestRunner::new([ordinary_ctx, wake_ctx], false, gateway),
                    CoordinatorConfig::default(),
                )
                .run(),
            );
            let backend = ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
            let ordinary = backend
                .spawn(auto_wake_test_request(&id), None)
                .await
                .expect("ordinary spawn");
            assert!(
                ordinary.success,
                "ordinary spawn failed: {:?}",
                ordinary.error
            );
            let child_info = SessionInfo {
                id: acp::SessionId::new(id.clone()),
                cwd: child_cwd,
            };
            let child_session_dir = crate::session::persistence::session_dir(&child_info);
            let prior_transcript = std::fs::read(child_session_dir.join("chat_history.jsonl"))
                .expect("prior transcript");
            let prior_summary: crate::session::persistence::Summary = serde_json::from_slice(
                &std::fs::read(child_session_dir.join("summary.json")).expect("prior summary"),
            )
            .expect("parse prior summary");
            let prior_meta =
                std::fs::read(meta_dir.path().join("meta.json")).expect("prior metadata");
            while gateway_rx.try_recv().is_ok() {}

            let wake = backend
                .send_active_message(
                    ActiveAgentMessageRequest::try_new(&id, "continue").expect("wake request"),
                )
                .await;
            assert_eq!(wake, ActiveAgentMessageOutcome::NotActiveOrFinalizing);
            let restored = backend
                .query(&id, true, Some(5_000))
                .await
                .expect("restored prior snapshot");
            assert!(matches!(
                restored.status,
                SubagentSnapshotStatus::Completed { ref output, .. }
                    if output == "completed output"
            ));
            assert_eq!(
                std::fs::read(child_session_dir.join("chat_history.jsonl"))
                    .expect("transcript after timeout"),
                prior_transcript
            );
            assert_eq!(
                std::fs::read(meta_dir.path().join("meta.json")).expect("metadata after timeout"),
                prior_meta
            );
            let restored_summary: crate::session::persistence::Summary = serde_json::from_slice(
                &std::fs::read(child_session_dir.join("summary.json"))
                    .expect("summary after timeout"),
            )
            .expect("parse summary after timeout");
            assert_eq!(restored_summary.attempt_id, prior_summary.attempt_id);
            assert_eq!(
                restored_summary.next_trace_turn,
                prior_summary.next_trace_turn
            );
            assert!(
                std::iter::from_fn(|| gateway_rx.try_recv().ok()).all(|message| {
                    !matches!(
                        message,
                        distill_acp_lib::AcpClientMessage::ExtNotification(args)
                            if args.request.params.get().contains("subagent_spawned")
                    )
                }),
                "timed-out wake must not publish a new lifecycle"
            );

            drop(backend);
            coordinator.await.expect("coordinator");
            usage_ack.abort();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_deferred_start_restores_prior_without_publication() {
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };

    tokio::task::LocalSet::new()
        .run_until(async {
            let meta_dir = tempfile::tempdir().expect("meta dir");
            let server = distill_test_support::MockInferenceServer::start()
                .await
                .expect("mock server");
            server.set_response("completed output");
            let id = uuid::Uuid::now_v7().to_string();
            let mut ordinary_ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(
                &mut ordinary_ctx,
                &server,
                RunShellChildHarnessConfig::new(
                    meta_dir.path().to_path_buf(),
                    InitialAttemptBehavior::Normal,
                ),
            );
            let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
            ordinary_ctx.parent_cmd_tx = Some(parent_cmd_tx.clone());
            let usage_ack = tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));
            let child_cwd = ordinary_ctx.parent_cwd.to_string_lossy().into_owned();
            let mut wake_ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(
                &mut wake_ctx,
                &server,
                RunShellChildHarnessConfig::new(
                    meta_dir.path().to_path_buf(),
                    InitialAttemptBehavior::Normal,
                )
                .reject_deferred_start_commit(),
            );
            wake_ctx.parent_cmd_tx = Some(parent_cmd_tx);
            let (gateway, mut gateway_rx) = test_gateway_with_receiver();
            let (command_tx, command_rx) =
                SubagentCoordinator::<RunShellChildTestRunner>::channel();
            let coordinator = tokio::task::spawn_local(
                SubagentCoordinator::from_channel(
                    command_rx,
                    RunShellChildTestRunner::new([ordinary_ctx, wake_ctx], false, gateway),
                    CoordinatorConfig::default(),
                )
                .run(),
            );
            let backend = ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
            let ordinary = backend
                .spawn(auto_wake_test_request(&id), None)
                .await
                .expect("ordinary spawn");
            assert!(
                ordinary.success,
                "ordinary spawn failed: {:?}",
                ordinary.error
            );
            let child_info = SessionInfo {
                id: acp::SessionId::new(id.clone()),
                cwd: child_cwd,
            };
            let child_session_dir = crate::session::persistence::session_dir(&child_info);
            let prior_summary =
                std::fs::read(child_session_dir.join("summary.json")).expect("prior summary");
            let prior_meta =
                std::fs::read(meta_dir.path().join("meta.json")).expect("prior metadata");
            assert!(write_subagent_output(
                meta_dir.path(),
                "prior output sentinel"
            ));
            while gateway_rx.try_recv().is_ok() {}

            assert_eq!(
                backend
                    .send_active_message(
                        ActiveAgentMessageRequest::try_new(&id, "continue").expect("wake request")
                    )
                    .await,
                ActiveAgentMessageOutcome::NotActiveOrFinalizing
            );
            let restored = backend
                .query(&id, true, Some(5_000))
                .await
                .expect("restored prior completion");
            assert!(
                matches!(restored.status, SubagentSnapshotStatus::Completed { .. }),
                "expected restored completion, got {:?}",
                restored.status
            );
            assert_eq!(
                std::fs::read(meta_dir.path().join("meta.json")).expect("restored metadata"),
                prior_meta
            );
            assert_eq!(
                std::fs::read(child_session_dir.join("summary.json")).expect("restored summary"),
                prior_summary
            );
            assert_eq!(
                read_subagent_output(meta_dir.path()).as_deref(),
                Some("prior output sentinel")
            );
            assert!(
                std::iter::from_fn(|| gateway_rx.try_recv().ok()).all(|message| {
                    !matches!(
                        message,
                        distill_acp_lib::AcpClientMessage::ExtNotification(args)
                            if args.request.params.get().contains("subagent_spawned")
                    )
                }),
                "rejected settle must not publish SubagentSpawned"
            );

            drop(backend);
            coordinator.await.expect("coordinator");
            usage_ack.abort();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn started_wake_with_failed_metadata_write_preserves_prior_durable_artifacts() {
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let meta_dir = tempfile::tempdir().expect("meta dir");
            let server = distill_test_support::MockInferenceServer::start()
                .await
                .expect("mock server");
            server.set_response("completed output");
            let id = uuid::Uuid::now_v7().to_string();
            let mut ordinary_ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(
                &mut ordinary_ctx,
                &server,
                RunShellChildHarnessConfig::new(
                    meta_dir.path().to_path_buf(),
                    InitialAttemptBehavior::Normal,
                ),
            );
            let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
            ordinary_ctx.parent_cmd_tx = Some(parent_cmd_tx.clone());
            let usage_ack = tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));
            let mut wake_ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(
                &mut wake_ctx,
                &server,
                RunShellChildHarnessConfig::new(
                    meta_dir.path().to_path_buf(),
                    InitialAttemptBehavior::Normal,
                ),
            );
            wake_ctx.parent_cmd_tx = Some(parent_cmd_tx);
            wake_ctx.fail_start_metadata_write = true;
            let child_cwd = ordinary_ctx.parent_cwd.to_string_lossy().into_owned();
            let (gateway, mut gateway_rx) = test_gateway_with_receiver();
            let (command_tx, command_rx) =
                SubagentCoordinator::<RunShellChildTestRunner>::channel();
            let coordinator = tokio::task::spawn_local(
                SubagentCoordinator::from_channel(
                    command_rx,
                    RunShellChildTestRunner::new([ordinary_ctx, wake_ctx], false, gateway),
                    CoordinatorConfig::default(),
                )
                .run(),
            );
            let backend = ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
            let ordinary = backend
                .spawn(auto_wake_test_request(&id), None)
                .await
                .expect("ordinary spawn");
            assert!(
                ordinary.success,
                "ordinary spawn failed: {:?}",
                ordinary.error
            );
            let child_info = SessionInfo {
                id: acp::SessionId::new(id.clone()),
                cwd: child_cwd,
            };
            let child_session_dir = crate::session::persistence::session_dir(&child_info);
            let prior_summary: crate::session::persistence::Summary = serde_json::from_slice(
                &std::fs::read(child_session_dir.join("summary.json")).expect("prior summary"),
            )
            .expect("parse prior summary");
            let prior_meta =
                std::fs::read(meta_dir.path().join("meta.json")).expect("prior metadata");
            assert!(write_subagent_output(
                meta_dir.path(),
                "prior output sentinel"
            ));
            while gateway_rx.try_recv().is_ok() {}

            assert!(matches!(
                backend
                    .send_active_message(
                        ActiveAgentMessageRequest::try_new(&id, "continue").expect("wake request")
                    )
                    .await,
                ActiveAgentMessageOutcome::Accepted { .. }
            ));
            let wake = backend
                .query(&id, true, Some(5_000))
                .await
                .expect("wake completion");
            assert!(matches!(
                wake.status,
                SubagentSnapshotStatus::Completed { .. }
            ));

            assert_eq!(
                std::fs::read(meta_dir.path().join("meta.json")).expect("restored metadata"),
                prior_meta,
            );
            assert_eq!(
                read_subagent_output(meta_dir.path()).as_deref(),
                Some("prior output sentinel"),
            );
            let restored_summary: crate::session::persistence::Summary = serde_json::from_slice(
                &std::fs::read(child_session_dir.join("summary.json")).expect("restored summary"),
            )
            .expect("parse restored summary");
            assert_eq!(
                restored_summary.next_trace_turn,
                prior_summary.next_trace_turn.saturating_add(1)
            );
            assert_ne!(restored_summary.attempt_id, prior_summary.attempt_id);
            assert!(
                restored_summary
                    .attempt_id
                    .as_deref()
                    .and_then(distill_message_delivery_core::AttemptId::parse)
                    .is_some()
            );

            drop(backend);
            coordinator.await.expect("coordinator");
            usage_ack.abort();
        })
        .await;
}
