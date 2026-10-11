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

#[test]
fn child_completion_settlement_preserves_output_and_parent_usage() {
    use crate::session::persistence::{ExplicitSessionOpen, PersistenceMsg, new_with_explicit_dir};
    use crate::session::usage_file::{SessionUsageFile, UsageSummary};
    use distill_test_support::{
        InferenceEndpoint, InferenceRequestMatcher, MockInferenceServer, ScriptedResponse,
    };
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };

    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(tokio::task::LocalSet::new().run_until(async {
                for interrupt_wake in [false, true] {
                    tokio::time::timeout(std::time::Duration::from_secs(45), async {
                        let temp = tempfile::tempdir().unwrap();
                        let meta_dir = temp.path().join("meta");
                        let parent_dir = temp.path().join("parent");
                        let parent_chat = spawn_test_parent_chat_state("test-model");
                        let persistence = new_with_explicit_dir(
                            &SessionInfo {
                                id: acp::SessionId::new("setup-parent"),
                                cwd: temp.path().to_string_lossy().into_owned(),
                            },
                            parent_dir.clone(),
                            acp::ModelId::new("test-model"),
                            "parent usage".into(),
                            ExplicitSessionOpen::New {
                                identity: None,
                                next_trace_turn: None,
                            },
                        )
                        .await
                        .unwrap();
                        let server = MockInferenceServer::start().await.unwrap();
                        let matcher =
                            InferenceRequestMatcher::foreground(InferenceEndpoint::Responses);
                        let response = |text| {
                            ScriptedResponse::sse(
                                distill_test_support::sse::responses_api_script_exact(
                                    text,
                                    "test-model",
                                ),
                            )
                        };
                        let mut primary = server.expect_response_blocked(
                            "primary",
                            matcher,
                            response("primary result"),
                        );
                        let mut protected = server.expect_response_blocked(
                            "protected",
                            matcher,
                            response("protected result"),
                        );
                        let mut wake = interrupt_wake.then(|| {
                            server.expect_response("wake", matcher, ScriptedResponse::hang())
                        });
                        let (settlement_tx, mut settlement_rx) = mpsc::unbounded_channel();
                        let mut harness = RunShellChildHarnessConfig::new(
                            meta_dir.clone(),
                            InitialAttemptBehavior::Normal,
                        );
                        harness.completion_settlement_tx = Some(settlement_tx);
                        let mut ctx = ctx_with_toggle(HashMap::new());
                        configure_completion_harness(&mut ctx, &server, harness);
                        ctx.parent_cwd = temp.path().to_path_buf();
                        let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
                        ctx.parent_cmd_tx = Some(parent_cmd_tx);
                        let (gateway, _gateway_rx) = test_gateway_with_receiver();
                        let (command_tx, command_rx) =
                            SubagentCoordinator::<RunShellChildTestRunner>::channel();
                        let backend =
                            ChannelBackend::for_coordinator_session(command_tx, "setup-parent");
                        // ctx_with_toggle's stub receiver is closed. Usage freeze must
                        // query this real coordinator, including when no descendants exist.
                        ctx.subagent_event_tx = backend.sender();
                        let coordinator = tokio::task::spawn_local(
                            SubagentCoordinator::from_channel(
                                command_rx,
                                RunShellChildTestRunner::new([ctx], false, gateway),
                                CoordinatorConfig::default(),
                            )
                            .run(),
                        );
                        let id = uuid::Uuid::now_v7().to_string();
                        let mut request = auto_wake_test_request(&id);
                        request.prompt = "primary work".into();
                        request.parent_prompt_id = Some("parent-prompt".into());
                        request.run_in_background = false;
                        let spawned = tokio::task::spawn_local({
                            let backend = backend.clone();
                            async move { backend.spawn(request, None).await }
                        });
                        primary.wait_blocked().await;
                        while !matches!(
                            backend.query(&id, false, None).await.unwrap().status,
                            SubagentSnapshotStatus::Running { .. }
                        ) {
                            tokio::task::yield_now().await;
                        }
                        assert!(matches!(
                            backend
                                .send_active_message(
                                    ActiveAgentMessageRequest::try_new(&id, "protected followup")
                                        .unwrap()
                                )
                                .await,
                            ActiveAgentMessageOutcome::Accepted { .. }
                        ));
                        primary.release();
                        protected.wait_blocked().await;
                        assert!(
                            settlement_rx.try_recv().is_err(),
                            "finalization must wait for the accepted parent receipt"
                        );
                        protected.release();
                        let (child_cmd, release) = settlement_rx.recv().await.unwrap();
                        primary.assert_satisfied();
                        protected.assert_satisfied();

                        let (wake_reply, mut wake_result) = oneshot::channel();
                        if let Some(wake) = wake.as_mut() {
                            child_cmd
                                .send(SessionCommand::Prompt {
                                    prompt_id: "task-completed-background".into(),
                                    prompt_blocks: vec![acp::ContentBlock::Text(
                                        acp::TextContent::new("Background task completed."),
                                    )],
                                    prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                                    artifact_upload_ctx: None,
                                    client_identifier: None,
                                    screen_mode: None,
                                    verbatim: true,
                                    traceparent: None,
                                    json_schema: None,
                                    send_now: false,
                                    admission: None,
                                    tool_overrides_update: None,
                                    respond_to: wake_reply,
                                    prompt_admitted: None,
                                    persist_ack: None,
                                    parsed_prompt_tx: None,
                                })
                                .unwrap();
                            wake.wait_received().await;
                        }
                        release.send(()).unwrap();

                        // Consume the real runner's usage command, applying the canonical parent
                        // ledger and persistence primitives before acknowledging its fold.
                        let (
                            by_model,
                            attributions,
                            pending_attempts,
                            parent_prompt_id,
                            incomplete,
                            respond_to,
                        ) = loop {
                            if let SessionCommand::RecordSubagentUsage {
                                by_model,
                                attributions,
                                pending_attempts,
                                parent_prompt_id,
                                incomplete,
                                respond_to,
                            } = parent_cmd_rx.recv().await.unwrap()
                            {
                                break (
                                    by_model,
                                    attributions,
                                    pending_attempts,
                                    parent_prompt_id,
                                    incomplete,
                                    respond_to,
                                );
                            }
                        };
                        assert_eq!(parent_prompt_id.as_deref(), Some("parent-prompt"));
                        assert_eq!(incomplete, interrupt_wake);
                        if interrupt_wake {
                            assert_eq!(
                                wake_result
                                    .try_recv()
                                    .expect("wake settled before usage fold")
                                    .unwrap()
                                    .stop_reason,
                                acp::StopReason::Cancelled
                            );
                        }
                        assert!(
                            parent_chat
                                .record_subagent_usage_with_attributions_and_pending(
                                    by_model,
                                    attributions,
                                    pending_attempts,
                                    true,
                                    incomplete,
                                )
                                .await
                        );
                        let ledger = parent_chat.try_get_session_usage().await.unwrap();
                        assert_eq!(ledger.is_incomplete(), interrupt_wake);
                        assert_eq!(
                            ledger
                                .attributions
                                .iter()
                                .filter(|call| call.role == "main"
                                    && call.usage_complete
                                    && call.usage.is_some())
                                .count(),
                            2
                        );
                        assert!(
                            ledger.totals.total_tokens() >= 30,
                            "both completed model turns retain their reported usage"
                        );
                        persistence
                            .tx
                            .send(PersistenceMsg::UsageTurn {
                                turn_number: 1,
                                prompt_id: parent_prompt_id,
                                live: UsageSummary::from_ledger(&ledger),
                            })
                            .unwrap();
                        let (flushed, ack) = oneshot::channel();
                        persistence
                            .tx
                            .send(PersistenceMsg::FlushAndAck {
                                respond_to: flushed,
                            })
                            .unwrap();
                        ack.await.unwrap().unwrap();
                        assert!(
                            !spawned.is_finished(),
                            "terminal child result must wait for the parent fold acknowledgement"
                        );
                        respond_to.send(()).unwrap();
                        let result = spawned.await.unwrap().unwrap();
                        assert!(result.success, "{:?}", result.error);
                        assert!(!result.cancelled);
                        assert_eq!(&*result.output, "protected result");
                        assert_eq!(result.output_usage_incomplete, interrupt_wake);
                        assert_eq!(result.total_tokens_used, ledger.totals.total_tokens());
                        let persisted: SessionUsageFile = serde_json::from_slice(
                            &std::fs::read(parent_dir.join("usage.json")).unwrap(),
                        )
                        .unwrap();
                        assert_eq!(persisted.session.usage_is_incomplete, interrupt_wake);
                        assert_eq!(persisted.session.total_tokens, result.total_tokens_used);
                        assert_eq!(persisted.session.model_calls, ledger.totals.model_calls);
                        assert_eq!(
                            read_subagent_output(&meta_dir).as_deref(),
                            Some("protected result")
                        );
                        let meta: SubagentMeta = serde_json::from_slice(
                            &std::fs::read(meta_dir.join("meta.json")).unwrap(),
                        )
                        .unwrap();
                        assert_eq!(meta.status, "completed");
                        drop(child_cmd);
                        drop(backend);
                        coordinator.await.unwrap();
                    })
                    .await
                    .expect("child settlement integration is bounded");
                }
            }));
        })
        .expect("spawn large-stack child settlement test thread")
        .join()
        .expect("child settlement test thread");
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

