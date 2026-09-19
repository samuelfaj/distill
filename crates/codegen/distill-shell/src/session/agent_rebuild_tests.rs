// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

use distill_tools::implementations::distill::task::backend::SubagentBackend;
use distill_tools::implementations::distill::task::coordinator::{
    ChildCompletion, ChildControl, ChildRunOutput, ChildRunRequest, ChildRunner, SendBoxFuture,
    SubagentCoordinator, SubagentProgress,
};
use distill_tools::implementations::distill::task::types::{
    AgentMessageSenderResource, SubagentDescribeOutcome, SubagentOwner, SubagentRequest,
    SubagentValidateTypeOutcome,
};
use distill_tools::types::tool::ToolKind;

struct SenderProbeRunner(tokio::sync::mpsc::UnboundedSender<Option<AgentMessageSender>>);
struct SenderProbeControl;

impl ChildControl for SenderProbeControl {
    type ProgressFuture = std::future::Ready<SubagentProgress>;
    fn progress(&self) -> Self::ProgressFuture {
        std::future::ready(Default::default())
    }
    fn cancel(&self) {}
}

impl ChildRunner for SenderProbeRunner {
    type Control = SenderProbeControl;
    type RootControl = distill_tools::implementations::distill::task::root_control::NoRootControl;
    type CompletionData = ();
    type RunFuture = SendBoxFuture<ChildRunOutput<()>>;
    type ValidateFuture = SendBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = SendBoxFuture<SubagentDescribeOutcome>;

    fn run(&self, run: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        let sender = self.0.clone();
        Box::pin(async move {
            sender.send(run.agent_message_sender).unwrap();
            std::future::pending().await
        })
    }
    fn validate_type(&self, _: String, _: String) -> Self::ValidateFuture {
        Box::pin(std::future::pending())
    }
    fn describe_type(&self, _: String, _: Option<String>, _: String) -> Self::DescribeFuture {
        Box::pin(std::future::pending())
    }
    fn supports_wake(&self) -> bool {
        true
    }
    fn supports_agent_message_sender(&self) -> bool {
        true
    }
    fn on_completed(&self, _: ChildCompletion<()>, terminal_published: Box<dyn FnOnce() + Send>) {
        terminal_published();
    }
}

async fn mint_test_sender(owner: SubagentOwner) -> Option<AgentMessageSender> {
    let (coordinator_sender, receiver) = SubagentCoordinator::<SenderProbeRunner>::channel();
    let (sender_tx, mut sender_rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = tokio::spawn(
        SubagentCoordinator::from_channel(
            receiver,
            SenderProbeRunner(sender_tx),
            Default::default(),
        )
        .run(),
    );
    let backend =
        distill_tools::implementations::distill::task::backend::ChannelBackend::from_coordinator(
            coordinator_sender,
        );
    let id = uuid::Uuid::now_v7().to_string();
    let spawn = tokio::spawn(async move {
        backend
            .spawn(
                SubagentRequest {
                    id,
                    prompt: "work".to_owned(),
                    description: "child".to_owned(),
                    subagent_type: "general-purpose".to_owned(),
                    parent_session_id: "root".to_owned(),
                    parent_prompt_id: None,
                    resume_from: None,
                    cwd: None,
                    runtime_overrides: Default::default(),
                    run_in_background: true,
                    surface_completion: false,
                    await_to_completion: false,
                    fork_context: false,
                    owner,
                    cancel_token: tokio_util::sync::CancellationToken::new(),
                    spawn_root: Default::default(),
                },
                None,
            )
            .await
    });
    let sender = sender_rx.recv().await.unwrap();
    actor.abort();
    spawn.abort();
    sender
}

#[tokio::test(flavor = "current_thread")]
async fn child_rebuild_rejects_coordinator_authority() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (coordinator_sender, _receiver) =
                SubagentCoordinator::<SenderProbeRunner>::channel();
            let mut spec = test_rebuild_spec_default();
            let spec_fields = Arc::get_mut(&mut spec).unwrap();
            spec_fields.prompt_audience = PromptAudience::Subagent;
            spec_fields.subagent_coordinator_sender = Some(coordinator_sender);

            let result = spec
                .build_agent(
                    AgentDefinition::default_distill(),
                    distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                )
                .await;
            assert!(matches!(result, Err(AgentBuildError::InvalidConfig(_))));
        })
        .await;
}

async fn child_agent(
    definition: AgentDefinition,
    sender: Option<AgentMessageSender>,
    active_agent_messages_enabled: bool,
) -> Agent {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut spec = test_rebuild_spec_default();
    let fields = Arc::get_mut(&mut spec).unwrap();
    fields.prompt_audience = PromptAudience::Subagent;
    fields.subagent_depth = 1;
    fields.subagent_event_tx = Some(event_tx);
    fields.agent_message_sender = sender;
    fields.active_agent_messages_enabled = active_agent_messages_enabled;
    spec.build_agent(definition, distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL)
        .await
        .unwrap()
}

async fn assert_no_child_messaging(agent: &Agent) {
    let bridge = agent.tool_bridge();
    assert!(
        bridge
            .read_resource::<AgentMessageSenderResource>()
            .await
            .is_none()
    );
    assert!(
        bridge
            .tool_for_kind(ToolKind::ActiveAgentMessage)
            .await
            .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn child_rebuild_message_authority_respects_grant_workflow_and_capability_ceiling() {
    use distill_tool_types::SubagentCapabilityMode;

    tokio::task::LocalSet::new()
        .run_until(async {
            let sender = mint_test_sender(SubagentOwner::Task)
                .await
                .expect("non-workflow child sender");
            let mut definition = AgentDefinition::default_distill();
            definition.capability_mode = Some(SubagentCapabilityMode::All);
            let agent = child_agent(definition.clone(), Some(sender.clone()), true).await;
            let bridge = agent.tool_bridge();
            assert!(bridge.read_resource::<AgentMessageSenderResource>().await.is_some());
            let backend = bridge
                .read_resource::<
                    distill_tools::implementations::distill::task::backend::SubagentBackendResource,
                >()
                .await
                .unwrap();
            assert_eq!(
                backend
                    .backend()
                    .send_active_message(
                        distill_tools::implementations::distill::task::types::ActiveAgentMessageRequest::try_new(
                            "child",
                            "follow up",
                        )
                        .unwrap(),
                    )
                    .await,
                distill_tools::implementations::distill::task::types::ActiveAgentMessageOutcome::Unsupported,
            );
            assert!(bridge.tool_for_kind(ToolKind::ActiveAgentMessage).await.is_some());

            let ungranted = child_agent(definition.clone(), Some(sender.clone()), false).await;
            assert_no_child_messaging(&ungranted).await;
            let workflow_sender = mint_test_sender(SubagentOwner::workflow("workflow-run")).await;
            assert!(workflow_sender.is_none());
            assert_no_child_messaging(&child_agent(definition.clone(), workflow_sender, true).await)
                .await;
            // The production spawn policy filters the definition; the rebuild must not add messaging back.
            definition.capability_mode = Some(SubagentCapabilityMode::ReadOnly);
            distill_subagent_resolution::apply_child_tool_policy(
                &mut definition,
                Some(SubagentCapabilityMode::ReadOnly),
                true,
            );
            assert_no_child_messaging(&child_agent(definition, Some(sender), true).await).await;
        })
        .await;
}