/// Background children that finish when the test releases them (or on cancel). Completions take
/// the shell runner's wake routing but present inline, so the parent queue reads in order.
struct GatedChildRunner {
    gates: std::rc::Rc<std::cell::RefCell<HashMap<String, oneshot::Receiver<()>>>>,
    completion_data: ShellCompletionData,
    holds: WakeHolds,
    gateway: GatewaySender,
}

impl distill_tools::implementations::distill::task::coordinator::ChildRunner for GatedChildRunner {
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
        let gate = self
            .gates
            .borrow_mut()
            .remove(&run.request.id)
            .expect("every test child is gated");
        let completion_data = self.completion_data.clone();
        let id = run.request.id.clone();
        let cancellation = run.cancellation.clone();
        Box::pin(async move {
            let result = tokio::select! {
                _ = gate => SubagentResult {
                    success: true,
                    output: Arc::from(format!("{id} output")),
                    subagent_id: id.clone(),
                    child_session_id: id.clone(),
                    ..Default::default()
                },
                () = cancellation.cancelled() => {
                    SubagentResult::cancelled(id.clone(), id.clone(), "cancelled")
                }
            };
            ChildRunOutput {
                result,
                completion_data,
                snapshot_ref: None,
            }
        })
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
        false
    }

    fn on_completed(
        &self,
        completion: ChildCompletion<Self::CompletionData>,
        terminal_published: Box<dyn FnOnce() + Send>,
    ) {
        completion_presenter(&self.holds, completion, self.gateway.clone())();
        terminal_published();
    }

    fn parent_session_cancelled(&self, parent_session_id: &str) {
        self.holds.cancel(parent_session_id);
    }
}

/// A real coordinator for parent `"parent"` whose background children the test finishes one by one.
struct GatedWakeHarness {
    sender: distill_tools::implementations::distill::task::backend::SubagentCoordinatorSender,
    backend: distill_tools::implementations::distill::task::backend::ChannelBackend,
    gates: HashMap<String, oneshot::Sender<()>>,
    pending_gates: std::rc::Rc<std::cell::RefCell<HashMap<String, oneshot::Receiver<()>>>>,
    parent_cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
    _results: Vec<oneshot::Receiver<SubagentResult>>,
    _gateway_rx: mpsc::UnboundedReceiver<crate::test_support::lsp_runtime::GatewayOut>,
}

impl GatedWakeHarness {
    /// Call inside a `LocalSet`.
    fn start() -> Self {
        use distill_tools::implementations::distill::task::coordinator::{
            CoordinatorConfig, SubagentCoordinator,
        };
        let (gateway, gateway_rx) = test_gateway_with_receiver();
        let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
        let pending_gates = std::rc::Rc::default();
        let runner = GatedChildRunner {
            gates: std::rc::Rc::clone(&pending_gates),
            completion_data: ShellCompletionData {
                auto_wake_enabled: true,
                parent_cmd_tx: Some(parent_cmd_tx),
                task_output_tool_name: "get_command_or_subagent_output".into(),
                ..Default::default()
            },
            holds: WakeHolds::default(),
            gateway,
        };
        let (sender, receiver) = SubagentCoordinator::<GatedChildRunner>::channel();
        tokio::task::spawn_local(
            SubagentCoordinator::from_channel(
                receiver,
                runner,
                CoordinatorConfig {
                    buffer_completions: true,
                    ..Default::default()
                },
            )
            .run(),
        );
        Self {
            backend:
                distill_tools::implementations::distill::task::backend::ChannelBackend::for_coordinator_session(
                    sender.clone(),
                    "parent",
                ),
            sender,
            gates: HashMap::new(),
            pending_gates,
            parent_cmd_rx,
            _results: Vec::new(),
            _gateway_rx: gateway_rx,
        }
    }

    async fn spawn_background(&mut self, id: &str) {
        let (gate_tx, gate_rx) = oneshot::channel();
        self.pending_gates
            .borrow_mut()
            .insert(id.to_owned(), gate_rx);
        self.gates.insert(id.to_owned(), gate_tx);
        let (result_tx, result_rx) = oneshot::channel();
        let (registered_tx, registered_rx) = oneshot::channel();
        self.sender
            .send(SubagentEvent::Spawn(SubagentSpawnRequest {
                request: Box::new(auto_wake_test_request(id)),
                result_tx,
                registered_tx: Some(registered_tx),
            }))
            .expect("coordinator open");
        registered_rx.await.expect("background child registered");
        self._results.push(result_rx);
    }

    /// Releases the child; returns what reached the parent up to and including its presentation.
    async fn finish(&mut self, id: &str) -> Vec<SessionCommand> {
        let _ = self.gates.remove(id).expect("gated child").send(());
        self.commands_through_finish(id).await
    }

    async fn commands_through_finish(&mut self, id: &str) -> Vec<SessionCommand> {
        let mut commands = Vec::new();
        loop {
            let command = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.parent_cmd_rx.recv(),
            )
            .await
            .expect("the child's finish reaches the parent")
            .expect("parent channel open");
            let finished = matches!(
                &command,
                SessionCommand::XaiSessionNotification {
                    notification: SessionNotification {
                        update: SessionUpdate::SubagentFinished { subagent_id, .. },
                        ..
                    }
                } if subagent_id == id
            );
            commands.push(command);
            if finished {
                break;
            }
        }
        while let Ok(command) = self.parent_cmd_rx.try_recv() {
            commands.push(command);
        }
        commands
    }
}

/// `(prompt_id, text)` of every wake prompt among `commands`.
fn wake_prompts(commands: Vec<SessionCommand>) -> Vec<(String, String)> {
    commands
        .into_iter()
        .filter_map(|command| match command {
            SessionCommand::Prompt {
                prompt_id,
                prompt_blocks,
                ..
            } => Some((prompt_id, prompt_text(&prompt_blocks))),
            _ => None,
        })
        .collect()
}

/// Each wake is a full main-model round, so a batch of background children must cost one.
#[tokio::test(flavor = "current_thread")]
async fn background_batch_wakes_parent_once_after_the_last_child() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut harness = GatedWakeHarness::start();
            harness.spawn_background("sa-a").await;
            harness.spawn_background("sa-b").await;
            let early = wake_prompts(harness.finish("sa-a").await);
            assert!(
                early.is_empty(),
                "sa-b still runs, so sa-a must not wake yet: {early:?}"
            );
            let wakes = wake_prompts(harness.finish("sa-b").await);
            let [(prompt_id, body)] = wakes.as_slice() else {
                panic!("one wake for the batch, got {wakes:?}");
            };
            assert_eq!(prompt_id, "subagent-completed-sa-b");
            assert!(body.contains("2 background subagents completed:"), "{body}");
            for id in ["sa-a", "sa-b"] {
                assert!(
                    body.contains(&format!("Background subagent \"{id}\"")),
                    "{body}"
                );
                assert!(body.contains(&format!("{id} output")), "{body}");
            }
        })
        .await;
}

/// The parent already has a block-waited child's result; the held wake carries only the other.
#[tokio::test(flavor = "current_thread")]
async fn block_waited_child_stays_out_of_the_held_wake() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut harness = GatedWakeHarness::start();
            harness.spawn_background("sa-a").await;
            harness.spawn_background("sa-b").await;
            let early = wake_prompts(harness.finish("sa-a").await);
            assert!(
                early.is_empty(),
                "sa-b still runs, so sa-a must not wake yet: {early:?}"
            );
            let (respond_to, waited) = oneshot::channel();
            harness
                .sender
                .send(SubagentEvent::Query(SubagentQueryRequest {
                    subagent_id: "sa-b".into(),
                    parent_session_id: Some("parent".into()),
                    block: true,
                    timeout_ms: Some(60_000),
                    respond_to,
                }))
                .expect("coordinator open");
            // Commands run in order: once this answers, the blocking waiter is registered.
            harness.backend.registry_counts().await;
            let wakes = wake_prompts(harness.finish("sa-b").await);
            assert!(
                waited.await.expect("waiter answered").is_some(),
                "sa-b's result went to the blocking caller"
            );
            let [(prompt_id, body)] = wakes.as_slice() else {
                panic!("one wake for the held child, got {wakes:?}");
            };
            assert_eq!(prompt_id, "subagent-completed-sa-a");
            assert!(body.contains("sa-a output"), "{body}");
            assert!(!body.contains("sa-b"), "{body}");
            assert!(!body.contains("background subagents completed:"), "{body}");
        })
        .await;
}

/// A sibling that outlives the hold must not starve the parent: the held part goes out alone.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn held_wake_goes_out_alone_when_a_sibling_outlives_the_hold() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut harness = GatedWakeHarness::start();
            harness.spawn_background("sa-a").await;
            harness.spawn_background("sa-b").await;
            let early = wake_prompts(harness.finish("sa-a").await);
            assert!(
                early.is_empty(),
                "sa-b still runs, so sa-a must not wake yet: {early:?}"
            );
            tokio::time::advance(WAKE_HOLD_MAX - std::time::Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert!(
                harness.parent_cmd_rx.try_recv().is_err(),
                "the hold lasts WAKE_HOLD_MAX"
            );
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
            let partial = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                harness.parent_cmd_rx.recv(),
            )
            .await
            .expect("the hold deadline delivers")
            .expect("parent channel open");
            let partial = wake_prompts(vec![partial]);
            let [(prompt_id, body)] = partial.as_slice() else {
                panic!("the held wake goes out at the deadline, got {partial:?}");
            };
            assert_eq!(prompt_id, "subagent-completed-sa-a");
            assert!(
                body.contains("sa-a output") && !body.contains("sa-b"),
                "{body}"
            );
            let late = wake_prompts(harness.finish("sa-b").await);
            let [(prompt_id, body)] = late.as_slice() else {
                panic!("sa-b wakes on its own, got {late:?}");
            };
            assert_eq!(prompt_id, "subagent-completed-sa-b");
            assert!(
                body.contains("sa-b output") && !body.contains("sa-a"),
                "{body}"
            );
        })
        .await;
}

/// Stop means stop: a held wake must not fire once the cancelled siblings wind down.
#[tokio::test(flavor = "current_thread")]
async fn stop_drops_held_wakes_without_waking() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut harness = GatedWakeHarness::start();
            harness.spawn_background("sa-a").await;
            harness.spawn_background("sa-b").await;
            let early = wake_prompts(harness.finish("sa-a").await);
            assert!(
                early.is_empty(),
                "sa-b still runs, so sa-a must not wake yet: {early:?}"
            );
            assert_eq!(
                harness.backend.cancel_parent_session().await,
                SubagentCancelOutcome::Cancelled
            );
            let after_stop = wake_prompts(harness.commands_through_finish("sa-b").await);
            assert!(after_stop.is_empty(), "{after_stop:?}");
        })
        .await;
}

/// No siblings: the lone child's wake is sent at once, in the single-child shape.
#[tokio::test(flavor = "current_thread")]
async fn lone_background_child_still_wakes_on_its_own() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut harness = GatedWakeHarness::start();
            harness.spawn_background("sa-a").await;
            let wakes = wake_prompts(harness.finish("sa-a").await);
            let [(prompt_id, body)] = wakes.as_slice() else {
                panic!("one wake for the lone child, got {wakes:?}");
            };
            assert_eq!(prompt_id, "subagent-completed-sa-a");
            assert!(body.contains("Background subagent \"sa-a\""), "{body}");
            assert!(body.contains("sa-a output"), "{body}");
            assert!(!body.contains("background subagents completed:"), "{body}");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn ultracode_nested_shell_inherits_distinct_shared_cwd_without_reparenting_root_state() {
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("root");
            let ordinary = temp.path().join("ordinary-parent");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&ordinary).unwrap();
            std::fs::write(root.join("marker"), b"root").unwrap();
            std::fs::write(ordinary.join("marker"), b"ordinary").unwrap();
            let meta_dir = temp.path().join("meta");
            let server = distill_test_support::MockInferenceServer::start()
                .await
                .unwrap();
            server.set_response("bounded descendant completed");
            let (context_tx, mut context_rx) = mpsc::unbounded_channel();
            let mut harness =
                RunShellChildHarnessConfig::new(meta_dir.clone(), InitialAttemptBehavior::Normal);
            harness.child_context_tx = Some(context_tx);
            let mut ctx = ctx_with_toggle(HashMap::new());
            configure_completion_harness(&mut ctx, &server, harness);
            ctx.parent_cwd = root.clone();
            ctx.parent_session_info = Some(SessionInfo {
                id: acp::SessionId::new("setup-parent"),
                cwd: root.to_string_lossy().into_owned(),
            });
            ctx.fs = Arc::new(distill_workspace::file_system::LocalFs::new(root.clone()));
            let root_fs = ctx.fs.clone();
            let enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let policy = UltracodePolicy {
                enabled: enabled.clone(),
                max_depth: 2,
                off_max_depth: 3,
                capability_ceiling: None,
                allowed_subagent_types: None,
            };
            ctx.parent_ultracode_policy = Some(policy.clone());
            ctx.subagents_max_depth = 3;
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
            let id = uuid::Uuid::now_v7().to_string();
            let mut request = auto_wake_test_request(&id);
            request.run_in_background = false;
            request.prompt = "Verify the bounded descendant objective".into();
            request.runtime_overrides.ultracode = Some(policy);
            request.runtime_overrides.model_override_provenance = ModelOverrideProvenance::Tool;
            request.runtime_overrides.spawn_depth = Some(2);
            request.runtime_overrides.inherited_cwd = Some(ordinary.to_string_lossy().into_owned());
            assert!(request.cwd.is_none());
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                backend.spawn(request, None),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(result.success, "{:?}", result.error);
            let (child, has_task, resources) = context_rx.recv().await.unwrap();
            assert!(
                has_task,
                "existing depth-two shared child needs installed Task for ordinary off depth three"
            );
            {
                let resources = resources.lock().await;
                assert_eq!(resources.get::<MaxSubagentDepth>().unwrap().0, 3);
                assert_eq!(
                    distill_tools::implementations::distill::task::effective_max_subagent_depth(
                        &resources
                    ),
                    2
                );
            }
            enabled.store(false, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(
                distill_tools::implementations::distill::task::effective_max_subagent_depth(
                    &*resources.lock().await
                ),
                3
            );
            enabled.store(true, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(
                distill_tools::implementations::distill::task::effective_max_subagent_depth(
                    &*resources.lock().await
                ),
                2
            );
            assert_eq!(child.cwd.as_path(), ordinary.as_path());
            assert_eq!(child.fs.root(), ordinary.as_path());
            assert_eq!(root_fs.root(), root.as_path());
            assert_eq!(child.fs.read_file("marker").await.unwrap(), b"ordinary");
            child
                .fs
                .write_file("descendant.txt", b"verified")
                .await
                .unwrap();
            assert!(ordinary.join("descendant.txt").exists());
            assert!(!root.join("descendant.txt").exists());
            let meta: SubagentMeta =
                serde_json::from_slice(&std::fs::read(meta_dir.join("meta.json")).unwrap())
                    .unwrap();
            assert_eq!(meta.parent_session_id, "setup-parent");
            assert_eq!(meta.child_cwd.as_deref(), ordinary.to_str());
            assert!(
                meta.worktree_path.is_none(),
                "shared-cwd descendant does not own an isolated checkout"
            );
            assert!(ordinary.is_dir());
            drop(backend);
            coordinator.await.unwrap();
            usage_ack.abort();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn ultracode_durable_isolated_resume_is_a_leaf_with_mode_on_or_off() {
    // Re-exec before the cached home is read, using the existing isolated-home pattern.
    const CHILD: &str = "DISTILL_ULTRACODE_DURABLE_RESUME_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!(
                    "{module}::ultracode_durable_isolated_resume_is_a_leaf_with_mode_on_or_off"
                ),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("DISTILL_HOME", home.path())
            .env("GROK_HOME", home.path())
            // Match the 16 MiB stack used by the neighboring resume tests.
            .env("RUST_MIN_STACK", (16 * 1024 * 1024).to_string())
            .output()
            .expect("isolated durable UltraCode test process");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child filter must run one test"
        );
        return;
    }

    distill_test_utils::require_git!();
    use crate::session::storage::StorageAdapter;
    use distill_test_utils::git::seed_repo_with_remote;
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::{
        CoordinatorConfig, SubagentCoordinator,
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let temp = tempfile::tempdir().unwrap();
            let (root, _remote) = seed_repo_with_remote(temp.path());
            let isolated = temp.path().join("isolated-source");
            distill_fast_worktree::WorktreeBuilder::new(&root, &isolated)
                .create()
                .unwrap();
            let source_id = uuid::Uuid::now_v7().to_string();
            let root_info = SessionInfo {
                id: acp::SessionId::new("setup-parent"),
                cwd: root.to_string_lossy().into_owned(),
            };
            let source_info = SessionInfo {
                id: acp::SessionId::new(source_id.clone()),
                cwd: isolated.to_string_lossy().into_owned(),
            };
            let shared = temp.path().join("saved-shared-source");
            std::fs::create_dir_all(&shared).unwrap();
            std::fs::write(root.join("marker"), b"root").unwrap();
            std::fs::write(shared.join("marker"), b"saved-shared").unwrap();
            let shared_source_info = SessionInfo {
                id: acp::SessionId::new(uuid::Uuid::now_v7().to_string()),
                cwd: shared.to_string_lossy().into_owned(),
            };
            let storage = crate::session::storage::jsonl::JsonlStorageAdapter::with_root(
                crate::util::distill_home::distill_home(),
            );
            for source in [&source_info, &shared_source_info] {
                let source_meta_dir = crate::session::persistence::session_dir(&root_info)
                    .join("subagents")
                    .join(source.id.to_string());
                let mut source_meta = prior_wake_meta(&source.id.to_string(), "test-model");
                source_meta.child_cwd = Some(source.cwd.clone());
                source_meta.worktree_path =
                    (source.id == source_info.id).then(|| source.cwd.clone());
                source_meta.snapshot_ref = None;
                assert!(write_subagent_meta(&source_meta_dir, &source_meta));
                storage
                    .init_session(source, acp::ModelId::new("test-model"))
                    .await
                    .unwrap();
                storage
                    .append_chat_message(source, &ConversationItem::system("prior source system"))
                    .await
                    .unwrap();
                storage
                    .append_chat_message(source, &ConversationItem::assistant("prior source work"))
                    .await
                    .unwrap();
            }
            let server = distill_test_support::MockInferenceServer::start()
                .await
                .unwrap();
            server.set_response("isolated leaf completed");
            let enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
            for (mode, resumed_source) in [
                (true, &source_info),
                (false, &source_info),
                (true, &shared_source_info),
            ] {
                enabled.store(mode, std::sync::atomic::Ordering::Relaxed);
                let policy = UltracodePolicy {
                    enabled: enabled.clone(),
                    max_depth: 2,
                    off_max_depth: 3,
                    capability_ceiling: None,
                    allowed_subagent_types: None,
                };
                let (context_tx, mut context_rx) = mpsc::unbounded_channel();
                let mut harness = RunShellChildHarnessConfig::new(
                    temp.path().join(if mode { "on-meta" } else { "off-meta" }),
                    InitialAttemptBehavior::Normal,
                );
                harness.child_context_tx = Some(context_tx);
                let mut ctx = ctx_with_toggle(HashMap::new());
                configure_completion_harness(&mut ctx, &server, harness);
                ctx.parent_cwd = root.clone();
                ctx.parent_session_info = Some(root_info.clone());
                ctx.parent_ultracode_policy = Some(policy.clone());
                let mut config = crate::agent::config::Config::default();
                config.feature_values.insert(
                    crate::agent::config::Feature::SubagentWorktreeSnapshot,
                    false,
                );
                ctx.agent_config = Some(config);
                ctx.subagents_max_depth = 3;
                ctx.fs = Arc::new(distill_workspace::file_system::LocalFs::new(root.clone()));
                let root_fs = ctx.fs.clone();
                let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
                ctx.parent_cmd_tx = Some(parent_cmd_tx);
                let usage_ack = tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));
                let (gateway, _gateway_rx) = test_gateway_with_receiver();
                // A new coordinator has no completed source: this must restore from disk.
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
                assert_eq!(backend.registry_counts().await.completed, 0);
                let mut request = auto_wake_test_request(&uuid::Uuid::now_v7().to_string());
                request.prompt = "Continue in this isolated checkout locally".into();
                request.resume_from = Some(resumed_source.id.to_string());
                request.runtime_overrides.ultracode = mode.then_some(policy);
                request.runtime_overrides.model_override_provenance = ModelOverrideProvenance::Tool;
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    backend.spawn(request, None),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(result.success, "{:?}", result.error);
                let (child, has_task, resources) = context_rx.recv().await.unwrap();
                assert_eq!(
                    child.cwd.as_path(),
                    std::path::Path::new(&resumed_source.cwd)
                );
                assert_eq!(child.fs.root(), child.cwd.as_path());
                assert_eq!(root_fs.root(), root.as_path());
                if resumed_source.id == source_info.id {
                    assert_eq!(
                        resources.lock().await.get::<MaxSubagentDepth>().unwrap().0,
                        child.subagent_depth
                    );
                    assert!(
                        !has_task,
                        "restored isolated children remain leaves at ordinary depth 3"
                    );
                    if mode {
                        let leaf = child.ultracode_policy.as_ref().unwrap();
                        assert_eq!(leaf.max_depth, child.subagent_depth);
                        assert_eq!(leaf.off_max_depth, child.subagent_depth);
                        enabled.store(false, std::sync::atomic::Ordering::Relaxed);
                        assert!(!leaf.is_enabled());
                        enabled.store(true, std::sync::atomic::Ordering::Relaxed);
                        assert_eq!(leaf.max_depth, child.subagent_depth);
                    }
                } else {
                    assert!(has_task, "restored shared cwd retains ordinary hierarchy");
                    assert_eq!(child.fs.read_file("marker").await.unwrap(), b"saved-shared");
                    child
                        .fs
                        .write_file("resume-proof.txt", b"verified")
                        .await
                        .unwrap();
                    assert!(shared.join("resume-proof.txt").is_file());
                    assert!(!root.join("resume-proof.txt").exists());
                }
                drop(backend);
                coordinator.await.unwrap();
                usage_ack.abort();
            }
        })
        .await;
}
